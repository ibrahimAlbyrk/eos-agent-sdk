#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use eos_agent_core::{CancellationToken, Extensions, FileStates, Tool, ToolContext, ToolOutput, Workspace};
use eos_agent_tools_fs::{EditTool, GlobTool, GrepTool, NotebookEditTool, ReadTool, WriteTool};
use eos_agent_types::ToolResultContent;
use serde_json::value::RawValue;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A temp directory that is also the agent's workspace.
pub(crate) struct Env {
    pub(crate) dir: PathBuf,
    pub(crate) ext: Extensions,
    pub(crate) cancel: CancellationToken,
}

impl Env {
    pub(crate) fn new() -> Self {
        Self::with(false)
    }

    /// An agent confined to its directory (isolated in its own worktree).
    pub(crate) fn confined() -> Self {
        Self::with(true)
    }

    fn with(confined: bool) -> Self {
        let name = format!("eos-fs-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let dir = dir.canonicalize().expect("canonical");
        let ext = Extensions::default();
        ext.insert(Workspace::new(dir.clone(), vec![], dir.join(".scratch")).confined(confined));
        Self { dir, ext, cancel: CancellationToken::new() }
    }

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub(crate) fn p(&self, rel: &str) -> String {
        self.path(rel).display().to_string()
    }

    pub(crate) fn write(&self, rel: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, content).expect("write");
        path
    }

    pub(crate) fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).expect("read")
    }

    pub(crate) fn read_bytes(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path(rel)).expect("read")
    }

    /// Sets a file's mtime `secs` from now.
    pub(crate) fn set_mtime(&self, rel: &str, secs: i64) {
        let now = SystemTime::now();
        let t = if secs >= 0 {
            now + Duration::from_secs(secs.unsigned_abs())
        } else {
            now - Duration::from_secs(secs.unsigned_abs())
        };
        std::fs::File::options().write(true).open(self.path(rel)).expect("open").set_modified(t).expect("mtime");
    }

    pub(crate) fn states(&self) -> std::sync::Arc<FileStates> {
        self.ext.get_or_default::<FileStates>()
    }

    pub(crate) async fn call(&self, tool: &dyn Tool, input: serde_json::Value) -> ToolOutput {
        let raw = RawValue::from_string(input.to_string()).expect("raw");
        let ctx = ToolContext {
            agent_id: "a1",
            tool_use_id: "t1",
            cancel: &self.cancel,
            extensions: &self.ext,
            transcript: &[],
        };
        tool.call(&raw, ctx).await
    }

    pub(crate) async fn read_tool(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&ReadTool::new(), input).await
    }

    pub(crate) async fn edit(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&EditTool::new(), input).await
    }

    pub(crate) async fn write_tool(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&WriteTool::new(), input).await
    }

    pub(crate) async fn notebook(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&NotebookEditTool::new(), input).await
    }

    pub(crate) async fn glob(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&GlobTool::new(), input).await
    }

    pub(crate) async fn grep(&self, input: serde_json::Value) -> ToolOutput {
        self.call(&GrepTool::new(), input).await
    }

    /// Reads a file whole so it may be edited.
    pub(crate) async fn read_file(&self, rel: &str) {
        let out = self.read_tool(serde_json::json!({"file_path": self.p(rel)})).await;
        assert!(!out.is_error, "read failed: {}", text(&out));
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn text(out: &ToolOutput) -> String {
    match &out.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => serde_json::to_string(blocks).expect("json"),
    }
}

/// The text of a successful result.
#[track_caller]
pub(crate) fn ok(out: &ToolOutput) -> String {
    assert!(!out.is_error, "expected success, got: {}", text(out));
    text(out)
}

/// The message inside an error result.
#[track_caller]
pub(crate) fn err(out: &ToolOutput) -> String {
    assert!(out.is_error, "expected an error, got: {}", text(out));
    let t = text(out);
    t.strip_prefix("<tool_use_error>").and_then(|t| t.strip_suffix("</tool_use_error>")).unwrap_or(&t).to_owned()
}

pub(crate) fn exists(path: &Path) -> bool {
    path.exists()
}
