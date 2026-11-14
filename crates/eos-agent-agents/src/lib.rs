//! Claude Code's subagent tools — Agent, SendMessage, ListAgents — over the
//! core's [`Subagents`](eos_agent_core::Subagents) port, plus agent
//! definitions: the Markdown agent-file parser, discovery with Claude Code's
//! precedence, the built-in types, and what a host needs to spawn one.

mod agent_tool;
mod builtin;
mod definition;
mod discovery;
mod fork;
mod frontmatter;
mod listing;
mod messaging;
mod notification;
mod registry;
mod spawn;
mod structured;

pub use agent_tool::{AgentTool, DEFAULT_MAX_DEPTH, SpawnLimits};
pub use builtin::{
    EXPLORE, FORK, GENERAL_PURPOSE, PLAN, SearchTools, WORKFLOW_SUBAGENT, builtin_definitions, fork_definition,
    workflow_subagent_definition,
};
pub use definition::{AgentDefinition, Source};
pub use discovery::{Discovered, agent_roots, discover};
pub use fork::{ForkedAgent, fork_directive, fork_seed};
pub use messaging::{ListAgentsTool, SendMessageTool};
pub use notification::{AgentNotification, Outcome, StoppedBy};
pub use registry::AgentDefinitions;
pub use spawn::SpawnPlan;
pub use structured::{STRUCTURED_OUTPUT, STRUCTURED_OUTPUT_NUDGE, StructuredOutput, validate as validate_output};

use eos_agent_core::{ToolOutput, ToolSpec};

fn spec(name: &str, prompt: &str, schema: &str) -> ToolSpec {
    let schema = serde_json::from_str(schema).expect("bundled schemas are valid JSON");
    ToolSpec::new(name, prompt, schema)
}

/// A refused call, worded as Claude Code words tool errors.
fn refused(message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

fn invalid_input(err: &serde_json::Error) -> ToolOutput {
    refused(format_args!("InputValidationError: {err}"))
}
