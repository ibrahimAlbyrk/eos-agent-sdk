//! Claude Code's Workflow tool: a model-written JavaScript script that
//! orchestrates subagents deterministically, run in an embedded QuickJS realm
//! that exists only while the run does.
//!
//! The tool reaches agents only through the core's
//! [`Subagents`](eos_agent_core::Subagents) port: the host starts each
//! `agent()` call as a subagent of the calling agent (type
//! [`WORKFLOW_SUBAGENT`]), hears its progress through an observer, and
//! delivers the run's `<task-notification>` and task events.

mod config;
mod deny;
pub mod engine;
pub mod journal;
pub mod lint;
pub mod meta;
mod named;
pub mod notify;
pub mod progress;
mod run;
pub mod schema;
mod store;
mod tool;

pub use config::{MAX_AGENTS, STALL, SizeGuideline, TokenBudgets, WorkflowConfig, default_concurrency};
pub use run::WORKFLOW_SUBAGENT;
pub use store::SessionDir;
pub use tool::{AUTHORING, NAME, SKILL, SKILL_DESCRIPTION, WorkflowTool, description};
