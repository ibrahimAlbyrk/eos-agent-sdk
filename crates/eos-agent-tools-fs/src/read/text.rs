//! Reading a text file: the window, the caps, auto-pagination and the result
//! text Claude Code produces.

use std::path::{Path, PathBuf};

use eos_agent_core::{ToolContext, ToolOutput, Workspace};

use super::limits::{self, ReadLimits};
use super::scan::{ScanError, Window, numbered, scan};
use crate::common::{Fail, Res, blocking, missing_file_message};
use crate::state;

#[derive(Debug)]
pub(super) struct TextRead {
    pub(super) path: PathBuf,
    pub(super) ext: String,
    /// 1-based; 0 reads from the top too.
    pub(super) offset: u64,
    pub(super) limit: Option<u64>,
    pub(super) allow_large: bool,
    pub(super) limits: ReadLimits,
}

pub(super) async fn read_text(ctx: &ToolContext<'_>, ws: &Workspace, req: TextRead) -> Res<ToolOutput> {
    let limits = req.limits;
    let raised = if req.allow_large { limits.raised() } else { None };
    let max_size = raised.map_or(limits.max_size_bytes, |r| r.max_size_bytes);
    let cap = raised.map_or(limits.max_tokens, |r| r.max_tokens);
    let max_selected = (limits.max_tokens * 128).max(raised.map_or(0, |r| r.max_size_bytes));
    let window = Window { start: req.offset.saturating_sub(1), limit: req.limit };

    let path = req.path.clone();
    let cancel = ctx.cancel.clone();
    let scanned =
        blocking(move || Ok(scan(&path, window, max_size, max_selected, &cancel))).await?.map_err(|e| match e {
            ScanError::Fail(f) => f,
            ScanError::NotFound => Fail(missing_file_message(ws, &req.path)),
            ScanError::TooLarge(size) => {
                Fail(limits::too_large(size, max_size, limits.hint(req.allow_large, Some(size), None)))
            }
            ScanError::RangeTooLarge(bytes) => {
                Fail(limits::range_too_large(max_selected, limits.hint(req.allow_large, Some(bytes), None)))
            }
        })?;

    let start_line = req.offset.max(1);
    let whole_file = req.offset <= 1 && req.limit.is_none();
    let mut content = scanned.content;
    let mut lines = scanned.lines;
    let counted_len = if raised.is_some() { numbered(&content, start_line).len() } else { content.len() };
    let tokens = limits::estimate_tokens(counted_len, &req.ext);
    let mut banner = None;
    if tokens > cap {
        let hint = limits.hint(req.allow_large, None, Some(tokens));
        if !whole_file {
            return Err(Fail(limits::too_many_tokens(tokens, cap, hint)));
        }
        let base = limits.max_tokens;
        let cap_text = if raised.is_some() && cap > base {
            format!("allow_large had room for {cap} tokens this turn")
        } else {
            format!("cap {base}")
        };
        let page = limits::first_page(&content, tokens, base);
        banner = Some(page_banner(&req.path, &page, &content, scanned.total_lines, tokens, &cap_text, hint));
        content.truncate(page.end);
        lines = page.lines;
    }

    let truncated = banner.is_some();
    let partial =
        truncated || req.offset > 1 || req.limit.is_some_and(|l| scanned.lines >= l) || lines < scanned.total_lines;
    state::record(ctx, &req.path, state::stamp(&scanned.meta, scanned.hash, partial), truncated);

    let mut out = banner.map(|b| b + "\n\n").unwrap_or_default();
    if !content.is_empty() {
        out.push_str(&numbered(&content, start_line));
    } else if lines >= 1 && scanned.total_lines > 1 {
        out.push_str(&format!("{start_line}\t"));
    } else if lines >= 1 || scanned.total_lines == 0 {
        out.push_str("<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>");
    } else {
        out.push_str(&format!(
            "<system-reminder>Warning: the file exists but is shorter than the provided offset ({}). The file has {} lines.</system-reminder>",
            req.offset, scanned.total_lines
        ));
    }
    if let Some(note) = scanned.not_text {
        out.push_str("\n\n");
        out.push_str(&note.read_note());
    }
    Ok(ToolOutput::text(out))
}

fn page_banner(
    path: &Path,
    page: &limits::FirstPage,
    content: &str,
    total_lines: u64,
    tokens: u64,
    cap_text: &str,
    hint: &str,
) -> String {
    let path = path.display();
    if !page.by_chars && page.lines < total_lines {
        let n = page.lines;
        format!(
            "[Truncated: PARTIAL view \u{2014} {path}: showing lines 1-{n} of {total_lines} total ({tokens} tokens, {cap_text}). Call Read with offset={} limit={n} for the next page, or Grep to find a specific section.{hint} Do NOT answer from this page alone if the answer may be further in the file.]",
            n + 1
        )
    } else {
        let shown = content[..page.end].chars().count();
        let all = content.chars().count();
        format!(
            "[Truncated: PARTIAL view \u{2014} {path}: showing the first {shown} of {all} characters ({tokens} tokens, {cap_text}); this file has very long lines and cannot be paginated by line. Use Grep to find a specific section, or Read with offset/limit to page through it.{hint} Do NOT answer from this excerpt alone if the answer may be elsewhere in the file.]"
        )
    }
}
