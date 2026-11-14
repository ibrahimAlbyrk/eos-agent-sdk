//! Turning a response body into the text the model works with.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;

use super::cache::Page;

const MAX_HTML: usize = 1024 * 1024;
pub(crate) const TRUNCATED: &str = "\n\n[Content truncated due to length...]";

/// A page plus the size it counts for in the cache.
pub(crate) async fn build(content_type: String, body: Bytes, scratch: Option<&Path>) -> (Page, usize) {
    let saved = match scratch {
        Some(dir) if is_binary(&content_type) => save(dir, &content_type, &body).await,
        _ => None,
    };
    let bytes = body.len();
    let text = String::from_utf8_lossy(&body).into_owned();
    drop(body);
    let (content, size) = if content_type.contains("text/html") {
        // Conversion is CPU work on up to a megabyte: keep it off the async workers.
        let md = tokio::task::spawn_blocking(move || html_to_markdown(&text)).await.unwrap_or_default();
        let size = md.len();
        (md, size)
    } else {
        (text, bytes)
    };
    (Page { content, content_type, saved }, size)
}

/// HTML → markdown the way Claude Code's turndown setup does it: style,
/// script, noscript and iframe dropped, input cut at 1 MiB.
pub(crate) fn html_to_markdown(html: &str) -> String {
    let cut = floor_boundary(html, MAX_HTML);
    let converter = htmd::HtmlToMarkdown::builder().skip_tags(vec!["style", "script", "noscript", "iframe"]).build();
    match converter.convert(&html[..cut]) {
        Ok(mut md) => {
            if cut < html.len() {
                md.push_str(TRUNCATED);
            }
            md
        }
        Err(_) => html.to_owned(),
    }
}

/// Anything that is not text-like is binary.
pub(crate) fn is_binary(content_type: &str) -> bool {
    if content_type.is_empty() {
        return false;
    }
    let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    !(mime.starts_with("text/")
        || mime.ends_with("+json")
        || mime == "application/json"
        || mime.ends_with("+xml")
        || mime == "application/xml"
        || mime.starts_with("application/javascript")
        || mime == "application/x-www-form-urlencoded")
}

/// `type/subtype` from a content-type header.
pub(crate) fn short_mime(content_type: &str) -> &str {
    let token =
        |s: &str| s.bytes().take_while(|b| b.is_ascii_alphanumeric() || b"_!#$&^.+-".contains(b)).count().min(64);
    let main = token(content_type);
    if main == 0 || content_type.as_bytes().get(main) != Some(&b'/') {
        return "unknown content type";
    }
    let sub = token(&content_type[main + 1..]);
    if sub == 0 { "unknown content type" } else { &content_type[..main + 1 + sub] }
}

/// Claude Code's file-size wording: `512 bytes`, `1.5KB`, `2MB`.
pub(crate) fn format_size(bytes: u64) -> String {
    let fmt = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    let kb = bytes as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{bytes} bytes");
    }
    if kb < 1024.0 {
        return fmt(kb, "KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 { fmt(mb, "MB") } else { fmt(mb / 1024.0, "GB") }
}

async fn save(dir: &Path, content_type: &str, body: &[u8]) -> Option<(PathBuf, u64)> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("webfetch-{ms}-{seq}.{}", extension(content_type)));
    tokio::fs::create_dir_all(dir).await.ok()?;
    tokio::fs::write(&path, body).await.ok()?;
    Some((path, body.len() as u64))
}

fn extension(content_type: &str) -> &str {
    let mime = short_mime(content_type);
    let sub = mime.split('/').nth(1).unwrap_or("");
    let sub = sub.split('+').next().unwrap_or("");
    let sub = sub.rsplit('.').next().unwrap_or("");
    match sub {
        "jpeg" => "jpg",
        "octet-stream" | "" => "bin",
        s if s.len() <= 8 && s.bytes().all(|b| b.is_ascii_alphanumeric()) => s,
        _ => "bin",
    }
}

/// The largest char boundary at or below `max`.
pub(crate) fn floor_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    (0..=max).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html() {
        let md = html_to_markdown(
            "<html><head><style>p{}</style><script>alert(1)</script></head>\
             <body><h1>Title</h1><p>Hello <a href=\"https://x.dev\">link</a></p></body></html>",
        );
        assert!(md.contains("# Title"), "{md}");
        assert!(md.contains("[link](https://x.dev)"), "{md}");
        assert!(!md.contains("alert"), "{md}");
    }

    #[test]
    fn classifies_content() {
        assert!(!is_binary("text/html; charset=utf-8"));
        assert!(!is_binary("application/ld+json"));
        assert!(is_binary("application/pdf"));
        assert_eq!(short_mime("application/pdf; x=1"), "application/pdf");
        assert_eq!(short_mime(""), "unknown content type");
        assert_eq!(extension("image/svg+xml"), "svg");
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(2 * 1024 * 1024), "2MB");
    }
}
