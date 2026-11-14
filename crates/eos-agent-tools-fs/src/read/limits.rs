//! Read's size and token caps, `allow_large`, and the first-page cut of an
//! over-cap whole-file read.

use crate::common::format_bytes;

/// Per-agent Read caps. Put one in the agent's extensions to change them;
/// without one the defaults (Claude Code's) apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
    /// Largest file read whole (no `limit`).
    pub max_size_bytes: u64,
    /// Largest estimated token count one read returns.
    pub max_tokens: u64,
    /// Context tokens an `allow_large` read may use this turn (Claude Code:
    /// 70 % of what is left before the compaction threshold). `None`: the
    /// host cannot tell, so `allow_large` is unavailable.
    pub large_read_room: Option<u64>,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self { max_size_bytes: 262_144, max_tokens: 25_000, large_read_room: Some(100_000) }
    }
}

/// Bytes per token in the estimate (Claude Code's: 4, JSON 2).
pub(super) fn bytes_per_token(ext: &str) -> u64 {
    if matches!(ext, "json" | "jsonl" | "jsonc") { 2 } else { 4 }
}

pub(super) fn estimate_tokens(bytes: usize, ext: &str) -> u64 {
    (bytes as u64).div_ceil(bytes_per_token(ext))
}

/// The caps an `allow_large` read gets, when the room beats the normal cap.
#[derive(Debug, Clone, Copy)]
pub(super) struct Raised {
    pub(super) max_tokens: u64,
    pub(super) max_size_bytes: u64,
}

impl ReadLimits {
    pub(super) fn raised(&self) -> Option<Raised> {
        let room = self.large_read_room?;
        (room > self.max_tokens)
            .then(|| Raised { max_tokens: room, max_size_bytes: self.max_size_bytes.max(room * 12) })
    }

    /// The sentence that tells the model whether `allow_large` would help.
    pub(super) fn hint(&self, opted_in: bool, bytes: Option<u64>, tokens: Option<u64>) -> &'static str {
        if self.large_read_room.is_none() {
            return if opted_in { UNAVAILABLE } else { "" };
        }
        match self.raised() {
            Some(r) if !opted_in => {
                let fits = bytes.is_none_or(|b| b <= r.max_size_bytes) && tokens.is_none_or(|t| t <= r.max_tokens);
                if fits { TRY_LARGE } else { STILL_TOO_LARGE }
            }
            _ => STILL_TOO_LARGE,
        }
    }
}

const TRY_LARGE: &str = " To read it anyway, up to what still fits in your context, call Read again with allow_large: true \u{2014} only if you genuinely need all of it or the user asked for the whole file.";
const STILL_TOO_LARGE: &str = " Even with allow_large, this is more than fits in the context you have left this turn (other large reads in the same turn share that room).";
const UNAVAILABLE: &str = " allow_large is not available where this read runs; read it in parts with offset and limit.";

pub(super) fn too_large(size: u64, max: u64, hint: &str) -> String {
    format!(
        "File content ({}) exceeds maximum allowed size ({}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.{hint}",
        format_bytes(size),
        format_bytes(max)
    )
}

pub(super) fn range_too_large(max: u64, hint: &str) -> String {
    format!(
        "The requested line range contains over {} of text, more than a read can return. Use a smaller limit \u{2014} or, if a single line is this large, no limit will fit it: search for specific content instead.{hint}",
        format_bytes(max)
    )
}

pub(super) fn too_many_tokens(tokens: u64, max: u64, hint: &str) -> String {
    format!(
        "File content ({tokens} tokens) exceeds maximum allowed tokens ({max}). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file.{hint}"
    )
}

/// The first page of an over-cap whole-file read.
#[derive(Debug)]
pub(super) struct FirstPage {
    pub(super) end: usize,
    pub(super) lines: u64,
    /// Cut mid-line: the file's lines are too long to page by line.
    pub(super) by_chars: bool,
}

/// Keeps ~85 % of the lines that fit `cap`, shrinking until the estimate
/// fits; falls back to a character cut for files of very long lines.
pub(super) fn first_page(content: &str, tokens: u64, cap: u64) -> FirstPage {
    let per_token = (content.len() as f64 / tokens.max(1) as f64).max(0.5);
    let est = |len: usize| len as f64 / per_token;
    let starts: Vec<usize> = std::iter::once(0).chain(content.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let total = starts.len();
    let end_of = |n: usize| if n >= total { content.len() } else { starts[n] - 1 };
    let ratio = cap as f64 / tokens.max(1) as f64;
    let mut n = ((total as f64 * ratio * 0.85).floor() as usize).clamp(1, total);
    for _ in 0..6 {
        if est(end_of(n)) <= cap as f64 || n <= 1 {
            break;
        }
        n = ((n as f64 * 0.7).floor() as usize).max(1);
    }
    let end = end_of(n);
    if est(end) <= cap as f64 && !content[..end].trim().is_empty() {
        return FirstPage { end, lines: n as u64, by_chars: false };
    }
    let mut cut = ((cap as f64 * per_token * 0.85).floor() as usize).max(1);
    let mut end = floor_char(content, cut);
    for _ in 0..6 {
        end = floor_char(content, cut);
        if est(end) <= cap as f64 {
            break;
        }
        cut = ((cut as f64 * 0.7).floor() as usize).max(1);
    }
    let lines = content[..end].matches('\n').count() as u64 + 1;
    FirstPage { end, lines, by_chars: true }
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_page_fits_the_cap() {
        let content: String = (0..1000).map(|i| format!("line {i} with some padding text\n")).collect();
        let tokens = estimate_tokens(content.len(), "txt");
        let page = first_page(&content, tokens, 1000);
        assert!(!page.by_chars);
        assert!(estimate_tokens(page.end, "txt") <= 1000);
        assert!(page.lines > 50);
    }

    #[test]
    fn one_long_line_is_cut_by_chars() {
        let content = "x".repeat(400_000);
        let page = first_page(&content, 100_000, 25_000);
        assert!(page.by_chars);
        assert!(page.end <= 100_000);
    }

    #[test]
    fn hints() {
        let l = ReadLimits::default();
        assert_eq!(l.hint(false, Some(300_000), None), TRY_LARGE);
        assert_eq!(l.hint(true, None, Some(30_000)), STILL_TOO_LARGE);
        let none = ReadLimits { large_read_room: None, ..l };
        assert_eq!(none.hint(false, None, None), "");
        assert_eq!(none.hint(true, None, None), UNAVAILABLE);
    }
}
