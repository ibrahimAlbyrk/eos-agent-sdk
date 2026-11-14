//! Sandboxed command execution, as Claude Code 2.1.319 does it on macOS:
//! each command under its own Seatbelt profile, its network through one
//! shared filtering proxy, its denials reported back from one shared
//! `log stream`. Off unless Claude Code's `sandbox.enabled` is set.
//!
//! One [`SandboxRuntime`] per process holds what commands share; each agent
//! gets a [`SandboxPolicy`] from the settings of its own working directory
//! ([`SandboxLayers`]) and carries it as an [`AgentSandbox`].

mod config;
mod credentials;
mod decide;
mod env;
mod layers;
mod paths;
mod profile;
mod prompt;
mod protect;
pub(crate) mod proxy;
mod runtime;
mod seatbelt;
mod violations;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use config::{
    AwsPair, CredentialEnv, CredentialFile, CredentialMode, CredentialsConfig, Decode, FilesystemConfig, NetworkConfig,
    OnExtractNoMatch, SandboxConfig, Sigv4Policies, Sigv4Policy, TlsTerminateConfig,
};
pub use decide::{needs_permission_anyway, should_sandbox};
pub use layers::SandboxLayers;
pub use proxy::NETWORK_TOOL;
pub(crate) use runtime::{CommandEnv, Prepare, Prepared, SandboxGuard};
pub use runtime::{SandboxPolicy, SandboxRuntime};
pub use seatbelt::Seatbelt;
pub(crate) use violations::annotate;

/// An OS mechanism that confines one command. The launch it returns must
/// keep the pid of the process we start (process groups, kills and timeouts
/// rely on it).
pub trait Sandbox: Send + Sync + fmt::Debug {
    fn wrap(&self, req: &SandboxRequest<'_>) -> io::Result<Wrapped>;
}

/// What one command may touch.
#[derive(Debug)]
pub struct SandboxRequest<'a> {
    /// The agent's sandbox settings.
    pub config: &'a SandboxConfig,
    /// Ends every denial's log line (routes violations back to the command).
    pub log_tag: &'a str,
    /// Where relative settings paths start (the agent's start folder).
    pub cwd: &'a Path,
    /// The agent's working directories: writable.
    pub roots: &'a [PathBuf],
    /// The command's `TMPDIR`: writable.
    pub tmpdir: &'a Path,
    /// Single files writable outside the roots.
    pub write_files: &'a [PathBuf],
    /// The shared proxy's port, the one network destination allowed.
    pub proxy_port: Option<u16>,
    /// The sandbox's own files commands must read even inside denied paths
    /// (the TLS-terminating proxy's CA and trust bundle).
    pub own_read: &'a [PathBuf],
}

/// The program to start instead of the shell, and the arguments that go
/// before the shell's own.
#[derive(Debug, Clone)]
pub struct Wrapped {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Where Claude Code's files are, for the paths it protects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxDirs {
    pub home: Option<PathBuf>,
    /// `~/.claude` or `$CLAUDE_CONFIG_DIR`.
    pub config_dir: Option<PathBuf>,
    /// The managed settings folder.
    pub managed_dir: Option<PathBuf>,
}

impl SandboxDirs {
    /// From `$HOME` and `$CLAUDE_CONFIG_DIR`, with macOS's managed folder.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME");
        let config_dir = var("CLAUDE_CONFIG_DIR").or_else(|| home.as_ref().map(|h| h.join(".claude")));
        Self { home, config_dir, managed_dir: Some(PathBuf::from("/Library/Application Support/ClaudeCode")) }
    }
}

/// The sandbox is on but cannot run here and `failIfUnavailable` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxUnavailable;

impl fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sandbox.enabled is set but the sandbox cannot start on this machine (sandbox.failIfUnavailable)")
    }
}

impl std::error::Error for SandboxUnavailable {}

/// The agent tree a command belongs to, for how long a network "yes" lasts
/// (a top-level agent and its subagents share one). Without it, each agent
/// is its own session.
#[derive(Debug, Clone)]
pub struct SandboxSession(pub String);

/// An agent's sandbox (in its extensions): commands of an agent without one
/// run unsandboxed.
#[derive(Debug, Clone)]
pub struct AgentSandbox(pub Arc<SandboxPolicy>);

/// Claude Code's changes to a shell tool's schema with the sandbox on: an
/// `allowed_domains` property on Bash and Monitor, and on Bash no
/// `dangerouslyDisableSandbox` when unsandboxed commands are not allowed.
/// False for other tools.
pub fn sandbox_schema(tool: &str, schema: &mut serde_json::Value, allow_unsandboxed: bool) -> bool {
    if !matches!(tool, "Bash" | "Monitor") {
        return false;
    }
    let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) else { return false };
    props.insert("allowed_domains".into(), crate::bash::allowed_domains_schema());
    if tool == "Bash" && !allow_unsandboxed {
        props.remove("dangerouslyDisableSandbox");
    }
    true
}
