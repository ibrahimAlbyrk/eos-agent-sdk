//! A skill's inline `` !`cmd` `` commands, run as Claude Code runs them: as
//! a Bash call of the agent loading the skill, through the same permission
//! check as the model's own calls; denied, the skill does not load.

use std::sync::{Arc, Weak};

use eos_agent_core::{BoxFuture, Permission, PermissionGate, Tool, ToolCall, ToolContext};
use eos_agent_skills::{CommandRunner, ShellFailure};
use eos_agent_types::{ContentBlock, ToolResultContent};
use serde_json::value::RawValue;

/// The shell tool and the gate inline commands go through.
#[derive(Clone)]
pub(crate) struct Shell {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) gate: Arc<dyn PermissionGate>,
}

#[derive(Debug)]
pub(crate) struct ShellRunner {
    shell: Weak<ShellCell>,
}

/// Set once the SDK is built (the tools exist before the services do).
#[derive(Default)]
pub(crate) struct ShellCell(std::sync::OnceLock<Shell>);

impl std::fmt::Debug for ShellCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellCell").field("set", &self.0.get().is_some()).finish()
    }
}

impl ShellCell {
    pub(crate) fn set(&self, shell: Shell) {
        let _ = self.0.set(shell);
    }

    pub(crate) fn runner(self: &Arc<Self>) -> ShellRunner {
        ShellRunner { shell: Arc::downgrade(self) }
    }
}

impl CommandRunner for ShellRunner {
    fn run<'a>(
        &'a self,
        command: &'a str,
        call: Option<ToolContext<'a>>,
    ) -> BoxFuture<'a, Result<String, ShellFailure>> {
        Box::pin(async move {
            let unavailable = || ShellFailure::Failed("inline shell commands are not available in this session".into());
            let cell = self.shell.upgrade().ok_or_else(unavailable)?;
            let shell = cell.0.get().ok_or_else(unavailable)?;
            let ctx = call.ok_or_else(unavailable)?;
            let input = serde_json::json!({ "command": command }).to_string();
            let input = RawValue::from_string(input).map_err(|e| ShellFailure::Failed(e.to_string()))?;
            let id = format!("{}-inline", ctx.tool_use_id);
            let workspace = ctx.extensions.get::<eos_agent_core::Workspace>();
            let origin = ctx.extensions.get::<eos_agent_core::AgentOrigin>();
            let request = ToolCall {
                agent_id: ctx.agent_id,
                id: &id,
                name: shell.tool.spec().name(),
                input: &input,
                sandboxed: shell.tool.runs_sandboxed(&input, ctx.extensions),
                review: None,
                workspace: workspace.as_deref(),
                origin: origin.as_deref(),
                writes: &[],
            };
            let input = match shell.gate.decide(request).await {
                Permission::Allow => input,
                Permission::AllowWith(updated) => {
                    RawValue::from_string(updated.get().to_owned()).map_err(|e| ShellFailure::Failed(e.to_string()))?
                }
                Permission::Deny(reason) => {
                    let reason = if reason.is_empty() { "Permission denied".to_owned() } else { reason };
                    return Err(ShellFailure::Denied(reason));
                }
            };
            let out = shell.tool.call(&input, ToolContext { tool_use_id: &id, ..ctx }).await;
            let text = match out.content {
                ToolResultContent::Text(text) => text,
                ToolResultContent::Blocks(blocks) => blocks
                    .into_iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            if out.is_error { Err(ShellFailure::Failed(text)) } else { Ok(text) }
        })
    }
}
