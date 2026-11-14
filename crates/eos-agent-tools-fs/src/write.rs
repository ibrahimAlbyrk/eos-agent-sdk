//! The Write tool: creates a file or replaces one the agent has fully read.

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, Tool, ToolContext, ToolOutput, ToolSpec, content_hash};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::common::{Fail, Res, blocking, finish, parse_input, resolve_write, with_absolute, workspace, written};
use crate::edit::{MODIFIED_SINCE_READ, NO_NEED_TO_REREAD, NOT_READ, store};
use crate::patch::{Hunk, structured_patch};
use crate::spec::tool_spec;
use crate::state::{self, Prior};
use crate::text::{self, BOM, Encoding};

const START_ONLY: &str = "You were given only the start of this file, and nothing shows that you have since read all of it in one call, so a Write could lose the rest. To change part of it, use Edit. To replace it whole, first Read it in one call with no offset or limit. If Read returns only a page or says the file is too large, Read it again with allow_large: true. If even that cannot return all of it, use Edit.";

/// Claude Code's `Write`.
#[derive(Debug)]
pub struct WriteTool {
    spec: ToolSpec,
}

impl WriteTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("Write") }
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for WriteTool {
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
    content: String,
}

enum Existing {
    Missing,
    Dir,
    Special,
    File(Vec<u8>, std::fs::Metadata),
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input: Input = parse_input(raw)?;
    let ws = workspace(ctx)?;
    let path = resolve_write(&ws, &input.file_path)?;
    let p = path.clone();
    let existing = blocking(move || match std::fs::metadata(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Existing::Missing),
        Err(e) => Err(Fail(e.to_string())),
        Ok(meta) if meta.is_dir() => Ok(Existing::Dir),
        Ok(meta) if !meta.is_file() => Ok(Existing::Special),
        Ok(meta) => std::fs::read(&p).map(|b| Existing::File(b, meta)).map_err(|e| Fail(e.to_string())),
    })
    .await?;

    let (bytes, patch) = match existing {
        Existing::Dir => {
            return Err(Fail(format!(
                "{} is a directory, not a file. To create a file inside it, include the file name in file_path.",
                input.file_path
            )));
        }
        Existing::Special => {
            return Err(Fail(format!(
                "{} exists but is not a regular file (a device, FIFO or socket). Write only creates or overwrites regular files.",
                input.file_path
            )));
        }
        Existing::Missing => (input.content.into_bytes(), None),
        Existing::File(old, meta) => {
            match state::prior(ctx, &path, &meta, &old) {
                Prior::Unread => return Err(Fail(NOT_READ.into())),
                Prior::Truncated => return Err(Fail(START_ONLY.into())),
                Prior::Read { changed: true, .. } => return Err(Fail(MODIFIED_SINCE_READ.into())),
                Prior::Read { .. } => {}
            }
            let decoded = text::decode(&old);
            if let Some(reason) = decoded.not_text {
                return Err(Fail(reason.refusal()));
            }
            let content =
                if decoded.text.starts_with(BOM) && !input.content.starts_with(BOM) && !input.content.is_empty() {
                    format!("{BOM}{}", input.content)
                } else {
                    input.content
                };
            let patch = structured_patch(&decoded.text, &content);
            let bytes = match decoded.encoding {
                Encoding::Utf8 => content.into_bytes(),
                Encoding::Utf16Le => text::encode(&content, Encoding::Utf16Le),
            };
            (bytes, Some(patch))
        }
    };
    let created = patch.is_none();
    let hash = content_hash(&bytes);
    let meta = store(&path, bytes, created).await?;
    state::record(ctx, &path, state::stamp(&meta, hash, false), false);
    let details = WriteDetails {
        kind: if created { "create" } else { "update" },
        file_path: &input.file_path,
        structured_patch: patch.unwrap_or_default(),
    };
    Ok(ToolOutput::text(if created {
        format!("File created successfully at: {}{NO_NEED_TO_REREAD}", input.file_path)
    } else {
        format!("The file {} has been updated successfully.{NO_NEED_TO_REREAD}", input.file_path)
    })
    .with_details(&details))
}

/// What Claude Code attaches to a Write result (without the file's texts;
/// a new file has no hunks).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WriteDetails<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    file_path: &'a str,
    structured_patch: Vec<Hunk>,
}
