//! Claude Code's `<persisted-output>` wording for a result too large to send:
//! the full text goes to a file and the model gets its path and a preview.
//! Pure text; whoever writes the file supplies its path and size.

use std::path::Path;

/// How much of an over-size result the preview shows.
pub const PREVIEW_CHARS: usize = 2000;

/// The result text that stands in for output saved to `path`. `text` needs to
/// hold only the start of the output (at least [`PREVIEW_CHARS`] chars when
/// there are that many).
pub fn persisted_output(path: &Path, size: u64, text: &str) -> String {
    let (preview, has_more) = preview(text);
    format!(
        "<persisted-output>\nOutput too large ({}). Full output saved to: {}\n\nPreview (first {}):\n{preview}{}</persisted-output>",
        format_size(size),
        path.display(),
        format_size(PREVIEW_CHARS as u64),
        if has_more { "\n...\n" } else { "\n" }
    )
}

/// The first PREVIEW_CHARS chars, cut at a line end when one is in the second half.
fn preview(text: &str) -> (&str, bool) {
    let Some((limit, _)) = text.char_indices().nth(PREVIEW_CHARS) else { return (text, false) };
    let head = &text[..limit];
    let cut = match head.rfind('\n') {
        Some(nl) if head[..nl].chars().count() > PREVIEW_CHARS / 2 => nl,
        _ => limit,
    };
    (&text[..cut], true)
}

/// Sizes as Claude Code prints them: `500 bytes`, `2KB`, `32.1KB`, `3MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let shown = format!("{value:.1}");
    format!("{}{}", shown.strip_suffix(".0").unwrap_or(&shown), UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_claude_code() {
        assert_eq!(format_size(500), "500 bytes");
        assert_eq!(format_size(2000), "2KB");
        assert_eq!(format_size(32_870), "32.1KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3MB");
    }

    #[test]
    fn preview_cuts_at_a_line_end() {
        let text = format!("{}\n{}", "a".repeat(1500), "b".repeat(1000));
        let (head, more) = preview(&text);
        assert!(more);
        assert_eq!(head.len(), 1500);
        let (head, more) = preview("short");
        assert_eq!((head, more), ("short", false));
    }

    #[test]
    fn short_output_has_no_ellipsis() {
        let out = persisted_output(Path::new("/s/t.txt"), 12, "0123456789a");
        assert_eq!(
            out,
            "<persisted-output>\nOutput too large (12 bytes). Full output saved to: /s/t.txt\n\nPreview (first \
             2KB):\n0123456789a\n</persisted-output>"
        );
    }
}
