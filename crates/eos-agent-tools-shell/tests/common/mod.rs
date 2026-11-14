#![allow(dead_code, unreachable_pub)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use eos_agent_core::{
    CancellationToken, Extensions, PermissionGate, Reminders, Tool, ToolContext, ToolOutput, Workspace,
};
use eos_agent_tools_shell::sandbox::Seatbelt;
use eos_agent_tools_shell::{AgentSandbox, SandboxConfig, SandboxDirs, SandboxRuntime, ShellConfig, ShellTools};
use eos_agent_types::ToolResultContent;
use serde_json::value::RawValue;

/// An agent's view of the shell tools, in a fresh temp directory.
pub struct Harness {
    pub tools: Vec<Arc<dyn Tool>>,
    pub ext: Option<Extensions>,
    /// The workspace root, spelled the way the temp dir is (on macOS through
    /// the /var -> /private/var symlink).
    pub root: PathBuf,
    base: PathBuf,
    calls: AtomicUsize,
}

impl Harness {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(configure: impl FnOnce(&mut ShellConfig)) -> Self {
        let base = std::env::temp_dir().join(format!("eos-shell-test-{}", fastrand::u64(..)));
        let root = base.join("work");
        std::fs::create_dir_all(root.join("sub")).expect("create work dir");
        let mut config = ShellConfig::new(base.join("snapshots"));
        configure(&mut config);
        let ext = Extensions::default();
        ext.insert(Workspace::new(root.clone(), vec![], base.join("scratch")));
        Self { tools: ShellTools::new(config), ext: Some(ext), root, base, calls: AtomicUsize::new(0) }
    }

    /// Under the sandbox with Claude Code's `sandbox` settings `config`
    /// (`enabled` implied); `None` (with a notice) where it cannot run.
    pub fn sandboxed(config: serde_json::Value, gate: Arc<dyn PermissionGate>) -> Option<Self> {
        if !Seatbelt::available() {
            eprintln!("skipped: sandbox-exec is not available here");
            return None;
        }
        let mut config: SandboxConfig = serde_json::from_value(config).expect("sandbox config");
        config.enabled = Some(true);
        let runtime = SandboxRuntime::new(SandboxDirs::from_env(), gate);
        let policy = runtime.policy(config).expect("available").expect("enabled");
        let harness = Self::new();
        harness.ext().insert(AgentSandbox(policy));
        Some(harness)
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn ext(&self) -> &Extensions {
        self.ext.as_ref().expect("agent still alive")
    }

    pub fn workspace(&self) -> Arc<Workspace> {
        self.ext().get::<Workspace>().expect("workspace")
    }

    pub fn scratch(&self) -> PathBuf {
        self.base.join("scratch")
    }

    pub fn tool(&self, name: &str) -> &Arc<dyn Tool> {
        self.tools.iter().find(|t| t.spec().name() == name).expect("tool exists")
    }

    pub async fn call(&self, name: &str, input: serde_json::Value) -> ToolOutput {
        self.call_with(name, input, &CancellationToken::new()).await
    }

    pub async fn call_with(&self, name: &str, input: serde_json::Value, cancel: &CancellationToken) -> ToolOutput {
        let n = self.calls.fetch_add(1, Ordering::Relaxed);
        let raw = RawValue::from_string(input.to_string()).expect("raw json");
        let tool_use_id = format!("toolu_{n}");
        let ctx = ToolContext {
            agent_id: "agent",
            tool_use_id: &tool_use_id,
            cancel,
            extensions: self.ext(),
            transcript: &[],
        };
        self.tool(name).call(&raw, ctx).await
    }

    pub fn reminders(&self) -> Vec<String> {
        self.ext().get_or_default::<Reminders>().take()
    }

    /// Waits until the reminders hold `count` notes and returns them.
    pub async fn wait_reminders(&self, count: usize) -> Vec<String> {
        let mut seen = Vec::new();
        wait_until(|| {
            seen.extend(self.reminders());
            seen.len() >= count
        })
        .await;
        seen
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.ext.take());
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

pub fn text(out: &ToolOutput) -> &str {
    match &out.content {
        ToolResultContent::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

/// The command's own output: the text before Claude Code's
/// `<sandbox_violations>` block, which the system's own denials (macOS 27
/// reads `kern.iossupportversion` in every process) add to sandboxed results.
pub fn output(out: &ToolOutput) -> &str {
    let t = text(out);
    match t.find("<sandbox_violations>") {
        Some(i) => t[..i].trim_end_matches('\n'),
        None => t,
    }
}

/// Polls `check` every 25 ms for up to 15 s.
pub async fn wait_until(mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < Duration::from_secs(15), "condition not met in time");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The first line of `file` as a pid, once the command has written it.
pub async fn pid_from(file: &Path) -> u32 {
    let mut pid = None;
    wait_until(|| {
        pid = std::fs::read_to_string(file).ok().and_then(|s| s.lines().next().and_then(|l| l.trim().parse().ok()));
        pid.is_some()
    })
    .await;
    pid.unwrap_or_default()
}

/// The task id in a background result ("… with ID: b123abcd. …").
pub fn task_id(result: &str) -> String {
    let at = result.find("ID: ").expect("an ID in the result") + 4;
    result[at..].chars().take_while(char::is_ascii_alphanumeric).collect()
}
