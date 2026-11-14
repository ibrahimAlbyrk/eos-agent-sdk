use std::sync::Arc;
use std::time::Duration;

use eos_agent_types::{StopReason, Usage};
use serde_json::value::RawValue;

use crate::BoxFuture;
use crate::message::StoredMessage;
use crate::model::RateLimitInfo;
use crate::subagents::AgentOrigin;
use crate::tool::{ToolOutput, ToolReview};
use crate::workspace::{Workspace, WriteTarget};

/// What the loop tells the host while it runs.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A model request is going out (attempt 1 = first try).
    RequestStarted {
        attempt: u32,
    },
    /// The previous attempt failed mid-stream: drop its partial deltas.
    StreamRestarted,
    /// A content block of the response opens. Blocks never overlap: every
    /// delta that follows belongs to this block until its `BlockStop`.
    BlockStart {
        index: u32,
        block: BlockKind,
    },
    /// The block is complete (a tool call's `ToolUse` follows at once).
    BlockStop {
        index: u32,
    },
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    /// Raw JSON fragment of a tool's input while the model writes it (the
    /// tool's name came with the block's `BlockStart`).
    ToolInputDelta {
        id: String,
        partial_json: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Arc<RawValue>,
    },
    ToolResult {
        id: String,
        output: Arc<ToolOutput>,
    },
    /// A message was appended to the conversation.
    Message(StoredMessage),
    // Boxed: every agent's event channel holds slots the size of the
    // largest variant, so rare large ones stay out of line.
    /// One response's token usage, when the response is complete.
    Usage(Box<ResponseUsage>),
    RateLimit(Box<RateLimitInfo>),
    Retrying {
        attempt: u32,
        delay: Duration,
        error: String,
    },
    TurnEnded {
        stop_reason: StopReason,
        /// `false` when the last response gave no stop reason and the turn
        /// ended as `end_turn` (Claude Code's workflow runner retries such a
        /// reply when it is short).
        stated: bool,
    },
    Interrupted,
    /// The conversation is being summarized. `Compacted` follows when the
    /// summary replaced it; a summary that fails or is interrupted ends with
    /// the error or interruption of the turn (or `compact` call) that ran it.
    Compacting {
        before_tokens: u64,
    },
    /// The conversation was replaced by its summary.
    Compacted {
        before_tokens: u64,
    },
}

/// What kind of content block opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Thinking,
    /// A call of one of the agent's tools; its input streams as `ToolInputDelta`s.
    ToolUse {
        id: String,
        name: String,
    },
    /// A call the API runs itself (web search); no input deltas are passed on.
    ServerToolUse {
        id: String,
        name: String,
    },
    /// Any other block, by its wire `type` (`redacted_thinking`, a server tool's result, …).
    Other(String),
}

/// The usage of one response and which model produced it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseUsage {
    pub usage: Usage,
    /// The model that answered, as the API named it (the requested model
    /// when the provider did not say).
    pub model: String,
}

/// Receives events. Awaiting `emit` is the backpressure: a host that stops
/// reading pauses the agent instead of letting events pile up in memory.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'_, ()>;
}

/// One tool call awaiting permission.
#[derive(Debug, Clone)]
pub struct ToolCall<'a> {
    pub agent_id: &'a str,
    pub id: &'a str,
    pub name: &'a str,
    /// The input with paths made absolute where the tool says so; see
    /// [`Tool::observable_input`](crate::Tool::observable_input).
    pub input: &'a RawValue,
    /// The call will run inside the OS sandbox (a shell command under
    /// Claude Code's `sandbox` settings); see [`Tool::runs_sandboxed`](crate::Tool::runs_sandboxed).
    pub sandboxed: bool,
    /// What the tool asks the gate to review; see [`Tool::review`](crate::Tool::review).
    pub review: Option<&'a ToolReview>,
    /// The calling agent's workspace (its working directory as it is now,
    /// after the commands' `cd`s), when it has one.
    pub workspace: Option<&'a Workspace>,
    /// Who started the calling agent: `None` for a top-level agent, set for
    /// a subagent (a workflow's agents included).
    pub origin: Option<&'a AgentOrigin>,
    /// The files and folders the call would create or change, absolute, each
    /// marked inside or outside the agent's working directories; see
    /// [`Tool::written_paths`](crate::Tool::written_paths). Empty for a call
    /// that writes no path it names (a shell command included).
    pub writes: &'a [WriteTarget],
}

impl ToolCall<'_> {
    /// The top-level agent of the calling agent's tree (itself, when no
    /// other agent started it).
    pub fn root_id(&self) -> &str {
        self.origin.map_or(self.agent_id, |o| &o.root_id)
    }

    /// The call would write somewhere outside the agent's working directories.
    pub fn writes_outside(&self) -> bool {
        self.writes.iter().any(|w| !w.inside)
    }

    /// The call comes from a workflow run's agent, or from an agent one of
    /// those started.
    pub fn in_workflow(&self) -> bool {
        self.origin.is_some_and(|o| o.workflow.is_some())
    }
}

#[derive(Debug, Clone)]
pub enum Permission {
    Allow,
    /// Allowed, run with this input instead of the model's (Claude Code's
    /// `updatedInput`). The conversation keeps the call as the model wrote it.
    AllowWith(Arc<RawValue>),
    /// The message goes back to the model as the tool's error result.
    Deny(String),
}

impl Permission {
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Self::Deny(_))
    }
}

/// The host's say over every tool call.
pub trait PermissionGate: Send + Sync {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission>;

    /// May sandboxed shell commands be allowed without asking this gate
    /// (Claude Code's `autoAllowBashIfSandboxed`)? A gate whose rule is
    /// stricter than the sandbox (one that keeps a session read-only) says no.
    fn lets_sandboxed_commands_through(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl PermissionGate for AllowAll {
    fn decide<'a>(&'a self, _call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        Box::pin(async { Permission::Allow })
    }
}

/// Time, so tests control it.
pub trait Clock: Send + Sync {
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()>;
}

/// Durable record of a conversation, one message at a time.
pub trait TranscriptStore: Send + Sync {
    /// Records one message; an assistant message comes with the usage of the
    /// response that produced it (kept beside the message, never sent back
    /// to the model).
    fn append<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a StoredMessage,
        usage: Option<&'a Usage>,
    ) -> BoxFuture<'a, std::io::Result<()>>;

    /// The conversation now consists of `messages` only (after a summary).
    fn replace<'a>(&'a self, agent_id: &'a str, messages: &'a [StoredMessage]) -> BoxFuture<'a, std::io::Result<()>>;

    /// The recorded conversation (empty when there is none), for resuming an
    /// agent or paging back a history that was dropped from memory.
    fn load<'a>(&'a self, agent_id: &'a str) -> BoxFuture<'a, std::io::Result<Vec<StoredMessage>>>;
}
