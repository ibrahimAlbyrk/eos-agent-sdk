//! The agent loop and the ports (traits) it runs on.
//!
//! The core does no I/O and owns no runtime: the model, tools, permissions,
//! time, storage and event delivery all come in through traits, so the loop is
//! tested with fakes and any host can supply its own adapters.

mod agent;
mod assembler;
mod auth;
mod context;
mod extensions;
mod message;
mod model;
mod modes;
mod ports;
mod retry;
mod spill;
mod subagents;
mod tasks;
mod tool;
mod wake;
mod workspace;

pub use agent::{
    Agent, AgentConfig, Deps, FailedServer, INLINE_TOOLS_BETA, MAX_TURNS, MID_CONVERSATION_TOOL_CHANGES_BETA,
    RewindError, ServerInstructions, ServerNotes, ToolChangeRefusals, ToolChanges, ToolSource, TurnError, TurnOutcome,
    check_cut, instructions_section, server_paragraphs,
};
pub use assembler::{Assembled, Assembler, Emitted};
pub use auth::{Credential, RequestAuth, SecretStore, StaticCredential, TokenSource};
pub use context::{Compaction, CompactionText, ReminderSource, SessionContext};
pub use extensions::Extensions;
pub use message::{CacheTtl, MessageError, StoredMessage, SystemPrompt};
pub use model::{EventStream, LiveRequest, ModelClient, ModelError, ModelRequest, RateLimitInfo, Thinking};
pub use modes::{DeclaredMode, PermissionMode, always_plan, resolve_chain, resolve_mode};
pub use ports::{
    AgentEvent, AllowAll, BlockKind, Clock, EventSink, Permission, PermissionGate, ResponseUsage, ToolCall,
    TranscriptStore,
};
pub use retry::RetryPolicy;
pub use spill::{PREVIEW_CHARS, format_size, persisted_output};
pub use subagents::{
    AgentOrigin, StopOrigin, StopRefused, SubagentInfo, SubagentObserver, SubagentPort, SubagentReport,
    SubagentRequest, SubagentStart, SubagentSystem, Subagents, TaskStopped, WorkflowTag, Worktree,
};
pub use tasks::{BackgroundTask, BackgroundTasks, TaskEvent, TaskEventKind, TaskEvents, TaskStatus, TaskUsage};
pub use tool::{Tool, ToolContext, ToolOutput, ToolReview, ToolSpec, Toolbox};
pub use wake::{TaskNotices, Wake};
pub use workspace::{
    AgentIdent, FileStamp, FileStates, InternalPaths, Reminders, Workspace, WriteTarget, content_hash,
};

pub use tokio_util::sync::CancellationToken;

/// A boxed, sendable future — the return type of every async port method.
pub type BoxFuture<'a, T> = futures_util::future::BoxFuture<'a, T>;
