//! Local paths in tool input, resolved through the calling agent's
//! [`Workspace`]: a file to publish may be anywhere, a folder to save into
//! anywhere the workspace lets the agent write (the permission gate saw it)
//! or in its scratch directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eos_agent_core::{ToolContext, Workspace};

pub(crate) fn workspace(ctx: &ToolContext<'_>) -> Result<Arc<Workspace>, String> {
    ctx.extensions.get::<Workspace>().ok_or_else(|| "this agent has no workspace, so no local file can be used".into())
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

/// A directory to write into (created if missing): the agent's scratch
/// directory, or anywhere the workspace lets it write.
pub(crate) async fn writable_dir(ws: &Workspace, raw: &str) -> Result<PathBuf, String> {
    let path = ws.resolve_user(raw);
    let real = canonical_lenient(&path).await.ok_or_else(|| format!("{} can't be resolved", path.display()))?;
    let scratch = canonical_lenient(ws.scratch()).await.unwrap_or_else(|| ws.scratch().to_owned());
    if !real.starts_with(&scratch)
        && let Some(refusal) = ws.write_refusal(&path)
    {
        return Err(format!("{refusal} Nothing was saved there."));
    }
    tokio::fs::create_dir_all(&real).await.map_err(|e| format!("{}: {e}", real.display()))?;
    Ok(real)
}

/// The folder `out_dir` names, for the permission gate, unless it is in
/// the agent's scratch directory (saving there needs no approval).
pub(crate) fn out_dir_written(ws: &Workspace, raw: &str) -> Option<PathBuf> {
    let path = ws.resolve_user(raw);
    let scratch = std::fs::canonicalize(ws.scratch()).unwrap_or_else(|_| ws.scratch().to_owned());
    let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    (!path.starts_with(ws.scratch()) && !real.starts_with(&scratch)).then_some(path)
}

/// The input with `out_dir` absolute, for the permission gate; `None` when
/// it already is, has none, or the agent has no workspace.
pub(crate) fn with_absolute_out_dir(
    input: &serde_json::value::RawValue,
    extensions: &eos_agent_core::Extensions,
) -> Option<Arc<serde_json::value::RawValue>> {
    let ws = extensions.get::<Workspace>()?;
    let mut value: serde_json::Map<String, serde_json::Value> = serde_json::from_str(input.get()).ok()?;
    let raw = value.get("out_dir")?.as_str()?;
    let path = ws.resolve_user(raw).to_str()?.to_owned();
    if path == raw {
        return None;
    }
    value.insert("out_dir".to_owned(), serde_json::Value::String(path));
    serde_json::value::RawValue::from_string(serde_json::to_string(&value).ok()?).ok().map(Arc::from)
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

    #[tokio::test]
    async fn saving_goes_where_the_workspace_lets_the_agent_write() {
        let base = std::env::temp_dir().join(format!("eos-artifact-dirs-{}", std::process::id()));
        let (proj, scratch, out) = (base.join("proj"), base.join("scratch"), base.join("out"));
        let free = Workspace::new(proj.clone(), vec![], scratch.clone());
        let confined = Workspace::new(proj.clone(), vec![], scratch.clone()).confined(true);
        let raw = out.join("docs").display().to_string();
        assert!(writable_dir(&free, &raw).await.is_ok());
        let refused = writable_dir(&confined, &raw).await.expect_err("confined");
        assert!(
            refused.ends_with("is outside this agent's working directories. Nothing was saved there."),
            "{refused}"
        );
        let in_scratch = scratch.join("artifacts/a").display().to_string();
        assert!(writable_dir(&confined, &in_scratch).await.is_ok(), "its scratch directory is its own");
        assert_eq!(out_dir_written(&free, "../out"), Some(base.join("out")));
        assert_eq!(out_dir_written(&free, &in_scratch), None, "saving to scratch needs no approval");

        let ext = eos_agent_core::Extensions::default();
        ext.insert(free);
        let input =
            serde_json::value::RawValue::from_string(r#"{"action":"read","out_dir":"../out"}"#.into()).expect("json");
        let shown = with_absolute_out_dir(&input, &ext).expect("absolute");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(shown.get()).expect("json")["out_dir"],
            out.display().to_string()
        );
        let _ = std::fs::remove_dir_all(&base);
    }

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
