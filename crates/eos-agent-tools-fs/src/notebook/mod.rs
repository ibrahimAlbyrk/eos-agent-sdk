//! The NotebookEdit tool: replace, insert or delete one notebook cell.

mod json;

use std::hash::{BuildHasher, Hasher};

use eos_agent_core::{BoxFuture, FileStates, Tool, ToolContext, ToolOutput, ToolSpec, content_hash};
use serde::Deserialize;
use serde_json::value::RawValue;

use self::json::Json;
use crate::common::{Fail, Res, blocking, finish, format_bytes, mtime_ms, parse_input, resolve_write, workspace};
use crate::edit::{MODIFIED_SINCE_READ, NOT_READ, store};
use crate::read::{MAX_NOTEBOOK_FILE, NOT_A_NOTEBOOK, too_big_message};
use crate::spec::tool_spec;
use crate::state;
use crate::text;

/// Claude Code's `NotebookEdit`.
#[derive(Debug)]
pub struct NotebookEditTool {
    spec: ToolSpec,
}

impl NotebookEditTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("NotebookEdit") }
    }
}

impl Default for NotebookEditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for NotebookEditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { finish(run(input, &ctx).await) })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    notebook_path: String,
    cell_id: Option<String>,
    new_source: String,
    cell_type: Option<String>,
    #[serde(default = "replace")]
    edit_mode: Mode,
}

fn replace() -> Mode {
    Mode::Replace
}

fn is_notebook(nb: &Json) -> bool {
    match nb.get("cells") {
        Some(Json::Arr(cells)) => cells.iter().all(|c| match c.get("source") {
            Some(Json::Str(_)) => true,
            Some(Json::Arr(parts)) => parts.iter().all(|p| matches!(p, Json::Str(_))),
            _ => false,
        }),
        _ => false,
    }
}

/// `cell-N` as Read labels cells without an id.
fn index_label(id: &str) -> Option<usize> {
    id.strip_prefix("cell-").filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?.parse().ok()
}

fn random_id() -> String {
    let n = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:08x}", n & 0xffff_ffff)
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input: Input = parse_input(raw)?;
    let ws = workspace(ctx)?;
    let path = resolve_write(&ws, &input.notebook_path)?;
    if path.extension().is_none_or(|e| e != "ipynb") {
        return Err(Fail(
            "File must be a Jupyter notebook (.ipynb file). For editing other file types, use the FileEdit tool."
                .into(),
        ));
    }
    if input.edit_mode == Mode::Insert && input.cell_type.is_none() {
        return Err(Fail("Cell type is required when using edit_mode=insert.".into()));
    }
    if input.cell_type.as_deref().is_some_and(|t| t != "code" && t != "markdown") {
        return Err(Fail("InputValidationError: cell_type must be \"code\" or \"markdown\"".into()));
    }
    let Some(stamp) = ctx.extensions.get_or_default::<FileStates>().get(&path) else {
        return Err(Fail(NOT_READ.into()));
    };
    let p = path.clone();
    let loaded = blocking(move || {
        Ok(std::fs::metadata(&p).and_then(|meta| {
            if meta.len() > MAX_NOTEBOOK_FILE { Ok(None) } else { std::fs::read(&p).map(|b| Some((b, meta))) }
        }))
    })
    .await?;
    let (bytes, meta) = match loaded {
        Ok(Some(found)) => found,
        Ok(None) => return Err(Fail(too_big_message())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Fail("Notebook file does not exist.".into())),
        Err(e) => return Err(Fail(e.to_string())),
    };
    if mtime_ms(&meta) > stamp.modified_ms && content_hash(&bytes) != stamp.hash {
        return Err(Fail(MODIFIED_SINCE_READ.into()));
    }
    let decoded = text::decode(&bytes);
    if let Some(reason) = decoded.not_text {
        return Err(Fail(reason.refusal()));
    }
    let mut nb: Json = serde_json::from_str(text::normalize(&decoded.text).as_str())
        .map_err(|_| Fail("Notebook is not valid JSON.".into()))?;
    if !is_notebook(&nb) {
        return Err(Fail(NOT_A_NOTEBOOK.into()));
    }
    let summary = edit(&mut nb, &input)?;
    let mut serialized = nb.to_pretty();
    drop(nb);
    if text::is_crlf(&decoded.text) {
        serialized = text::to_crlf(&serialized);
    }
    if decoded.text.starts_with(text::BOM) {
        serialized.insert(0, text::BOM);
    }
    let out = text::encode(&serialized, decoded.encoding);
    if out.len() as u64 > MAX_NOTEBOOK_FILE {
        return Err(Fail(format!(
            "Edit rejected: the updated notebook would exceed the maximum size this tool can read ({}). The file was left unchanged.",
            format_bytes(MAX_NOTEBOOK_FILE)
        )));
    }
    let hash = content_hash(&out);
    let meta = store(&path, out, false).await?;
    state::record(ctx, &path, state::stamp(&meta, hash, false), false);
    Ok(ToolOutput::text(summary))
}

/// Applies the edit; returns the result text.
fn edit(nb: &mut Json, input: &Input) -> Res<String> {
    let has_ids = match (nb.get("nbformat").and_then(Json::as_u64), nb.get("nbformat_minor").and_then(Json::as_u64)) {
        (Some(major), minor) => major > 4 || (major == 4 && minor.unwrap_or(0) >= 5),
        _ => false,
    };
    let Some(Json::Arr(cells)) = nb_cells(nb) else {
        return Err(Fail(NOT_A_NOTEBOOK.into()));
    };
    let mut index = match &input.cell_id {
        None if input.edit_mode != Mode::Insert => {
            return Err(Fail("Cell ID must be specified when not inserting a new cell.".into()));
        }
        None => 0,
        Some(id) => match cells.iter().position(|c| c.get("id").and_then(Json::as_str) == Some(id)) {
            Some(i) => i,
            None => match index_label(id) {
                Some(i) if i < cells.len() => i,
                Some(i) => return Err(Fail(format!("Cell with index {i} does not exist in notebook."))),
                None => return Err(Fail(format!("Cell with ID \"{id}\" not found in notebook."))),
            },
        },
    };
    if input.edit_mode == Mode::Insert && input.cell_id.is_some() {
        index += 1;
    }
    let mut mode = input.edit_mode;
    let mut cell_type = input.cell_type.clone();
    if mode == Mode::Replace && index == cells.len() {
        mode = Mode::Insert;
        cell_type.get_or_insert_with(|| "code".into());
    }
    let source = &input.new_source;
    let label = input.cell_id.clone().unwrap_or_else(|| format!("cell-{index}"));
    match mode {
        Mode::Delete => {
            cells.remove(index);
            Ok(format!("Deleted cell {label}"))
        }
        Mode::Insert => {
            let id = has_ids.then(random_id);
            let mut cell = vec![("cell_type".to_owned(), Json::str(cell_type.as_deref().unwrap_or("code")))];
            if let Some(id) = &id {
                cell.push(("id".into(), Json::str(id.as_str())));
            }
            cell.push(("source".into(), Json::str(source.as_str())));
            cell.push(("metadata".into(), Json::Obj(Vec::new())));
            if cell_type.as_deref() != Some("markdown") {
                cell.push(("execution_count".into(), Json::Null));
                cell.push(("outputs".into(), Json::Arr(Vec::new())));
            }
            cells.insert(index, Json::Obj(cell));
            Ok(format!("Inserted cell {} with {source}", id.unwrap_or_else(|| format!("cell-{index}"))))
        }
        Mode::Replace => {
            let cell = &mut cells[index];
            cell.set("source", Json::str(source.as_str()));
            if cell.get("cell_type").and_then(Json::as_str) == Some("code") {
                cell.set("execution_count", Json::Null);
                cell.set("outputs", Json::Arr(Vec::new()));
            }
            if let Some(t) = cell_type {
                cell.set("cell_type", Json::str(t));
            }
            Ok(format!("Updated cell {label} with {source}"))
        }
    }
}

fn nb_cells(nb: &mut Json) -> Option<&mut Json> {
    match nb {
        Json::Obj(fields) => fields.iter_mut().find(|(k, _)| k == "cells").map(|(_, v)| v),
        _ => None,
    }
}
