use std::sync::{Mutex, PoisonError};

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::nudge;

const RESULT: &str = "Todos have been modified successfully. Ensure that you continue to use the todo list to track \
your progress. Please proceed with the current tasks if applicable";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Todo {
    pub content: String,
    pub status: TodoStatus,
    pub active_form: String,
}

/// An agent's todo list, kept in its extensions. In memory only, as in
/// Claude Code; a list whose items are all completed is cleared.
#[derive(Debug, Default)]
pub struct TodoList(Mutex<Vec<Todo>>);

impl TodoList {
    pub fn items(&self) -> Vec<Todo> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn replace(&self, todos: Vec<Todo>) {
        let keep = !todos.iter().all(|t| t.status == TodoStatus::Completed);
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = if keep { todos } else { Vec::new() };
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    todos: Vec<Todo>,
}

/// Claude Code's `TodoWrite`: the model sends the whole list each call.
#[derive(Debug)]
pub struct TodoWrite {
    spec: ToolSpec,
}

impl Default for TodoWrite {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoWrite {
    pub fn new() -> Self {
        Self {
            spec: crate::spec(
                "TodoWrite",
                include_str!("../prompts/TodoWrite.md"),
                include_str!("../prompts/TodoWrite.schema.json"),
            )
            .with_search_hint("manage the session task checklist"),
        }
    }

    /// Sent with `defer_loading`, as Claude Code does when tool search is on.
    pub fn deferred(mut self) -> Self {
        self.spec = self.spec.deferred();
        self
    }
}

impl Tool for TodoWrite {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        let out = match crate::parse::<Input>(input) {
            Err(e) => e,
            Ok(Input { todos }) => match todos.iter().find_map(empty_field) {
                Some(problem) => {
                    ToolOutput::error(format!("<tool_use_error>InputValidationError: {problem}</tool_use_error>"))
                }
                None => {
                    ctx.extensions.get_or_default::<TodoList>().replace(todos);
                    nudge::mark_used(ctx.extensions);
                    ToolOutput::text(RESULT)
                }
            },
        };
        Box::pin(std::future::ready(out))
    }
}

fn empty_field(todo: &Todo) -> Option<&'static str> {
    if todo.content.is_empty() {
        Some("Content cannot be empty")
    } else if todo.active_form.is_empty() {
        Some("Active form cannot be empty")
    } else {
        None
    }
}
