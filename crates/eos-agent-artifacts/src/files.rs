//! Local paths in tool input, resolved through the calling agent's
//! [`Workspace`]: a file to publish may be anywhere, a folder to save into
//! only inside its roots and scratch directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eos_agent_core::{ToolContext, Workspace};

pub(crate) fn workspace(ctx: &ToolContext<'_>) -> Result<Arc<Workspace>, String> {
    ctx.extensions.get::<Workspace>().ok_or_else(|| "this agent has no workspace, so no local file can be used".into())
}

async fn writable_roots(ws: &Workspace) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(ws.roots().len() + 1);
    for root in ws.roots().iter().map(PathBuf::as_path).chain([ws.scratch()]) {
        out.push(canonical_lenient(root).await.unwrap_or_else(|| root.to_owned()));
    }
    out
}

/// `path` with its nearest existing ancestor's symlinks resolved, so a path
/// that doesn't exist yet compares like one that does.
async fn canonical_lenient(path: &Path) -> Option<PathBuf> {
    let mut probe = path.to_owned();
    loop {
        if let Ok(real) = tokio::fs::canonicalize(&probe).await {
            return Some(real.join(path.strip_prefix(&probe).unwrap_or(Path::new(""))));
        }
        if !probe.pop() {
            return None;
        }
    }
}

/// An existing file, anywhere the process can read (the permission gate saw
/// the call); symlinks resolved.
pub(crate) async fn existing_file(ws: &Workspace, raw: &str) -> Result<PathBuf, String> {
    let path = ws.resolve(Path::new(raw));
    let real = tokio::fs::canonicalize(&path).await.map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = tokio::fs::metadata(&real).await.map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    Ok(real)
}

/// A directory to write into (created if missing), inside the workspace.
pub(crate) async fn writable_dir(ws: &Workspace, raw: &Path) -> Result<PathBuf, String> {
    let path = ws.resolve(raw);
    let roots = writable_roots(ws).await;
    let real = canonical_lenient(&path).await.ok_or_else(|| format!("{} can't be resolved", path.display()))?;
    if !roots.iter().any(|r| real.starts_with(r)) {
        return Err(format!(
            "{} is outside this agent's working directories, so nothing was saved there",
            path.display()
        ));
    }
    tokio::fs::create_dir_all(&real).await.map_err(|e| format!("{}: {e}", real.display()))?;
    Ok(real)
}

/// Where a read of `slug` is saved by default.
pub(crate) fn artifact_dir(ws: &Workspace, slug: &str) -> PathBuf {
    ws.scratch().join("artifacts").join(slug)
}

/// A published path joined under `dir`, refusing `..` and absolute forms.
pub(crate) fn join_published(dir: &Path, published: &str) -> Result<PathBuf, String> {
    let clean = published.trim_start_matches('/');
    if clean.is_empty() || clean.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err(format!("{published:?} is not a published path"));
    }
    Ok(dir.join(clean))
}

/// Media types this client publishes, by extension (the first extension of
/// a type is the one downloads are saved with).
const TYPES: &[(&str, &str)] = &[
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("json", "application/json"),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("xml", "application/xml"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("ogg", "audio/ogg"),
    ("pdf", "application/pdf"),
    ("wasm", "application/wasm"),
];

fn base_type(content_type: &str) -> &str {
    content_type.split(';').next().unwrap_or_default().trim()
}

/// The media type served for a published path, by extension.
pub(crate) fn content_type_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit_once('.')?.1.to_ascii_lowercase();
    TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// `content_type` if this client publishes it.
pub(crate) fn known_type(content_type: &str) -> Option<&'static str> {
    let base = base_type(content_type).to_ascii_lowercase();
    TYPES.iter().find(|(_, t)| *t == base).map(|(_, t)| *t)
}

/// Text types travel as JSON strings; everything else as base64.
pub(crate) fn is_text(content_type: &str) -> bool {
    let base = base_type(content_type);
    base.starts_with("text/")
        || matches!(base, "application/json" | "application/javascript" | "application/xml" | "image/svg+xml")
}

/// File extension for a media type, for saving downloaded assets.
pub(crate) fn extension_for(content_type: &str) -> &'static str {
    let base = base_type(content_type);
    TYPES.iter().find(|(_, t)| *t == base).map_or("bin", |(e, _)| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_paths_stay_inside() {
        let dir = Path::new("/s/a");
        assert_eq!(join_published(dir, "css/site.css").expect("ok"), PathBuf::from("/s/a/css/site.css"));
        assert!(join_published(dir, "../x").is_err());
        assert!(join_published(dir, "a//b").is_err());
        assert_eq!(content_type_for("Logo.PNG"), Some("image/png"));
        assert!(is_text("text/html; charset=utf-8") && !is_text("image/png"));
        assert_eq!(known_type("Image/PNG"), Some("image/png"));
        assert_eq!(extension_for("image/jpeg"), "jpg");
    }
}
