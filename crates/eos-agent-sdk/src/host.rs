//! Tools of the host's own, given to one agent when it is spawned.

use std::sync::Arc;

use eos_agent_core::Tool;

/// What a host tool's text result is cut to before it is saved to a file
/// (Claude Code's limit for its own tools).
pub(crate) const HOST_RESULT_LIMIT: usize = 100_000;

/// A tool the host implements, for [`AgentOptions::tools`](crate::AgentOptions::tools).
///
/// It runs through the same pipeline as the built-in tools: the permission
/// gate decides every call, a long text result is saved to a file with a
/// preview, and its calls and results are in the agent's events. Its
/// [`ToolSpec`](eos_agent_core::ToolSpec) says how it is sent: as it is, or,
/// marked [`deferred`](eos_agent_core::ToolSpec::deferred) (with a
/// [search hint](eos_agent_core::ToolSpec::with_search_hint)), behind
/// ToolSearch like an MCP tool without `alwaysLoad`.
///
/// Subagents get it by the rule for every tool (an agent type that takes all
/// tools, or names it), unless it is [main-agent-only](Self::main_agent_only).
/// Agents given the same `Arc`s share one serialized tool list.
#[derive(Clone)]
pub struct HostTool {
    pub(crate) tool: Arc<dyn Tool>,
    pub(crate) main_agent_only: bool,
}

impl HostTool {
    pub fn new(tool: Arc<dyn Tool>) -> Self {
        Self { tool, main_agent_only: false }
    }

    /// Only the agent it was given to may call it: subagents start without
    /// it, and a fork (which keeps its parent's tool list) is refused.
    pub fn main_agent_only(mut self) -> Self {
        self.main_agent_only = true;
        self
    }

    pub fn name(&self) -> &str {
        self.tool.spec().name()
    }
}

impl std::fmt::Debug for HostTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostTool").field("name", &self.name()).field("main_agent_only", &self.main_agent_only).finish()
    }
}
