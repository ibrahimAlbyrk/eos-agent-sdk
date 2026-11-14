//! The Edit tool: one string replacement, guarded by what the agent has read.

mod matching;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, Tool, ToolContext, ToolOutput, ToolSpec, content_hash};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::common::{
    Fail, Res, blocking, finish, format_bytes, missing_file_message, parse_input, resolve_write, with_absolute,
    workspace, written,
};
use crate::fsio::write_atomic;
use crate::patch::{Hunk, structured_patch};
use crate::spec::tool_spec;
use crate::state::{self, Prior};
use crate::text::{self, BOM, Encoding};

pub(crate) const NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
pub(crate) const MODIFIED_SINCE_READ: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";
pub(crate) const NO_NEED_TO_REREAD: &str = " (file state is current in your context \u{2014} no need to Read it back)";
const STALE_APPLIED: &str = " (note: the file had been modified on disk since you last read it \u{2014} the edit applied cleanly, but the file contains other changes not in your context. Read it before edits that depend on surrounding content.)";
const MAX_EDIT_BYTES: u64 = 1 << 30;

/// Claude Code's `Edit`.
#[derive(Debug)]
pub struct EditTool {
    spec: ToolSpec,
}

impl EditTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("Edit") }
    }
}

impl Default for EditTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for EditTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        with_absolute(input, extensions, "file_path")
    }

    fn written_paths(&self, input: &RawValue, extensions: &Extensions) -> Vec<PathBuf> {
        written(input, extensions, "file_path")
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { finish(run(input, &ctx).await) })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

/// The file as it is on disk now: `None` when it does not exist.
type Current = Option<(Vec<u8>, std::fs::Metadata)>;

async fn load(path: &Path) -> Res<Current> {
    let p = path.to_path_buf();
    blocking(move || match std::fs::metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Fail(e.to_string())),
        Ok(meta) if meta.len() > MAX_EDIT_BYTES => Err(Fail(format!(
            "File is too large to edit ({}). Maximum editable file size is {}.",
            format_bytes(meta.len()),
            format_bytes(MAX_EDIT_BYTES)
        ))),
        Ok(meta) => std::fs::read(&p).map(|b| Some((b, meta))).map_err(|e| Fail(e.to_string())),
    })
    .await
}

/// Writes `bytes`, creating parent directories when asked; returns the new metadata.
pub(crate) async fn store(path: &Path, bytes: Vec<u8>, create_parents: bool) -> Res<std::fs::Metadata> {
    let p = path.to_path_buf();
    blocking(move || {
        if create_parents && let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Fail(e.to_string()))?;
        }
        write_atomic(&p, &bytes).map_err(|e| Fail(format!("Failed to write {}: {e}", p.display())))
    })
    .await
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input: Input = parse_input(raw)?;
    let ws = workspace(ctx)?;
    let path = resolve_write(&ws, &input.file_path)?;
    let (old, new) = (input.old_string.as_str(), input.new_string.as_str());
    if old == new {
        return Err(Fail("No changes to make: old_string and new_string are exactly the same.".into()));
    }
    let Some((bytes, meta)) = load(&path).await? else {
        if !old.is_empty() {
            return Err(Fail(missing_file_message(&ws, &path)));
        }
        let meta = store(&path, new.as_bytes().to_vec(), true).await?;
        state::record(ctx, &path, state::stamp(&meta, content_hash(new.as_bytes()), false), false);
        return Ok(ToolOutput::text(format!(
            "The file {} has been updated successfully.{NO_NEED_TO_REREAD}",
            input.file_path
        )));
    };

    let decoded = text::decode(&bytes);
    let content = text::normalize(&decoded.text);
    if old.is_empty() && !content.trim().is_empty() {
        return Err(Fail("Cannot create new file - file already exists.".into()));
    }
    if path.extension().is_some_and(|e| e == "ipynb") {
        return Err(Fail("File is a Jupyter Notebook. Use the NotebookEdit to edit this file.".into()));
    }
    let actual = if old.is_empty() { Some(String::new()) } else { matching::find_actual(&content, old) };
    let occurrences = actual.as_deref().filter(|a| !a.is_empty()).map_or(1, |a| content.matches(a).count());
    let (complete, stale) = match state::prior(ctx, &path, &meta, &bytes) {
        Prior::Unread | Prior::Truncated => return Err(Fail(NOT_READ.into())),
        Prior::Read { complete, changed } => (complete, changed),
    };
    let applies = !old.is_empty() && actual.is_some() && (occurrences == 1 || input.replace_all);
    if stale && !applies {
        return Err(Fail(MODIFIED_SINCE_READ.into()));
    }
    if let Some(reason) = decoded.not_text {
        return Err(Fail(reason.refusal()));
    }
    let Some(actual) = actual else {
        let note = if old.contains("\\u") || !old.is_ascii() {
            "\n(note: Edit also tried swapping \\uXXXX escapes and their characters; neither form matched, so the mismatch is likely elsewhere in old_string. Re-read the file and copy the exact surrounding text.)"
        } else {
            ""
        };
        return Err(Fail(format!("String to replace not found in file.\nString: {old}{note}")));
    };
    if occurrences > 1 && !input.replace_all {
        return Err(Fail(format!(
            "Found {occurrences} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: {old}"
        )));
    }

    let new = matching::adapt_new(old, &actual, new);
    let applied = (!old.is_empty()).then(|| matching::apply(&content, &actual, &new, input.replace_all));
    let updated: &str = applied.as_deref().unwrap_or(&new);
    if updated == content {
        return Err(Fail("String not found in file. Failed to apply edit.".into()));
    }
    let details = EditDetails {
        file_path: &input.file_path,
        old_string: &actual,
        new_string: &new,
        replace_all: input.replace_all,
        structured_patch: structured_patch(&content, updated),
    };
    let out = encode_like(&decoded.text, decoded.encoding, updated);
    let hash = content_hash(&out);
    let meta = store(&path, out, false).await?;
    let in_context = complete && !stale;
    state::record(ctx, &path, state::stamp(&meta, hash, !in_context), false);

    let note = if stale {
        STALE_APPLIED
    } else if in_context {
        NO_NEED_TO_REREAD
    } else {
        ""
    };
    Ok(ToolOutput::text(if input.replace_all {
        format!("The file {} has been updated. All occurrences were successfully replaced.{note}", input.file_path)
    } else {
        format!("The file {} has been updated successfully.{note}", input.file_path)
    })
    .with_details(&details))
}

/// What Claude Code attaches to an Edit result (without the file's old text).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EditDetails<'a> {
    file_path: &'a str,
    old_string: &'a str,
    new_string: &'a str,
    replace_all: bool,
    structured_patch: Vec<Hunk>,
}

/// `text` in the original file's encoding, BOM and line endings.
fn encode_like(original: &str, encoding: Encoding, text: &str) -> Vec<u8> {
    let body = if text::is_crlf(original) { text::to_crlf(text) } else { text.to_owned() };
    let body = if original.starts_with(BOM) { format!("{BOM}{body}") } else { body };
    text::encode(&body, encoding)
}
