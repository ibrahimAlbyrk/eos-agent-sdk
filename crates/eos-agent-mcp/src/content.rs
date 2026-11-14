//! MCP tool results → the `tool_result` content Claude Code sends, with its
//! output cap.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use eos_agent_core::ToolOutput;
use eos_agent_types::ContentBlock;
use serde_json::value::RawValue;
use serde_json::{Value, json};

/// Token cost Claude Code charges an image when sizing MCP output.
const IMAGE_TOKENS: usize = 1600;
const CHARS_PER_TOKEN: usize = 4;
/// Upper bound for a tool's `anthropic/maxResultSizeChars`.
pub(crate) const MAX_RESULT_SIZE_CHARS_CEILING: usize = 500_000;
const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Maps a `tools/call` result. `max_tokens` is the output cap; `blob_dir`
/// receives binary content the model cannot take inline.
pub(crate) async fn tool_output(server: &str, result: &Value, max_tokens: usize, blob_dir: &Path) -> ToolOutput {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return ToolOutput::error(error_text(result));
    }
    if let Some(legacy) = result.get("toolResult") {
        let text = match legacy {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        return ToolOutput::text(text);
    }
    let items = result.get("content").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let mut blocks = Vec::with_capacity(items.len());
    for item in items {
        blocks.extend(map_item(server, item, blob_dir).await);
    }
    if let Some(structured) = result.get("structuredContent") {
        let has_text = blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if !text.trim().is_empty()));
        if !has_text {
            blocks.retain(|b| !matches!(b, ContentBlock::Text { .. }));
            blocks.push(text_block(structured.to_string()));
        }
    }
    if estimate_tokens(&blocks) > max_tokens {
        blocks = truncate(blocks, max_tokens);
    }
    ToolOutput::blocks(blocks)
}

/// Claude Code's message for a result with `isError: true`.
fn error_text(result: &Value) -> String {
    let parts: Vec<String> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| match item.get("text") {
            Some(text) => Some(text.as_str().map_or_else(|| text.to_string(), str::to_owned)),
            None if item.get("type").and_then(Value::as_str) == Some("resource_link") => Some(resource_link(item)),
            None => None,
        })
        .collect();
    if !parts.is_empty() {
        parts.join(" ")
    } else if let Some(error) = result.get("error") {
        error.as_str().map_or_else(|| error.to_string(), str::to_owned)
    } else {
        "Unknown error".into()
    }
}

async fn map_item(server: &str, item: &Value, blob_dir: &Path) -> Vec<ContentBlock> {
    let str_field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    match str_field("type") {
        "text" => vec![text_block(str_field("text").to_owned())],
        "image" => {
            let (data, mime) = (str_field("data"), str_field("mimeType"));
            if IMAGE_TYPES.contains(&mime) {
                vec![image_block(data, mime)]
            } else {
                vec![text_block(save_blob(blob_dir, server, data, mime, &format!("[Image from {server}] ")).await)]
            }
        }
        "audio" => {
            let prefix = format!("[Audio from {server}] ");
            vec![text_block(save_blob(blob_dir, server, str_field("data"), str_field("mimeType"), &prefix).await)]
        }
        "resource" => {
            let resource = item.get("resource").unwrap_or(&Value::Null);
            let field = |key: &str| resource.get(key).and_then(Value::as_str);
            let prefix = format!("[Resource from {server} at {}] ", field("uri").unwrap_or_default());
            let mime = field("mimeType").unwrap_or_default();
            match (field("text"), field("blob")) {
                (Some(text), _) => vec![text_block(format!("{prefix}{text}"))],
                (None, Some(blob)) if IMAGE_TYPES.contains(&mime) => vec![text_block(prefix), image_block(blob, mime)],
                (None, Some(blob)) => vec![text_block(save_blob(blob_dir, server, blob, mime, &prefix).await)],
                (None, None) => Vec::new(),
            }
        }
        "resource_link" => vec![text_block(resource_link(item))],
        _ => Vec::new(),
    }
}

fn resource_link(item: &Value) -> String {
    let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
    let mut text = format!("[Resource link: {}] {}", field("name"), field("uri"));
    if let Some(description) = item.get("description").and_then(Value::as_str) {
        text.push_str(&format!(" ({description})"));
    }
    text
}

pub(crate) fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text { text, citations: None }
}

fn image_block(data: &str, mime: &str) -> ContentBlock {
    raw_block(&json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}}))
}

fn raw_block(value: &Value) -> ContentBlock {
    let raw = RawValue::from_string(value.to_string()).expect("serialized JSON is valid");
    ContentBlock::Unknown(raw)
}

fn is_image(block: &ContentBlock) -> bool {
    block.kind() == Some("image")
}

fn estimate_tokens(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text, .. } => text.len().div_ceil(CHARS_PER_TOKEN),
            b if is_image(b) => IMAGE_TOKENS,
            _ => 0,
        })
        .sum()
}

fn images_left_out(count: usize) -> String {
    if count == 1 {
        "[Image left out: it did not fit in the tool output limit]".into()
    } else {
        format!("[{count} images left out: they did not fit in the tool output limit]")
    }
}

fn truncation_notice(max_tokens: usize) -> String {
    format!(
        "\n\n[OUTPUT TRUNCATED - exceeded {max_tokens} token limit]\n\nThe tool output was truncated. If this MCP \
         server provides pagination or filtering tools, use them to retrieve specific portions of the data. If \
         pagination is not available, inform the user that you are working with truncated output and results may \
         be incomplete."
    )
}

/// Keeps blocks in order until the character budget runs out, cutting the
/// text block that crosses it; images that do not fit are left out with a
/// note in their place. The truncation notice goes last.
fn truncate(blocks: Vec<ContentBlock>, max_tokens: usize) -> Vec<ContentBlock> {
    let budget = max_tokens * CHARS_PER_TOKEN;
    let mut out = Vec::new();
    let mut used = 0;
    let mut left_out = 0;
    let mut note_at = None;
    let mut cut = false;
    for block in blocks {
        if cut {
            left_out += usize::from(is_image(&block));
            continue;
        }
        match block {
            ContentBlock::Text { text, citations } => {
                let room = budget.saturating_sub(used);
                if room == 0 {
                    cut = true;
                } else if text.len() <= room {
                    used += text.len();
                    out.push(ContentBlock::Text { text, citations });
                } else {
                    let end = (0..=room).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                    out.push(text_block(text[..end].to_owned()));
                    cut = true;
                }
            }
            b if is_image(&b) => {
                let cost = IMAGE_TOKENS * CHARS_PER_TOKEN;
                if used + cost <= budget {
                    used += cost;
                    out.push(b);
                } else {
                    if note_at.is_none() {
                        note_at = Some(out.len());
                        used += images_left_out(1).len();
                    }
                    left_out += 1;
                }
            }
            other => out.push(other),
        }
    }
    if left_out > 0 {
        out.insert(note_at.unwrap_or(out.len()), text_block(images_left_out(left_out)));
    }
    out.push(text_block(truncation_notice(max_tokens)));
    out
}

/// Writes base64 `data` under `dir`; returns the line the model sees and,
/// when the write worked, the file.
pub(crate) async fn save_blob_at(
    dir: &Path,
    server: &str,
    data: &str,
    mime: &str,
    prefix: &str,
) -> (String, Option<PathBuf>) {
    let kind = if mime.is_empty() { "unknown type" } else { mime };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(bytes) => bytes,
        Err(e) => return (format!("{prefix}Binary content ({kind}) could not be decoded: {e}"), None),
    };
    match write_blob(dir, server, &bytes, mime).await {
        Ok(path) => {
            let note =
                format!("{prefix}Binary content ({kind}, {}) saved to {}", format_size(bytes.len()), path.display());
            (note, Some(path))
        }
        Err(e) => {
            (format!("{prefix}Binary content ({kind}, {} bytes) could not be saved to disk: {e}", bytes.len()), None)
        }
    }
}

async fn save_blob(dir: &Path, server: &str, data: &str, mime: &str, prefix: &str) -> String {
    save_blob_at(dir, server, data, mime, prefix).await.0
}

async fn write_blob(dir: &Path, server: &str, bytes: &[u8], mime: &str) -> std::io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "mcp-{}-blob-{millis}-{}-{seq}.{}",
        crate::tool::normalize(server),
        std::process::id(),
        extension(mime)
    );
    tokio::fs::create_dir_all(dir).await?;
    let path = dir.join(name);
    tokio::fs::write(&path, bytes).await?;
    Ok(path)
}

fn extension(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "application/json" => "json",
        "audio/mpeg" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        m if m.starts_with("text/") => "txt",
        _ => "bin",
    }
}

/// Claude Code's file-size wording ("812 bytes", "1.5KB", "3MB").
pub(crate) fn format_size(bytes: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let kb = bytes as f64 / 1024.0;
    let short = |v: f64, unit: &str| format!("{}{unit}", format!("{v:.1}").trim_end_matches(".0"));
    if kb < 1.0 {
        format!("{bytes} bytes")
    } else if kb < 1024.0 {
        short(kb, "KB")
    } else if kb < 1024.0 * 1024.0 {
        short(kb / 1024.0, "MB")
    } else {
        short(kb / 1024.0 / 1024.0, "GB")
    }
}

#[cfg(test)]
mod tests {
    use eos_agent_types::ToolResultContent;

    use super::*;

    fn texts(out: &ToolOutput) -> Vec<String> {
        let ToolResultContent::Blocks(blocks) = &out.content else { return vec![] };
        blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text, .. } => text.clone(),
                other => other.kind().unwrap_or("?").to_owned(),
            })
            .collect()
    }

    #[tokio::test]
    async fn maps_content_kinds() {
        let dir = std::env::temp_dir().join("eos-mcp-content-test");
        let result = json!({"content": [
            {"type": "text", "text": "hi"},
            {"type": "image", "data": "aGk=", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
            {"type": "resource_link", "uri": "file:///b", "name": "b", "description": "d"},
            {"type": "audio", "data": "aGk=", "mimeType": "audio/wav"},
        ]});
        let out = tool_output("srv", &result, 25_000, &dir).await;
        let t = texts(&out);
        assert_eq!(
            t[..4],
            ["hi", "image", "[Resource from srv at file:///a] body", "[Resource link: b] file:///b (d)"]
        );
        assert!(t[4].starts_with("[Audio from srv] Binary content (audio/wav, 2 bytes) saved to "), "{}", t[4]);
    }

    #[tokio::test]
    async fn errors_and_structured_content() {
        let dir = std::env::temp_dir();
        let err = tool_output(
            "s",
            &json!({"isError": true, "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
            10,
            &dir,
        )
        .await;
        assert!(err.is_error);
        assert!(matches!(&err.content, ToolResultContent::Text(t) if t == "a b"));
        let structured = tool_output("s", &json!({"content": [], "structuredContent": {"x": 1}}), 10, &dir).await;
        assert_eq!(texts(&structured), [r#"{"x":1}"#]);
    }

    #[test]
    fn truncates_text_and_images() {
        let blocks = vec![text_block("a".repeat(30)), image_block("x", "image/png"), text_block("b".repeat(90))];
        let out = truncate(blocks, 30);
        assert_eq!(out.len(), 4);
        assert!(matches!(&out[0], ContentBlock::Text { text, .. } if text.len() == 30));
        assert!(matches!(&out[1], ContentBlock::Text { text, .. } if text.starts_with("[Image left out")));
        let room = 30 * 4 - 30 - images_left_out(1).len();
        assert!(matches!(&out[2], ContentBlock::Text { text, .. } if text == &"b".repeat(room)));
        assert!(matches!(&out[3], ContentBlock::Text { text, .. } if text.contains("exceeded 30 token limit")));
    }

    #[test]
    fn sizes_read_like_claude_code() {
        assert_eq!(format_size(812), "812 bytes");
        assert_eq!(format_size(1536), "1.5KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3MB");
    }
}
