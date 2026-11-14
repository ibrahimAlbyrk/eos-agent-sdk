//! Publish and upload bodies, streamed from disk: file contents go into the
//! JSON a chunk at a time (text JSON-escaped, binary base64), so a publish
//! holds one read buffer, never a whole file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use super::RequestBody;

/// A multiple of 3, so every base64 chunk but the last needs no padding.
const CHUNK: usize = 48 * 1024;

/// A local file as it will be sent: its size on the wire is known before
/// anything is sent, so the publish can be split to fit the server's limits.
#[derive(Debug, Clone)]
pub(crate) struct SourceFile {
    pub(crate) path: PathBuf,
    pub(crate) content_type: String,
    /// Sent as JSON-escaped text (else base64).
    pub(crate) text: bool,
    pub(crate) size: u64,
    /// Length of the content inside its JSON string, quotes excluded.
    pub(crate) wire_len: u64,
    pub(crate) sha256: String,
}

impl SourceFile {
    /// Reads the file once, in chunks, for its hash and wire size.
    pub(crate) async fn scan(path: &Path, content_type: &str, text: bool) -> Result<Self, String> {
        let mut file = tokio::fs::File::open(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; CHUNK];
        let (mut size, mut escaped) = (0u64, 0u64);
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let n = file.read(&mut buf).await.map_err(|e| format!("{}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            hasher.update(chunk);
            size += n as u64;
            if text {
                escaped += chunk.iter().map(|b| escaped_len(*b)).sum::<u64>();
                carry.extend_from_slice(chunk);
                carry = utf8_tail(&carry).ok_or_else(|| format!("{} is not UTF-8 text", path.display()))?;
            }
        }
        if !carry.is_empty() {
            return Err(format!("{} is not UTF-8 text", path.display()));
        }
        let wire_len = if text { escaped } else { size.div_ceil(3) * 4 };
        let sha256 = hex::encode(hasher.finalize());
        Ok(Self { path: path.to_owned(), content_type: content_type.to_owned(), text, size, wire_len, sha256 })
    }
}

/// Validates `bytes` as UTF-8 and returns the unfinished character at its end.
fn utf8_tail(bytes: &[u8]) -> Option<Vec<u8>> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Some(Vec::new()),
        Err(e) if e.error_len().is_none() => Some(bytes[e.valid_up_to()..].to_vec()),
        Err(_) => None,
    }
}

fn escaped_len(b: u8) -> u64 {
    match b {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
        0..=0x1f => 6,
        _ => 1,
    }
}

fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0..=0x1f => out.extend_from_slice(format!("\\u{b:04x}").as_bytes()),
            _ => out.push(b),
        }
    }
}

/// One published path in a publish's `manifest`.
#[derive(Debug, Clone)]
pub(crate) enum Entry {
    /// `{content, contentType}`, sent in this body.
    Inline(SourceFile),
    /// `{sha256, contentType}`: the content went up through `/upload` first.
    Staged { sha256: String, content_type: String },
    /// `null`: removes the path.
    Remove,
}

/// The body of `POST /api/frame/deploy/direct`.
#[derive(Debug, Clone, Default)]
pub(crate) struct PublishBody {
    /// Top-level members: slug, title, icon, baseVersion, …
    pub(crate) meta: Map<String, Value>,
    /// The page alone, sent as `content` when there is no `manifest`.
    pub(crate) content: Option<SourceFile>,
    /// Sent as `manifest` (with `mode` in `meta`): `{path: {content,
    /// contentType} | {sha256, contentType} | null}`.
    pub(crate) files: Vec<(String, Entry)>,
}

impl PublishBody {
    pub(crate) fn into_request(self) -> RequestBody {
        let mut segments = Vec::new();
        let mut head = Value::Object(self.meta).to_string();
        head.pop(); // the closing brace; members follow
        let mut sep = if head.len() > 1 { "," } else { "" };
        segments.push(Segment::Static(Bytes::from(head)));
        if let Some(page) = &self.content {
            segments.push(Segment::Static(Bytes::from(format!("{sep}\"content\":\""))));
            segments.push(Segment::File { path: page.path.clone(), mode: Mode::of(page) });
            segments.push(Segment::Static(Bytes::from_static(b"\"")));
            sep = ",";
        }
        if !self.files.is_empty() {
            segments.push(Segment::Static(Bytes::from(format!("{sep}\"manifest\":{{"))));
            for (i, (path, entry)) in self.files.iter().enumerate() {
                let key = format!("{}{}:", if i == 0 { "" } else { "," }, Value::String(path.clone()));
                match entry {
                    Entry::Inline(file) => push_file_object(&mut segments, key, file, None),
                    Entry::Staged { sha256, content_type } => {
                        let obj = serde_json::json!({ "sha256": sha256, "contentType": content_type });
                        segments.push(Segment::Static(Bytes::from(format!("{key}{obj}"))));
                    }
                    Entry::Remove => segments.push(Segment::Static(Bytes::from(format!("{key}null")))),
                }
            }
            segments.push(Segment::Static(Bytes::from_static(b"}")));
        }
        segments.push(Segment::Static(Bytes::from_static(b"}")));
        stream_body(segments, "application/json")
    }
}

/// One `POST /api/frame/upload` batch: `{slug, files: [{path, content, contentType}]}`.
#[derive(Debug, Clone)]
pub(crate) struct UploadBatch {
    pub(crate) slug: String,
    pub(crate) files: Vec<(String, SourceFile)>,
}

impl UploadBatch {
    pub(crate) fn wire_len(&self) -> u64 {
        self.files.iter().map(|(p, f)| f.wire_len + p.len() as u64 + 64).sum()
    }

    pub(crate) fn into_request(self) -> RequestBody {
        let mut segments =
            vec![Segment::Static(Bytes::from(format!("{{\"slug\":{},\"files\":[", Value::String(self.slug.clone()))))];
        for (i, (path, file)) in self.files.iter().enumerate() {
            let lead = format!("{}{{\"path\":{},", if i == 0 { "" } else { "," }, Value::String(path.clone()));
            push_file_object(&mut segments, lead, file, Some(()));
        }
        segments.push(Segment::Static(Bytes::from_static(b"]}")));
        stream_body(segments, "application/json")
    }
}

/// `{"content":"…","contentType":"…"}` after `lead`; with `open`, `lead`
/// already opened the object.
fn push_file_object(segments: &mut Vec<Segment>, lead: String, file: &SourceFile, open: Option<()>) {
    let brace = if open.is_some() { "" } else { "{" };
    segments.push(Segment::Static(Bytes::from(format!("{lead}{brace}\"content\":\""))));
    segments.push(Segment::File { path: file.path.clone(), mode: Mode::of(file) });
    segments.push(Segment::Static(Bytes::from(format!(
        "\",\"contentType\":{}}}",
        Value::String(file.content_type.clone())
    ))));
}

/// A file's bytes as the whole body (asset uploads).
pub(crate) fn raw_file(path: PathBuf, content_type: &str) -> RequestBody {
    stream_body(vec![Segment::File { path, mode: Mode::Raw }], content_type)
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Text,
    Base64,
    Raw,
}

impl Mode {
    fn of(file: &SourceFile) -> Self {
        if file.text { Self::Text } else { Self::Base64 }
    }
}

#[derive(Debug)]
enum Segment {
    Static(Bytes),
    File { path: PathBuf, mode: Mode },
}

fn stream_body(segments: Vec<Segment>, content_type: &str) -> RequestBody {
    let segments: Arc<[Segment]> = segments.into();
    RequestBody::Stream {
        content_type: content_type.into(),
        make: Arc::new(move || {
            let state = Reader { segments: Arc::clone(&segments), next: 0, open: None, buf: Vec::new() };
            let stream = futures_util::stream::unfold(state, |mut st| async move {
                match st.step().await {
                    Ok(Some(bytes)) => Some((Ok(bytes), st)),
                    Ok(None) => None,
                    Err(e) => {
                        st.next = st.segments.len();
                        st.open = None;
                        Some((Err(e), st))
                    }
                }
            });
            reqwest::Body::wrap_stream(stream)
        }),
    }
}

struct Reader {
    segments: Arc<[Segment]>,
    next: usize,
    open: Option<(tokio::fs::File, Mode)>,
    buf: Vec<u8>,
}

impl Reader {
    async fn step(&mut self) -> std::io::Result<Option<Bytes>> {
        loop {
            if let Some((file, mode)) = &mut self.open {
                self.buf.resize(CHUNK, 0);
                let mut filled = 0;
                while filled < CHUNK {
                    let n = file.read(&mut self.buf[filled..]).await?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    self.open = None;
                    self.buf = Vec::new();
                    continue;
                }
                let chunk = &self.buf[..filled];
                let out = match mode {
                    Mode::Text => {
                        let mut out = Vec::with_capacity(filled + filled / 8);
                        escape_into(&mut out, chunk);
                        out
                    }
                    Mode::Base64 => base64::engine::general_purpose::STANDARD.encode(chunk).into_bytes(),
                    Mode::Raw => chunk.to_vec(),
                };
                return Ok(Some(Bytes::from(out)));
            }
            let Some(segment) = self.segments.get(self.next) else {
                return Ok(None);
            };
            self.next += 1;
            match segment {
                Segment::Static(b) => return Ok(Some(b.clone())),
                Segment::File { path, mode } => self.open = Some((tokio::fs::File::open(path).await?, *mode)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_matches_its_length() {
        let text = "a\"b\\c\nd\u{1}é";
        let mut out = Vec::new();
        escape_into(&mut out, text.as_bytes());
        let json = serde_json::to_string(text).expect("json");
        assert_eq!(out, json.as_bytes()[1..json.len() - 1]);
        assert_eq!(text.bytes().map(escaped_len).sum::<u64>(), out.len() as u64);
    }

    #[test]
    fn utf8_split_across_chunks_is_carried() {
        let bytes = "é".as_bytes();
        assert_eq!(utf8_tail(&bytes[..1]), Some(vec![bytes[0]]));
        assert_eq!(utf8_tail(bytes), Some(Vec::new()));
        assert_eq!(utf8_tail(&[0xff, b'a']), None);
    }
}
