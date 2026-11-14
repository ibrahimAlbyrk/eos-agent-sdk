//! Jupyter notebooks as Read shows them: one `<cell id="…">` block per cell,
//! then each output as text or an image.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use eos_agent_core::{ToolContext, ToolOutput, Workspace, content_hash};
use eos_agent_types::ContentBlock;
use serde_json::Value;

use super::limits::{self, ReadLimits};
use super::media::{encoded_block, image_type, text_block};
use crate::common::{Fail, Res, blocking, format_bytes, missing_file_message};
use crate::state;

pub(crate) const MAX_NOTEBOOK_FILE: u64 = 100 * 1024 * 1024;
const MAX_OUTPUT_CHARS: usize = 30_000;
const MAX_OUTPUTS_PER_CELL: usize = 10_000;

pub(crate) fn jq_hint() -> &'static str {
    "Use Bash with jq to read specific portions:\n  cat <notebook_path> | jq '.cells[:20]' # First 20 cells\n  cat <notebook_path> | jq '.cells[100:120]' # Cells 100-120\n  cat <notebook_path> | jq '.cells | length' # Count total cells\n  cat <notebook_path> | jq '.cells[] | select(.cell_type==\"code\") | .source' # All code sources"
}

pub(crate) fn too_big_message() -> String {
    format!(
        "Notebook file exceeds the maximum size this tool can read ({}). {}",
        format_bytes(MAX_NOTEBOOK_FILE),
        jq_hint()
    )
}

pub(crate) const NOT_A_NOTEBOOK: &str = "Notebook file is not a valid Jupyter notebook (top-level \"cells\" must be an array of cell objects, each with a string or string-array \"source\").";

/// `cells` is an array of objects whose `source` is a string or strings.
pub(crate) fn is_notebook(nb: &Value) -> bool {
    nb["cells"].as_array().is_some_and(|cells| {
        cells.iter().all(|c| match &c["source"] {
            Value::String(_) => true,
            Value::Array(parts) => parts.iter().all(Value::is_string),
            _ => false,
        })
    })
}

/// A `source` (or output text) that may be split into lines.
pub(crate) fn joined(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

pub(super) async fn read_notebook(
    ctx: &ToolContext<'_>,
    ws: &Workspace,
    path: PathBuf,
    limits: ReadLimits,
) -> Res<ToolOutput> {
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Fail(missing_file_message(ws, &path))),
        Err(e) => return Err(Fail(e.to_string())),
    };
    let nb: Value = serde_json::from_slice(&bytes).map_err(|e| {
        Fail(format!("Notebook file is not valid JSON (it may be truncated, corrupted, or still being written): {e}"))
    })?;
    if !is_notebook(&nb) {
        return Err(Fail(NOT_A_NOTEBOOK.into()));
    }
    let blocks = render(&nb);
    let size: usize = blocks.iter().map(block_len).sum();
    if size as u64 > limits.max_size_bytes {
        return Err(Fail(format!(
            "Notebook content ({}) exceeds maximum allowed size ({}). {}",
            format_bytes(size as u64),
            format_bytes(limits.max_size_bytes),
            jq_hint()
        )));
    }
    let tokens = limits::estimate_tokens(size, "ipynb");
    if tokens > limits.max_tokens {
        return Err(Fail(limits::too_many_tokens(tokens, limits.max_tokens, "")));
    }
    state::record(ctx, &path, state::stamp(&meta, content_hash(&bytes), false), false);
    Ok(ToolOutput::blocks(blocks))
}

fn block_len(b: &ContentBlock) -> usize {
    match b {
        ContentBlock::Text { text, .. } => text.len(),
        ContentBlock::Unknown(raw) => raw.get().len(),
        _ => 0,
    }
}

fn render(nb: &Value) -> Vec<ContentBlock> {
    let language = nb["metadata"]["language_info"]["name"].as_str().unwrap_or("python");
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut push = |block: ContentBlock| match (blocks.last_mut(), block) {
        (Some(ContentBlock::Text { text: last, .. }), ContentBlock::Text { text, .. }) => {
            last.push('\n');
            last.push_str(&text);
        }
        (_, block) => blocks.push(block),
    };
    for (i, cell) in nb["cells"].as_array().into_iter().flatten().enumerate() {
        let id = cell["id"].as_str().map_or_else(|| format!("cell-{i}"), str::to_owned);
        let kind = cell["cell_type"].as_str().unwrap_or("code");
        let mut tags = String::new();
        if kind != "code" {
            tags = format!("<cell_type>{kind}</cell_type>");
        } else if language != "python" {
            tags = format!("<language>{language}</language>");
        }
        push(text_block(format!("<cell id=\"{id}\">{tags}{}</cell id=\"{id}\">", joined(&cell["source"]))));
        if kind != "code" {
            continue;
        }
        let outputs: Vec<Output> = cell["outputs"].as_array().into_iter().flatten().map(Output::from_json).collect();
        let weight: usize = outputs.iter().map(|o| o.text.len() + o.image.as_ref().map_or(0, |(_, d)| d.len())).sum();
        if weight > MAX_OUTPUTS_PER_CELL {
            push(text_block(format!(
                "\nOutputs are too large to include. Use Bash with: cat <notebook_path> | jq '.cells[{i}].outputs'"
            )));
            continue;
        }
        for out in outputs {
            if !out.text.is_empty() {
                push(text_block(format!("\n{}", out.text)));
            }
            if let Some((media_type, data)) = out.image {
                push(encoded_block("image", media_type, &data));
            }
        }
    }
    blocks
}

struct Output {
    text: String,
    image: Option<(&'static str, String)>,
}

impl Output {
    fn from_json(o: &Value) -> Self {
        let text = match o["output_type"].as_str() {
            Some("stream") => joined(&o["text"]),
            Some("execute_result" | "display_data") => joined(&o["data"]["text/plain"]),
            Some("error") => {
                let tb: Vec<&str> = o["traceback"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                format!(
                    "{}: {}\n{}",
                    o["ename"].as_str().unwrap_or(""),
                    o["evalue"].as_str().unwrap_or(""),
                    tb.join("\n")
                )
            }
            _ => String::new(),
        };
        let image = ["image/png", "image/jpeg"].into_iter().find_map(|mt| {
            let data: String = o["data"][mt].as_str()?.chars().filter(|c| !c.is_whitespace()).collect();
            let head = STANDARD.decode(data.get(..16)?).ok()?;
            image_type(&head).map(|t| (t, data))
        });
        Self { text: truncate_output(text), image }
    }
}

fn truncate_output(text: String) -> String {
    let Some((cut, _)) = text.char_indices().nth(MAX_OUTPUT_CHARS) else {
        return text;
    };
    let hidden = text[cut..].matches('\n').count() + 1;
    format!("{}\n\n... [{hidden} lines truncated] ...", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_cells_like_claude_code() {
        let nb = serde_json::json!({
            "cells": [
                {"cell_type": "markdown", "id": "m1", "source": ["# Title\n", "text"]},
                {"cell_type": "code", "source": "print(1)", "outputs": [{"output_type": "stream", "text": ["1\n"]}]}
            ],
            "metadata": {}
        });
        let blocks = render(&nb);
        let ContentBlock::Text { text, .. } = &blocks[0] else { panic!("text block") };
        assert_eq!(
            text,
            "<cell id=\"m1\"><cell_type>markdown</cell_type># Title\ntext</cell id=\"m1\">\n<cell id=\"cell-1\">print(1)</cell id=\"cell-1\">\n\n1\n"
        );
    }
}
