//! What a resumed agent had seen of its files, rebuilt from its transcript
//! the way Claude Code's `extractReadFilesFromMessages` does on resume: the
//! newest successful Read, Write or Edit of each file (up to the file-state
//! cache's size), its content as the result or the call showed it, stamped
//! with the time the result was recorded. Each file is then checked against
//! the disk: unchanged since (not modified after that time, or modified to the
//! same text) it counts as read now; changed, it counts as read before the
//! change, so writes are refused until it is read again.

use std::collections::HashSet;
use std::path::PathBuf;

use eos_agent_core::{Extensions, FileStamp, FileStates, StoredMessage, Workspace, content_hash};
use eos_agent_types::{ContentBlock, Role, ToolResultContent};
use serde::Deserialize;
use serde_json::Value;

use crate::common::{blocking, mtime_ms, resolve};
use crate::state;
use crate::text;

/// The most files one history restores (the cache's own size).
const MAX_FILES: usize = 256;
/// A Read that hit the size cap: the start of the file only.
const TRUNCATED_BANNER: &str = "[Truncated: PARTIAL view";

/// What the agent saw of one file, and when.
#[derive(Debug)]
struct Seen {
    path: PathBuf,
    /// The text it saw, where that was the whole file.
    content: Option<String>,
    at_ms: Option<u64>,
    /// Only part of the file (offset/limit).
    partial: bool,
    /// Cut by the size cap: writes need a fresh Read.
    truncated: bool,
    /// An Edit: the file on disk is what the agent last made it.
    edited: bool,
}

#[derive(Debug)]
enum Call {
    Read { path: PathBuf, partial: bool },
    Write { path: PathBuf, content: String },
    Edit { path: PathBuf },
}

pub(crate) async fn restore(history: &[StoredMessage], extensions: &Extensions) {
    let Some(ws) = extensions.get::<Workspace>() else { return };
    let seen = collect(history, &ws);
    if seen.is_empty() {
        return;
    }
    let states = extensions.get_or_default::<FileStates>();
    // Oldest first, so the newest ends up the most recently used.
    for seen in seen.into_iter().rev() {
        if states.get(&seen.path).is_some() {
            continue;
        }
        let truncated = seen.truncated;
        let path = seen.path.clone();
        if let Ok(Some(stamp)) = blocking(move || Ok(check(&seen))).await {
            state::record_in(extensions, &path, stamp, truncated);
        }
    }
}

/// The newest successful call per file, newest first.
fn collect(history: &[StoredMessage], ws: &Workspace) -> Vec<Seen> {
    let mut calls = std::collections::HashMap::new();
    for message in history.iter().filter(|m| m.role() == Role::Assistant) {
        for block in message.blocks().unwrap_or_default() {
            if let ContentBlock::ToolUse { id, name, input } = block
                && let Some(call) = call(&name, input.get(), ws)
            {
                calls.insert(id, call);
            }
        }
    }
    if calls.is_empty() {
        return Vec::new();
    }
    let mut done = HashSet::new();
    let mut out = Vec::new();
    for message in history.iter().rev().filter(|m| m.role() == Role::User) {
        let blocks = message.blocks().unwrap_or_default();
        for block in blocks.iter().rev() {
            let ContentBlock::ToolResult { tool_use_id, content, is_error } = block else { continue };
            let Some(call) = calls.get(tool_use_id) else { continue };
            if *is_error {
                continue;
            }
            let at_ms = message.recorded_ms();
            let seen = match call {
                Call::Read { path, partial } => {
                    let Some(ToolResultContent::Text(text)) = content else { continue };
                    if spilled(text) {
                        continue;
                    }
                    let truncated = text.starts_with(TRUNCATED_BANNER);
                    let partial = *partial || truncated;
                    let content = (!partial).then(|| shown_text(text));
                    Seen { path: path.clone(), content, at_ms, partial, truncated, edited: false }
                }
                Call::Write { path, content } => Seen {
                    path: path.clone(),
                    content: Some(content.clone()),
                    at_ms,
                    partial: false,
                    truncated: false,
                    edited: false,
                },
                Call::Edit { path } => {
                    Seen { path: path.clone(), content: None, at_ms, partial: false, truncated: false, edited: true }
                }
            };
            if done.insert(seen.path.clone()) {
                out.push(seen);
                if out.len() >= MAX_FILES {
                    return out;
                }
            }
        }
    }
    out
}

fn call(name: &str, input: &str, ws: &Workspace) -> Option<Call> {
    #[derive(Deserialize)]
    struct Input {
        file_path: String,
        #[serde(default)]
        offset: Value,
        #[serde(default)]
        limit: Value,
        #[serde(default)]
        content: Option<String>,
    }
    let input: Input = serde_json::from_str(input).ok()?;
    let path = resolve(ws, &input.file_path);
    match name {
        "Read" => {
            let offset = whole(&input.offset);
            let limit = whole(&input.limit);
            // Claude Code skips calls whose range is not a valid one.
            if offset.is_some_and(|o| o < 0) || limit.is_some_and(|l| l < 1) {
                return None;
            }
            Some(Call::Read { path, partial: offset.is_some_and(|o| o > 1) || limit.is_some() })
        }
        "Write" => Some(Call::Write { path, content: input.content? }),
        "Edit" => Some(Call::Edit { path }),
        _ => None,
    }
}

fn whole(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// A result moved to a file: its text is not the file's.
fn spilled(text: &str) -> bool {
    text.lines().any(|l| l == "<persisted-output>" || l == "<truncated-output>")
}

/// The file text a Read result showed: notes in `<system-reminder>` tags
/// dropped, line numbers stripped, trimmed (Claude Code's reading of it).
fn shown_text(result: &str) -> String {
    let head_end = result.rfind("</system-reminder>").map_or(0, |i| i + "</system-reminder>".len());
    let mut text = without_reminders(&result[..head_end]);
    text.push_str(&result[head_end..]);
    text.split('\n').map(strip_line_number).collect::<Vec<_>>().join("\n").trim().to_owned()
}

fn without_reminders(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<system-reminder>") {
        let Some(len) = rest[start..].find("</system-reminder>") else { break };
        out.push_str(&rest[..start]);
        rest = &rest[start + len + "</system-reminder>".len()..];
    }
    out.push_str(rest);
    out
}

/// `  12\tline`, `12→line` or `12:line` → `line` (Claude Code's `^\s*\d+[→\t:](.*)$`).
fn strip_line_number(line: &str) -> &str {
    let trimmed = line.trim_start();
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return line;
    }
    let rest = &trimmed[digits..];
    rest.strip_prefix('\t').or_else(|| rest.strip_prefix(':')).or_else(|| rest.strip_prefix('\u{2192}')).unwrap_or(line)
}

/// The stamp a seen file gets now; `None` when it is gone.
fn check(seen: &Seen) -> Option<FileStamp> {
    let meta = std::fs::metadata(&seen.path).ok().filter(std::fs::Metadata::is_file)?;
    let bytes = std::fs::read(&seen.path).ok()?;
    let current = content_hash(&bytes);
    let untouched = seen.at_ms.is_some_and(|at| mtime_ms(&meta) <= at);
    let same_text = || {
        seen.content.as_deref().is_some_and(|content| {
            let now = text::normalize(&text::decode(&bytes).text);
            now.trim() == text::normalize(content).trim()
        })
    };
    if seen.edited || untouched || same_text() {
        return Some(state::stamp(&meta, current, seen.partial));
    }
    // Changed since: what the agent saw is older than the file.
    let saw = seen.content.as_deref().map_or(current ^ 1, |c| content_hash(c.as_bytes()));
    let saw = if saw == current { current ^ 1 } else { saw };
    Some(FileStamp { modified_ms: seen.at_ms.unwrap_or(0), len: meta.len(), hash: saw, partial: seen.partial })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_text_a_result_showed() {
        let result = "<system-reminder>note</system-reminder>1\tfn main() {\n2\t    let x = 1;\n3\t}";
        assert_eq!(shown_text(result), "fn main() {\n    let x = 1;\n}");
        assert_eq!(strip_line_number("  7\u{2192}a"), "a");
        assert_eq!(strip_line_number("no number"), "no number");
        assert!(spilled("<persisted-output>\nOutput too large"));
    }
}
