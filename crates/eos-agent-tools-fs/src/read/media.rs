//! Images and PDFs: returned as base64 content blocks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use eos_agent_core::{ToolContext, ToolOutput, Workspace, content_hash};
use eos_agent_types::ContentBlock;
use serde_json::value::RawValue;

use crate::common::{Fail, Res, blocking, format_bytes, missing_file_message};
use crate::state;

pub(super) const IMAGE_EXTS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
const MAX_IMAGE_BASE64: usize = 5 * 1024 * 1024;
const MAX_IMAGE_FILE: u64 = 128 * 1024 * 1024;
const MAX_PDF_WHOLE: u64 = 20 * 1024 * 1024;
const MAX_PDF_RENDER: u64 = 100 * 1024 * 1024;
const MAX_PDF_PAGES_WHOLE: usize = 10;
pub(super) const MAX_PDF_PAGES: u64 = 20;

pub(super) fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// `{"type": kind, "source": {"type":"base64","media_type":…,"data":…}}`
pub(super) fn base64_block(kind: &str, media_type: &str, bytes: &[u8]) -> ContentBlock {
    block(kind, media_type, bytes.len().div_ceil(3) * 4, |json| STANDARD.encode_string(bytes, json))
}

/// Like [`base64_block`] for data already in base64 (checked by the caller).
pub(super) fn encoded_block(kind: &str, media_type: &str, data: &str) -> ContentBlock {
    block(kind, media_type, data.len(), |json| json.push_str(data))
}

fn block(kind: &str, media_type: &str, data_len: usize, data: impl FnOnce(&mut String)) -> ContentBlock {
    let mut json = String::with_capacity(data_len + 96);
    json.push_str(r#"{"type":""#);
    json.push_str(kind);
    json.push_str(r#"","source":{"type":"base64","media_type":""#);
    json.push_str(media_type);
    json.push_str(r#"","data":""#);
    data(&mut json);
    json.push_str(r#""}}"#);
    let raw = RawValue::from_string(json).unwrap_or_else(|_| {
        RawValue::from_string(r#"{"type":"text","text":"[unreadable media]"}"#.into()).expect("literal JSON")
    });
    ContentBlock::Unknown(raw)
}

pub(super) fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text { text, citations: None }
}

/// Reads a whole file of at most `max` bytes off the runtime.
async fn load(
    ws: &Workspace,
    path: &Path,
    max: u64,
    too_big: impl FnOnce(u64) -> String,
) -> Res<(Vec<u8>, std::fs::Metadata)> {
    let p = path.to_path_buf();
    let loaded = blocking(move || {
        Ok((|| -> std::io::Result<Result<(Vec<u8>, std::fs::Metadata), u64>> {
            let meta = std::fs::metadata(&p)?;
            if meta.len() > max {
                return Ok(Err(meta.len()));
            }
            Ok(Ok((std::fs::read(&p)?, meta)))
        })())
    })
    .await?;
    match loaded {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(size)) => Err(Fail(too_big(size))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Fail(missing_file_message(ws, path))),
        Err(e) => Err(Fail(e.to_string())),
    }
}

pub(super) async fn read_image(ctx: &ToolContext<'_>, ws: &Workspace, path: PathBuf) -> Res<ToolOutput> {
    let (bytes, meta) = load(ws, &path, MAX_IMAGE_FILE, |_| {
        format!(
            "File is too large to read as an image (the limit is {}): {}. If it is an image, resize it or export a smaller copy, then read that.",
            format_bytes(MAX_IMAGE_FILE),
            path.display()
        )
    })
    .await?;
    if bytes.is_empty() {
        return Err(Fail(format!("Image file is empty: {}", path.display())));
    }
    let Some(media_type) = image_type(&bytes) else {
        return Err(Fail(format!(
            "File has an image extension but its content is not a valid PNG/JPEG/GIF/WebP. Detected: {}. This usually means a download saved an error/login page instead of the image. Use `file \"{}\"` to confirm, or read it as text with Bash (e.g. `head -c 500`).",
            describe(&bytes),
            path.display()
        )));
    };
    if bytes.len().div_ceil(3) * 4 > MAX_IMAGE_BASE64 {
        return Err(Fail(format!(
            "Image is too large to send ({}; at most {} once base64-encoded). Resize it or export a smaller copy, then read that.",
            format_bytes(bytes.len() as u64),
            format_bytes(MAX_IMAGE_BASE64 as u64)
        )));
    }
    state::record(ctx, &path, state::stamp(&meta, content_hash(&bytes), false), false);
    Ok(ToolOutput::blocks(vec![base64_block("image", media_type, &bytes)]))
}

fn describe(bytes: &[u8]) -> &'static str {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]).to_ascii_lowercase();
    if head.contains("<html") || head.contains("<!doctype") {
        "an HTML page"
    } else if head.trim_start().starts_with('{') {
        "JSON text"
    } else if std::str::from_utf8(bytes).is_ok() {
        "plain text"
    } else {
        "unrecognized binary data"
    }
}

/// `"3"` or `"1-5"` (`"4-"` runs to the end), 1-based.
pub(super) fn parse_pages(raw: &str) -> Res<(u64, Option<u64>)> {
    let invalid = || {
        Fail(format!(
            "Invalid pages parameter: \"{raw}\". Give one page (\"3\") or one range (\"1-5\"), not a list. To read several pages or ranges, read each one separately. Pages are 1-indexed."
        ))
    };
    let num = |s: &str| {
        (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let trimmed = raw.trim();
    let (first, last) = match trimmed.split_once('-') {
        None => (num(trimmed).ok_or_else(invalid)?, Some(num(trimmed).ok_or_else(invalid)?)),
        Some((a, b)) => {
            let first = num(a.trim_end()).ok_or_else(invalid)?;
            let b = b.trim_start();
            (first, if b.is_empty() { None } else { Some(num(b).ok_or_else(invalid)?) })
        }
    };
    if first < 1 || last.is_some_and(|l| l < first) {
        return Err(invalid());
    }
    let count = last.map_or(MAX_PDF_PAGES + 1, |l| l - first + 1);
    if count > MAX_PDF_PAGES {
        return Err(Fail(format!(
            "Page range \"{raw}\" exceeds maximum of {MAX_PDF_PAGES} pages per request. Please use a smaller range."
        )));
    }
    Ok((first, last))
}

pub(super) async fn read_pdf(
    ctx: &ToolContext<'_>,
    ws: &Workspace,
    path: PathBuf,
    pages: Option<&str>,
) -> Res<ToolOutput> {
    if let Some(pages) = pages {
        return render_pages(ctx, ws, path, parse_pages(pages)?).await;
    }
    let (bytes, meta) = load(ws, &path, MAX_PDF_WHOLE, |_| {
        format!("PDF file exceeds maximum allowed size of {}.", format_bytes(MAX_PDF_WHOLE))
    })
    .await?;
    check_pdf(&path, &bytes)?;
    let pages = count_pages(&bytes);
    if pages > MAX_PDF_PAGES_WHOLE {
        return Err(Fail(format!(
            "This PDF has {pages} pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum {MAX_PDF_PAGES} pages per request."
        )));
    }
    state::record(ctx, &path, state::stamp(&meta, content_hash(&bytes), false), false);
    let header = format!("PDF file read: {} ({})", path.display(), format_bytes(meta.len()));
    Ok(ToolOutput::blocks(vec![text_block(header), base64_block("document", "application/pdf", &bytes)]))
}

fn check_pdf(path: &Path, bytes: &[u8]) -> Res<()> {
    if bytes.is_empty() {
        return Err(Fail(format!("PDF file is empty: {}", path.display())));
    }
    if !bytes.starts_with(b"%PDF-") {
        return Err(Fail(format!("File is not a valid PDF (missing %PDF- header): {}", path.display())));
    }
    Ok(())
}

/// Page objects (`/Type /Page`, not `/Pages`). Pages inside compressed object
/// streams are invisible, so this can only undercount; 0 means unknown.
fn count_pages(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut rest = bytes;
    while let Some(i) = rest.windows(5).position(|w| w == b"/Type") {
        rest = &rest[i + 5..];
        let after = rest.iter().position(|b| !b.is_ascii_whitespace()).unwrap_or(rest.len());
        let tail = &rest[after..];
        if tail.starts_with(b"/Page") && !tail[5..].first().is_some_and(u8::is_ascii_alphanumeric) {
            count += 1;
        }
    }
    count
}

static RENDER_SEQ: AtomicU64 = AtomicU64::new(0);

async fn render_pages(
    ctx: &ToolContext<'_>,
    ws: &Workspace,
    path: PathBuf,
    (first, last): (u64, Option<u64>),
) -> Res<ToolOutput> {
    let meta = std::fs::metadata(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => Fail(missing_file_message(ws, &path)),
        _ => Fail(e.to_string()),
    })?;
    if meta.len() == 0 {
        return Err(Fail(format!("PDF file is empty: {}", path.display())));
    }
    if meta.len() > MAX_PDF_RENDER {
        return Err(Fail(format!(
            "PDF file exceeds maximum allowed size for text extraction ({}).",
            format_bytes(MAX_PDF_RENDER)
        )));
    }
    let dir = ws.scratch().join(format!("pdf-{}-{}", std::process::id(), RENDER_SEQ.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&dir).map_err(|e| Fail(format!("Could not create {}: {e}", dir.display())))?;
    let result = super::pdftoppm::render(&path, &dir, first, last, ctx.cancel, Duration::from_secs(120)).await;
    let cleanup = dir.clone();
    let pages = blocking(move || {
        let pages = result.and_then(|files| {
            files.iter().map(|f| std::fs::read(f).map_err(|e| Fail(e.to_string()))).collect::<Res<Vec<_>>>()
        });
        let _ = std::fs::remove_dir_all(&cleanup);
        pages
    })
    .await?;
    state::record(ctx, &path, state::stamp(&meta, 0, true), false);
    let mut blocks = Vec::with_capacity(pages.len() + 1);
    blocks.push(text_block(format!(
        "PDF pages extracted: {} page(s) from {} ({})",
        pages.len(),
        path.display(),
        format_bytes(meta.len())
    )));
    blocks.extend(pages.iter().map(|jpeg| base64_block("image", "image/jpeg", jpeg)));
    Ok(ToolOutput::blocks(blocks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_ranges() {
        assert_eq!(parse_pages("3").ok().map(|r| r.0), Some(3));
        assert_eq!(parse_pages(" 1 - 5 ").ok(), Some((1, Some(5))));
        assert!(parse_pages("1,3").is_err());
        assert!(parse_pages("0").is_err());
        assert!(parse_pages("5-2").is_err());
        assert!(parse_pages("1-21").is_err());
        assert!(parse_pages("4-").is_err());
    }

    #[test]
    fn counts_page_objects() {
        let pdf = b"%PDF-1.4 << /Type /Pages /Count 2 >> << /Type /Page >> << /Type/Page /Parent 1 >>";
        assert_eq!(count_pages(pdf), 2);
    }

    #[test]
    fn sniffs_images() {
        assert_eq!(image_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"<html>"), None);
    }
}
