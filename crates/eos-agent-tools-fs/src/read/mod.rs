//! The Read tool: text with line numbers, images, PDFs and notebooks.

mod limits;
mod media;
mod notebook;
mod pdftoppm;
mod scan;
mod text;

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use eos_agent_core::{BoxFuture, Extensions, StoredMessage, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

pub use limits::ReadLimits;
pub(crate) use notebook::{MAX_NOTEBOOK_FILE, NOT_A_NOTEBOOK, too_big_message};

use crate::common::{Fail, Res, finish, parse_input, resolve, with_absolute, workspace};
use crate::spec::tool_spec;

/// Claude Code's `Read`.
#[derive(Debug)]
pub struct ReadTool {
    spec: ToolSpec,
}

impl ReadTool {
    pub fn new() -> Self {
        Self { spec: tool_spec!("Read") }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ReadTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    /// Claude Code shows the gate an absolute `file_path`.
    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        with_absolute(input, extensions, "file_path")
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { finish(run(input, &ctx).await) })
    }

    /// Rebuilds the files the agent had read, written or edited (for Write
    /// and Edit too: one pass over the history serves all three).
    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        Box::pin(crate::restore::restore(history, extensions))
    }
}

/// The model's input, taken leniently the way Claude Code coerces it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    file_path: String,
    #[serde(default)]
    offset: Value,
    #[serde(default)]
    limit: Value,
    #[serde(default)]
    length: Value,
    pages: Option<String>,
    #[serde(default)]
    allow_large: Value,
    /// Models sometimes send one; Claude Code drops it.
    #[serde(default, rename = "description")]
    _description: Value,
}

/// A whole number given as a number, a numeric string or a one-item array.
fn whole_number(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Array(items) if items.len() == 1 => whole_number(&items[0]),
        _ => None,
    }
}

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "tif", "heic", "heif", "avif", "mp4", "mov", "avi",
    "mkv", "webm", "wmv", "flv", "m4v", "mpeg", "mpg", "mp3", "wav", "ogg", "flac", "aac", "m4a", "wma", "aiff",
    "opus", "zip", "tar", "gz", "bz2", "7z", "rar", "xz", "z", "tgz", "iso", "zst", "lz4", "whl", "exe", "dll", "so",
    "dylib", "bin", "o", "a", "obj", "lib", "app", "msi", "deb", "rpm", "dmg", "apk", "pdf", "doc", "docx", "docm",
    "dotx", "dotm", "xls", "xlsx", "xlsm", "xlsb", "xltx", "xltm", "ppt", "pptx", "pptm", "ppsx", "ppsm", "potx",
    "potm", "odt", "ods", "odp", "numbers", "epub", "ttf", "otf", "woff", "woff2", "eot", "pyc", "pyo", "class", "jar",
    "war", "ear", "node", "wasm", "rlib", "sqlite", "sqlite3", "db", "mdb", "idx", "parquet", "feather", "npy", "npz",
    "pickle", "psd", "ai", "eps", "sketch", "fig", "xd", "blend", "3ds", "max", "swf", "fla", "lockb", "dat", "data",
];

fn extension(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// An extensionless file that starts like an image.
fn looks_like_image(path: &Path) -> bool {
    let mut head = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| {
            let n = f.read(&mut head)?;
            Ok(media::image_type(&head[..n]).is_some())
        })
        .unwrap_or(false)
}

async fn run(raw: &RawValue, ctx: &ToolContext<'_>) -> Res<ToolOutput> {
    let input: Input = parse_input(raw)?;
    let ws = workspace(ctx)?;
    let path = resolve(&ws, &input.file_path);
    let limits = ctx.extensions.get::<ReadLimits>().map(|l| *l).unwrap_or_default();
    if let Some(pages) = &input.pages {
        media::parse_pages(pages)?;
    }
    let ext = extension(&path);
    let is_image = media::IMAGE_EXTS.contains(&ext.as_str());
    if BINARY_EXTS.contains(&ext.as_str()) && !is_image && ext != "pdf" {
        return Err(Fail(format!(
            "This tool cannot read binary files. The file appears to be a binary .{ext} file. Use a skill for this file type if one is available, or a shell command or script that can read the format."
        )));
    }
    if ext == "ipynb" {
        return notebook::read_notebook(ctx, &ws, path, limits).await;
    }
    if is_image || (ext.is_empty() && path.is_file() && looks_like_image(&path)) {
        return media::read_image(ctx, &ws, path).await;
    }
    if ext == "pdf" {
        return media::read_pdf(ctx, &ws, path, input.pages.as_deref()).await;
    }
    let offset = whole_number(&input.offset).filter(|n| *n >= 0);
    let limit = whole_number(&input.limit).or_else(|| whole_number(&input.length)).filter(|n| *n > 0);
    let allow_large = matches!(&input.allow_large, Value::Bool(true)) || input.allow_large == "true";
    let req = text::TextRead {
        path,
        ext,
        offset: offset.map_or(1, |n| n as u64),
        limit: limit.map(|n| n as u64),
        allow_large,
        limits,
    };
    text::read_text(ctx, &ws, req).await
}
