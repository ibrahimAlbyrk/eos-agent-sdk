//! Permission gates: one that lets agents look but not change anything, and
//! the sandbox's auto-allow in front of the host's gate.

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use eos_agent_core::{AgentOrigin, BoxFuture, Permission, PermissionGate, ToolCall, always_plan};
use eos_agent_tools_shell::is_read_only_command;
use eos_agent_tools_shell::sandbox::needs_permission_anyway;
use serde_json::Value;

use crate::registry::Registry;

/// Denies file writers, shell commands not known to be read-only, Artifact
/// actions other than reading, and reads that save files outside the
/// working directories; allows everything else.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReadOnly;

const DENIED: &str = "Permission denied: this session is read-only.";

fn allowed(call: &ToolCall<'_>) -> bool {
    let field = |name: &str| -> Option<String> {
        serde_json::from_str::<Value>(call.input.get()).ok()?.get(name)?.as_str().map(str::to_owned)
    };
    match call.name {
        "Write" | "Edit" | "NotebookEdit" => false,
        "Bash" | "Monitor" => field("command").is_some_and(|c| is_read_only_command(&c)),
        "Artifact" => field("action").is_some_and(|a| matches!(a.as_str(), "read" | "list")) && !call.writes_outside(),
        "ArtifactData" | "ArtifactComments" => {
            field("action").is_some_and(|a| matches!(a.as_str(), "get" | "list" | "query" | "read"))
                && !call.writes_outside()
        }
        _ => true,
    }
}

impl PermissionGate for ReadOnly {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let decision = if allowed(&call) { Permission::Allow } else { Permission::Deny(DENIED.to_owned()) };
        Box::pin(std::future::ready(decision))
    }

    /// A sandboxed command can still write inside the working directories.
    fn lets_sandboxed_commands_through(&self) -> bool {
        false
    }
}

/// Claude Code's `autoAllowBashIfSandboxed`: a Bash or Monitor call (Monitor
/// shares Bash's permission check) that runs in the sandbox is allowed without asking `inner`, unless it is one of Claude
/// Code's exceptions (see [`needs_permission_anyway`]); everything else,
/// including `dangerouslyDisableSandbox` and excluded commands, is asked.
pub(crate) struct SandboxAutoAllow {
    inner: Arc<dyn PermissionGate>,
    registry: Weak<Registry>,
    home: Option<PathBuf>,
}

impl SandboxAutoAllow {
    pub(crate) fn new(inner: Arc<dyn PermissionGate>, registry: Weak<Registry>) -> Self {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        Self { inner, registry, home }
    }

    fn auto_allows(&self, call: &ToolCall<'_>) -> bool {
        if !call.sandboxed || !matches!(call.name, "Bash" | "Monitor") {
            return false;
        }
        let Some(command) = serde_json::from_str::<Value>(call.input.get())
            .ok()
            .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_owned))
        else {
            return false;
        };
        let Some(blueprint) = self.registry.upgrade().and_then(|r| r.blueprint(call.agent_id)) else { return false };
        // The agent's own settings say whether sandboxed commands skip the gate.
        if !blueprint.sandbox.as_ref().is_some_and(|s| s.config().auto_allow_bash_if_sandboxed()) {
            return false;
        }
        // Claude Code checks removals against the shell's working directory
        // as it is now; without a workspace, the agent's start folder.
        match call.workspace {
            Some(w) => !needs_permission_anyway(&command, &w.cwd(), w.roots(), self.home.as_deref()),
            None => !needs_permission_anyway(&command, &blueprint.project.cwd, &blueprint.roots, self.home.as_deref()),
        }
    }
}

/// In front of every gate of the SDK, what follows from the agent tree: a
/// subagent's call of a main-agent-only host tool is refused before anyone
/// is asked (a fork keeps its parent's tool list, so it can try), and a
/// request that comes without its agent's origin (the sandbox's network
/// question, an artifact reaction) gets it, so the host's gate always knows
/// the root and parent of who asks.
pub(crate) struct TreeGate {
    pub(crate) inner: Arc<dyn PermissionGate>,
    pub(crate) registry: Weak<Registry>,
}

impl PermissionGate for TreeGate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let Some(registry) = self.registry.upgrade() else { return self.inner.decide(call) };
        if let Some(origin) = call.origin {
            let main_only = registry.blueprint(call.agent_id).is_some_and(|b| b.asked.is_main_only(call.name));
            if main_only {
                let refusal = format!("{} is only available to the main agent.", call.name);
                return Box::pin(std::future::ready(Permission::Deny(refusal)));
            }
            if let Some(refusal) = self.plan_refusal(origin, &call) {
                return Box::pin(std::future::ready(Permission::Deny(refusal)));
            }
            return self.inner.decide(call);
        }
        let Some(origin) = registry.origin(call.agent_id) else { return self.inner.decide(call) };
        if let Some(refusal) = self.plan_refusal(&origin, &call) {
            return Box::pin(std::future::ready(Permission::Deny(refusal)));
        }
        Box::pin(async move { self.inner.decide(ToolCall { origin: Some(&origin), ..call }).await })
    }

    fn lets_sandboxed_commands_through(&self) -> bool {
        self.inner.lets_sandboxed_commands_through()
    }

    fn applies_agent_modes(&self) -> bool {
        self.inner.applies_agent_modes()
    }
}

impl TreeGate {
    /// For a host gate that does not apply declared modes: a subagent whose
    /// chain declares `plan` may only look (as [`ReadOnly`] allows).
    fn plan_refusal(&self, origin: &AgentOrigin, call: &ToolCall<'_>) -> Option<String> {
        if self.inner.applies_agent_modes() || !always_plan(&origin.modes) || allowed(call) {
            return None;
        }
        Some(format!(
            "Permission denied: this agent runs in plan mode, so it may read and research but not change anything ({} was not run).",
            call.name
        ))
    }
}

impl PermissionGate for SandboxAutoAllow {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        if self.auto_allows(&call) {
            return Box::pin(std::future::ready(Permission::Allow));
        }
        self.inner.decide(call)
    }

    fn lets_sandboxed_commands_through(&self) -> bool {
        self.inner.lets_sandboxed_commands_through()
    }

    fn applies_agent_modes(&self) -> bool {
        self.inner.applies_agent_modes()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;

    use super::*;

    fn decide(name: &str, input: &str) -> bool {
        let input = RawValue::from_string(input.to_owned()).expect("json");
        allowed(&ToolCall {
            agent_id: "a",
            id: "t",
            name,
            input: &input,
            sandboxed: false,
            review: None,
            workspace: None,
            origin: None,
            writes: &[],
        })
    }

    #[test]
    fn denies_writers() {
        assert!(!decide("Write", r#"{"file_path":"/x","content":""}"#));
        assert!(decide("Read", r#"{"file_path":"/x"}"#));
        assert!(decide("Bash", r#"{"command":"ls -la"}"#));
        assert!(!decide("Bash", r#"{"command":"rm -rf x"}"#));
        assert!(decide("Artifact", r#"{"action":"list"}"#));
        assert!(!decide("Artifact", r#"{"file_path":"a.html"}"#));
    }
}
