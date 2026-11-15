//! A batteries-included composition of the Eos agent crates: shared services
//! once per process, Claude Code's tools, prompts, skills, MCP servers and
//! subagents per agent.
//!
//! ```ignore
//! let sdk = Sdk::builder().auth(token_source).build().await?;
//! let (agent, events) = sdk.spawn(AgentOptions::new(cwd).model("claude-opus-5-5")).await?;
//! agent.send_text("…").await;
//! ```
//!
//! A host can give an agent its own tools and session state
//! ([`AgentOptions::tools`], [`AgentOptions::extensions`]), choose its MCP
//! servers, tool search, summarizing and instruction files per agent, decide
//! every tool call ([`SdkBuilder::permissions`]), change a live agent
//! ([`SdkAgent::set_model`], [`SdkAgent::rewind`], [`Sdk::stop_task`]) and
//! ask a model once outside any conversation ([`Sdk::complete`]); see
//! `docs/host-api.md`.
//!
//! Per process: one API client (one connection pool), the engine, the shell
//! config (snapshot and process budget), the MCP hub, the WebFetch cache, the
//! task store, the prompts and the system prompt, Claude Code's global config.
//! Per working directory (read once, re-read when a file it came from
//! changes): CLAUDE.md files, skills, agent types, MCP configs. Per agent: its MCP servers, its session context, its config;
//! agents with equal tool sets share one serialized toolbox.

mod agent;
mod auth;
mod child;
mod complete;
mod context;
mod control;
mod decorate;
mod gate;
mod git;
mod host;
mod inline;
mod models;
mod options;
mod profile;
mod project;
mod registry;
mod routing;
mod sandbox;
mod sdk;
mod services;
mod shared;
mod side;
mod spawn;
mod stamps;
mod subagents;
mod toolbox;
mod watch;
mod wording;
mod workflows;
mod worktree;

pub use agent::{CommandInfo, InputError, SdkAgent};
pub use auth::{Account, AuthFailover, AuthFailure, AuthSource};
pub use child::{SubagentEnded, SubagentEvent, SubagentEventKind, SubagentStarted};
pub use complete::{CompleteError, CompleteRequest, Completion};
pub use control::{ControlError, ModelChange, StopTaskError, StoppedTask};
pub use decorate::{Deferred, SpillOver, result_limit};
pub use eos_agent_agents::SpawnLimits;
pub use eos_agent_artifacts::{ArtifactOpener, FrameConfig, OpenReason, OpenRequest, WatchConfig};
pub use eos_agent_core::{TaskEvent, TaskEventKind, TaskUsage};
pub use eos_agent_mcp::{
    AuthPrompter, AuthRequest, BrowserOpener, McpConfig, McpCredentials, McpServerConfig, McpServerState,
    McpServerStatus, McpTransport, PrintPrompter, STEP_UP_CHOICES, StepUpAnswer, StepUpChoice, StepUpRequest,
};
/// The context window of an OpenAI model the account's catalog gives none for.
pub use eos_agent_openai::models::DEFAULT_CONTEXT_WINDOW as OPENAI_CONTEXT_WINDOW;
pub use eos_agent_openai::{Catalog as OpenAiCatalog, ChatGptLogin, ModelInfo as OpenAiModel, ToolPlacement};
pub use eos_agent_skills::Origin as SkillOrigin;
pub use eos_agent_tools_shell::sandbox::{
    FilesystemConfig, NETWORK_TOOL, NetworkConfig, SandboxConfig, SandboxUnavailable,
};
pub use gate::ReadOnly;
pub use host::HostTool;
pub use models::resolve_model;
pub use options::{AgentOptions, SystemPromptOverride};
pub use profile::Provider;
pub use registry::AgentInfo;
pub use sdk::{Sdk, SdkBuilder, SdkError, claude_mcp_credentials};
pub use side::SideError;
