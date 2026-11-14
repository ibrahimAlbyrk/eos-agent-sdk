//! How a host asks for an agent.

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::{CompactionText, Extensions, StoredMessage, TaskEvent, TokenSource};
use eos_agent_mcp::McpServerConfig;
use tokio::sync::mpsc;

use crate::auth::AuthSource;
use crate::child::SubagentEvent;
use crate::host::HostTool;

/// How to start one agent. Only the working directory is required.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub cwd: PathBuf,
    /// A model id or alias; the SDK's default when `None`.
    pub model: Option<String>,
    /// `low` … `max`; the model's default (as Claude Code sets it) when `None`.
    pub effort: Option<String>,
    /// The model's output limit (as Claude Code sends it) when `None`.
    pub max_tokens: Option<u32>,
    /// For when to summarize and the skill listing's budget; the model's
    /// window when `None`.
    pub context_window: Option<u64>,
    /// Stream readable summaries of the model's thinking (`false`: thinking
    /// stays hidden, as in Claude Code's `-p`, and costs nothing to send).
    pub thinking_summaries: bool,
    /// More directories the tools may touch.
    pub roots: Vec<PathBuf>,
    /// A fixed id (to resume); a fresh one when `None`.
    pub id: Option<String>,
    /// A recorded conversation to continue.
    pub history: Vec<StoredMessage>,
    /// Connect the MCP servers configured for `cwd` (Claude Code's user,
    /// project and local settings).
    pub mcp: bool,
    /// MCP servers of the host's choice (Claude Code's `--mcp-config`):
    /// connected beside the configured ones, before them when a name
    /// repeats; with [`mcp`](Self::mcp) off they are the agent's only
    /// servers (`--strict-mcp-config`).
    pub mcp_servers: Vec<McpServerConfig>,
    /// Rarely used tools wait behind ToolSearch on models that have it
    /// (`false`: every tool is sent in full and there is no ToolSearch),
    /// for this agent and its subagents.
    pub tool_search: bool,
    /// Summarize the conversation on its own: where Claude Code starts for
    /// the model (or at [`compact_at`](Self::compact_at)), and when a request
    /// no longer fits the model's window. `false`: only when the host calls
    /// [`SdkAgent::compact`](crate::SdkAgent::compact); a request that does
    /// not fit then fails the turn. Holds for the agent's subagents too.
    pub auto_compact: bool,
    /// Summarize once the live context reaches this many tokens, instead of
    /// where Claude Code starts for the model.
    pub compact_at: Option<u64>,
    /// The host's own words for summarizing (the instruction sent to the
    /// model and the message that replaces the conversation), for this agent
    /// and its subagents; Claude Code's when `None`.
    pub compaction_text: Option<Arc<dyn CompactionText>>,
    /// Put the CLAUDE.md files found for `cwd` in front of the first
    /// message, for this agent and its subagents.
    pub claude_md: bool,
    /// Plugin folders loaded as they are (Claude Code's `--plugin-dir`):
    /// their skills and commands join the ones found for `cwd`.
    pub plugin_dirs: Vec<PathBuf>,
    /// Claude Code's system prompt replaced or extended (see
    /// [`SystemPromptOverride`]).
    pub system_prompt: Option<SystemPromptOverride>,
    /// The only tools the agent (and its subagents) get; all when `None`.
    /// A trailing `*` matches by prefix (`mcp__github__*`).
    pub allowed_tools: Option<Vec<String>>,
    /// Tools taken away, matched like `allowed_tools`.
    pub disallowed_tools: Vec<String>,
    /// The host's own tools for this agent, beside the built-in ones (one
    /// named like a built-in tool takes its place); see [`HostTool`].
    /// `allowed_tools` and `disallowed_tools` apply to them too.
    pub tools: Vec<HostTool>,
    /// Values every agent of the tree (this agent, its subagents and forks)
    /// starts with in its [`Extensions`], shared between them: how a host
    /// tool finds the host's own state for the session through
    /// [`ToolContext::extensions`](eos_agent_core::ToolContext::extensions).
    /// Freed when the agent is dropped.
    pub extensions: Option<Arc<Extensions>>,
    /// Where the events of this agent's subagents (at any depth) go: each
    /// turn a subagent runs between a `Started` (what it is: its type, model
    /// and the call that started it) and an `Ended` (how it went, its final
    /// usage). The subagents wait when the receiver stops reading, as the
    /// agent itself does.
    pub subagent_events: Option<mpsc::Sender<SubagentEvent>>,
    /// Where the events of this agent's (and its subagents') background
    /// tasks go: a workflow run's start, progress and end (Claude Code's SDK
    /// `task_*` messages), and a backgrounded Bash command's or a Monitor's
    /// start, output growth and end.
    pub task_events: Option<mpsc::Sender<TaskEvent>>,
    /// The credential this agent and its subagents run on (one for the
    /// model's provider); the SDK's own when `None`. See
    /// [`SdkAgent::set_auth`](crate::SdkAgent::set_auth) to change it later.
    pub auth: Option<AuthSource>,
    /// Behave as Claude Code's interactive terminal where it differs from
    /// its `-p` and SDK sessions (the default, `false`): a `context: fork`
    /// skill, typed as `/name` or loaded through the Skill tool by this agent
    /// or its subagents, launches its subagent in the background, and the
    /// subagent's result arrives later as a task notification.
    pub interactive: bool,
}

/// A system prompt of the host's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemPromptOverride {
    /// Instead of Claude Code's.
    Replace(String),
    /// After Claude Code's.
    Append(String),
}

impl AgentOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            model: None,
            effort: None,
            max_tokens: None,
            context_window: None,
            thinking_summaries: true,
            roots: Vec::new(),
            id: None,
            history: Vec::new(),
            mcp: true,
            mcp_servers: Vec::new(),
            tool_search: true,
            auto_compact: true,
            compact_at: None,
            compaction_text: None,
            claude_md: true,
            plugin_dirs: Vec::new(),
            system_prompt: None,
            allowed_tools: None,
            disallowed_tools: Vec::new(),
            tools: Vec::new(),
            extensions: None,
            subagent_events: None,
            task_events: None,
            auth: None,
            interactive: false,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn context_window(mut self, tokens: u64) -> Self {
        self.context_window = Some(tokens);
        self
    }

    pub fn thinking_summaries(mut self, on: bool) -> Self {
        self.thinking_summaries = on;
        self
    }

    pub fn roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.roots = roots;
        self
    }

    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    pub fn history(mut self, history: Vec<StoredMessage>) -> Self {
        self.history = history;
        self
    }

    pub fn mcp(mut self, on: bool) -> Self {
        self.mcp = on;
        self
    }

    pub fn mcp_servers(mut self, servers: Vec<McpServerConfig>) -> Self {
        self.mcp_servers = servers;
        self
    }

    pub fn tool_search(mut self, on: bool) -> Self {
        self.tool_search = on;
        self
    }

    pub fn auto_compact(mut self, on: bool) -> Self {
        self.auto_compact = on;
        self
    }

    pub fn compact_at(mut self, tokens: u64) -> Self {
        self.compact_at = Some(tokens);
        self
    }

    pub fn compaction_text(mut self, text: Arc<dyn CompactionText>) -> Self {
        self.compaction_text = Some(text);
        self
    }

    pub fn claude_md(mut self, on: bool) -> Self {
        self.claude_md = on;
        self
    }

    pub fn plugin_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.plugin_dirs = dirs;
        self
    }

    pub fn system_prompt(mut self, prompt: SystemPromptOverride) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    pub fn allowed_tools(mut self, names: Vec<String>) -> Self {
        self.allowed_tools = Some(names);
        self
    }

    pub fn disallowed_tools(mut self, names: Vec<String>) -> Self {
        self.disallowed_tools = names;
        self
    }

    pub fn tools(mut self, tools: Vec<HostTool>) -> Self {
        self.tools = tools;
        self
    }

    pub fn extensions(mut self, seed: Extensions) -> Self {
        self.extensions = Some(Arc::new(seed));
        self
    }

    pub fn subagent_events(mut self, to: mpsc::Sender<SubagentEvent>) -> Self {
        self.subagent_events = Some(to);
        self
    }

    pub fn task_events(mut self, to: mpsc::Sender<TaskEvent>) -> Self {
        self.task_events = Some(to);
        self
    }

    pub fn auth(mut self, source: Arc<dyn TokenSource>) -> Self {
        self.auth = Some(AuthSource(source));
        self
    }

    pub fn interactive(mut self, on: bool) -> Self {
        self.interactive = on;
        self
    }
}
