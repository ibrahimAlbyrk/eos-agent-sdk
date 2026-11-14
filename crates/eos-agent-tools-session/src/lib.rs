//! Claude Code's tools that act on the agent's own session: TodoWrite, the
//! Task tools, ToolSearch, StructuredOutput and AskUserQuestion, plus the
//! reminder that nudges an agent back to its task list.
//!
//! Tools are stateless and shared by every agent; per-agent state (the todo
//! list, the task list id, the structured result) lives in the agent's
//! extensions.

mod ask;
mod nudge;
mod schema;
mod search;
mod structured;
mod tasks;
mod todo;

use eos_agent_core::{ToolOutput, ToolSpec};
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

pub use ask::{
    Annotation, AskUserQuestion, NoUser, Question, QuestionOption, REJECTED, REJECTED_SAYING, Refusal, Reply,
    UserPrompter,
};
pub use nudge::{NudgeKind, TrackingNudge};
pub use search::{ConnectingServers, DeferredTool, McpConnecting, PendingServer, ToolSearch};
pub use structured::{StructuredOutput, StructuredResult};
pub use tasks::{LockedList, Task, TaskCreate, TaskGet, TaskList, TaskListId, TaskStatus, TaskStore, TaskUpdate};
pub use todo::{Todo, TodoList, TodoStatus, TodoWrite};

fn spec(name: &str, description: &str, schema: &str) -> ToolSpec {
    ToolSpec::new(name, description, serde_json::from_str(schema).expect("bundled schemas are valid JSON"))
}

fn parse<T: DeserializeOwned>(input: &RawValue) -> Result<T, ToolOutput> {
    serde_json::from_str(input.get())
        .map_err(|e| ToolOutput::error(format!("<tool_use_error>InputValidationError: {e}</tool_use_error>")))
}

/// Claude Code's wrapper for harness notes.
fn system_reminder(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}
