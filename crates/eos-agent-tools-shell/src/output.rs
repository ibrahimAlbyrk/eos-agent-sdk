//! Turning a command's output file into result text without loading more than
//! the result can show.

use std::io::{self, SeekFrom};
use std::path::Path;

use eos_agent_core::{PREVIEW_CHARS, persisted_output};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Bytes read to build a preview: enough for PREVIEW_CHARS of any UTF-8.
const PREVIEW_BYTES: u64 = 4 * PREVIEW_CHARS as u64;

/// The output as the result shows it: inline when it fits in `max_chars`,
/// else moved to `spill_path` and previewed.
pub(crate) async fn render(file: &Path, max_chars: usize, spill_path: &Path) -> io::Result<String> {
    let size = tokio::fs::metadata(file).await?.len();
    // A char is at most 4 bytes, so a bigger file cannot fit.
    let max_bytes = (max_chars as u64).saturating_mul(4);
    if size <= max_bytes {
        let text = clean(&read_prefix(file, size).await?);
        if text.chars().count() <= max_chars {
            return Ok(text);
        }
    }
    let preview = clean(&read_prefix(file, PREVIEW_BYTES).await?);
    if let Some(dir) = spill_path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::rename(file, spill_path).await?;
    Ok(persisted_output(spill_path, size, &preview))
}

/// The last `max_bytes` of the file, starting at a line, cleaned.
pub(crate) async fn tail(file: &Path, max_bytes: u64) -> io::Result<String> {
    let mut f = File::open(file).await?;
    let size = f.metadata().await?.len();
    let start = size.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).await?;
    let mut buf = Vec::with_capacity(usize::try_from(size - start).unwrap_or(0));
    f.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let text = if start > 0 { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
    Ok(clean(text))
}

async fn read_prefix(file: &Path, max_bytes: u64) -> io::Result<String> {
    let mut buf = Vec::new();
    File::open(file).await?.take(max_bytes).read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Leading blank lines and trailing whitespace dropped.
pub(crate) fn clean(text: &str) -> String {
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            start = i + 1;
        } else if !c.is_whitespace() {
            break;
        }
    }
    text[start.min(text.len())..].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_leading_blank_lines_and_trailing_space() {
        assert_eq!(clean("\n  \n  hi\nthere  \n\n"), "  hi\nthere");
        assert_eq!(clean("   \n"), "");
        assert_eq!(clean("x"), "x");
    }
}
