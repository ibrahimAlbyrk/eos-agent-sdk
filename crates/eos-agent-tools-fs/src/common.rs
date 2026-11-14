//! Plumbing every tool shares: input parsing, the workspace, path checks and
//! Claude Code's error and path wording.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use eos_agent_core::{Extensions, ToolContext, ToolOutput, Workspace};
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

/// A message for the model; becomes an error result.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Fail(pub(crate) String);

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Fail {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

pub(crate) type Res<T> = Result<T, Fail>;

/// Errors read the way Claude Code returns them to the model.
pub(crate) fn finish(result: Res<ToolOutput>) -> ToolOutput {
    result.unwrap_or_else(|Fail(msg)| ToolOutput::error(format!("<tool_use_error>{msg}</tool_use_error>")))
}

pub(crate) fn parse_input<T: DeserializeOwned>(raw: &RawValue) -> Res<T> {
    serde_json::from_str(raw.get()).map_err(|e| Fail(format!("InputValidationError: {e}")))
}

pub(crate) fn workspace(ctx: &ToolContext<'_>) -> Res<Arc<Workspace>> {
    ctx.extensions.get::<Workspace>().ok_or_else(|| Fail("No workspace is configured for this agent.".into()))
}

/// Absolute form of a model-given path (`~` expanded, relative ones from the
/// cwd). A read may go anywhere the process can read: the permission gate,
/// which saw the call, decides.
pub(crate) fn resolve(ws: &Workspace, raw: &str) -> PathBuf {
    ws.resolve_user(raw)
}

/// [`resolve`] for a path the tool writes. Anywhere the process can write
/// (the permission gate, which saw the path, decides), except where the
/// workspace refuses: outside the roots for an agent confined to its
/// worktree, and protected paths.
pub(crate) fn resolve_write(ws: &Workspace, raw: &str) -> Res<PathBuf> {
    let path = resolve(ws, raw);
    match ws.write_refusal(&path) {
        Some(refusal) => Err(Fail(refusal)),
        None => Ok(path),
    }
}

/// The path in `field` a tool writes, resolved as the tool resolves it
/// ([`Tool::written_paths`](eos_agent_core::Tool::written_paths)).
pub(crate) fn written(input: &RawValue, extensions: &Extensions, field: &str) -> Vec<PathBuf> {
    let Some(ws) = extensions.get::<Workspace>() else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(input.get()) else { return Vec::new() };
    value.get(field).and_then(serde_json::Value::as_str).map(|raw| vec![resolve(&ws, raw)]).unwrap_or_default()
}

/// The input with its path `field` absolute, for the permission gate; `None`
/// when it already is, has none, or the agent has no workspace.
pub(crate) fn with_absolute(input: &RawValue, extensions: &Extensions, field: &str) -> Option<Arc<RawValue>> {
    let ws = extensions.get::<Workspace>()?;
    let mut value: serde_json::Map<String, serde_json::Value> = serde_json::from_str(input.get()).ok()?;
    let raw = value.get(field)?.as_str()?;
    let path = resolve(&ws, raw).to_str()?.to_owned();
    if path == raw {
        return None;
    }
    value.insert(field.to_owned(), serde_json::Value::String(path));
    RawValue::from_string(serde_json::to_string(&value).ok()?).ok().map(Arc::from)
}

pub(crate) fn cwd_note(ws: &Workspace) -> String {
    format!("Note: your current working directory is {}.", ws.cwd().display())
}

/// Claude Code's byte sizes: `512 bytes`, `1.5KB`, `256KB`, `1GB`.
pub(crate) fn format_bytes(n: u64) -> String {
    let fmt = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    let kb = n as f64 / 1024.0;
    if kb < 1.0 {
        return format!("{n} bytes");
    }
    if kb < 1024.0 {
        return fmt(kb, "KB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 { fmt(mb, "MB") } else { fmt(mb / 1024.0, "GB") }
}

/// A result path as Claude Code shows it: relative when under the cwd.
pub(crate) fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => path.display().to_string(),
    }
}

pub(crate) fn mtime_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Runs blocking filesystem work off the async runtime.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| Err(Fail(format!("The tool stopped unexpectedly: {e}"))))
}

/// "Did you mean …?" for a missing path: the same file under the cwd when the
/// path points at a sibling project, else a sibling with another extension.
pub(crate) fn suggestion(cwd: &Path, path: &Path) -> Option<String> {
    if let (Some(cwd_parent), false) = (cwd.parent(), path.starts_with(cwd))
        && let Ok(rel) = path.strip_prefix(cwd_parent)
        && cwd.join(rel).exists()
    {
        return Some(cwd.join(rel).display().to_string());
    }
    let dir = path.parent()?;
    let stem = path.file_stem()?;
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let p = entry.path();
        (p != path && p.extension().is_some() && p.file_stem() == Some(stem))
            .then(|| entry.file_name().to_string_lossy().into_owned())
    })
}

pub(crate) fn missing_file_message(ws: &Workspace, path: &Path) -> String {
    let mut msg = format!("File does not exist. {}", cwd_note(ws));
    if let Some(s) = suggestion(&ws.cwd(), path) {
        msg.push_str(&format!(" Did you mean {s}?"));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_read_like_claude_code() {
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(262_144), "256KB");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(1_073_741_824), "1GB");
        assert_eq!(format_bytes(20_971_520), "20MB");
    }

    #[test]
    fn the_gate_sees_absolute_paths() {
        let ext = Extensions::default();
        ext.insert(Workspace::new("/w/p".into(), vec![], "/tmp/s".into()));
        let shown = |json: &str, field: &str| {
            let raw = RawValue::from_string(json.to_owned()).expect("json");
            with_absolute(&raw, &ext, field).map(|v| v.get().to_owned())
        };
        assert_eq!(
            shown(r#"{"file_path":"../x/a.png"}"#, "file_path").as_deref(),
            Some(r#"{"file_path":"/w/x/a.png"}"#)
        );
        assert_eq!(shown(r#"{"file_path":"/etc/hosts"}"#, "file_path"), None);
        assert_eq!(shown(r#"{"pattern":"*.rs"}"#, "path"), None);
        assert_eq!(shown(r#"{"file_path":"a"}"#, "file_path").as_deref(), Some(r#"{"file_path":"/w/p/a"}"#));
        assert_eq!(shown(r#"{"file_path":"a"}"#, "file_path"), shown(r#"{"file_path":"./a"}"#, "file_path"));
    }

    #[test]
    fn written_paths_are_absolute() {
        let ext = Extensions::default();
        ext.insert(Workspace::new("/w/p".into(), vec![], "/tmp/s".into()));
        let paths = |json: &str| written(&RawValue::from_string(json.to_owned()).expect("json"), &ext, "file_path");
        assert_eq!(paths(r#"{"file_path":"../x/a.txt"}"#), [PathBuf::from("/w/x/a.txt")]);
        assert_eq!(paths(r#"{"content":"x"}"#), Vec::<PathBuf>::new());
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_absolute()) {
            assert_eq!(paths(r#"{"file_path":"~/a.txt"}"#), [PathBuf::from(home).join("a.txt")]);
        }
    }

    #[test]
    fn paths_relative_under_cwd() {
        let cwd = Path::new("/w/p");
        assert_eq!(display_path(cwd, Path::new("/w/p/src/a.rs")), "src/a.rs");
        assert_eq!(display_path(cwd, Path::new("/w/other/a.rs")), "/w/other/a.rs");
    }
}
