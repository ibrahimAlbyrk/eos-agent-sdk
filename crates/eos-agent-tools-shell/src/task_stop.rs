//! The TaskStop tool.

use eos_agent_core::{BackgroundTasks, BoxFuture, TaskStatus, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::common::{invalid_input, refused, spec};
use crate::tasks::{ShellTasks, Status};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    task_id: Option<String>,
    /// The old name of `task_id`.
    shell_id: Option<String>,
}

/// Claude Code's result, fields in its order.
#[derive(Serialize)]
struct Stopped<'a> {
    message: String,
    task_id: &'a str,
    task_type: &'a str,
    command: &'a str,
}

/// Stops one of the agent's background commands or monitors, or another
/// background task of its tools (a workflow run).
#[derive(Debug)]
pub struct TaskStop {
    spec: ToolSpec,
}

impl TaskStop {
    pub(crate) fn new() -> Self {
        Self {
            spec: spec(
                "TaskStop",
                include_str!("../prompts/TaskStop.md"),
                include_str!("../prompts/TaskStop.schema.json"),
            ),
        }
    }
}

impl Tool for TaskStop {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let input: Input = match serde_json::from_str(input.get()) {
                Ok(input) => input,
                Err(e) => return invalid_input(&e),
            };
            let Some(id) = input.task_id.or(input.shell_id).filter(|id| !id.is_empty()) else {
                return refused("Missing required parameter: task_id");
            };
            let Some(task) = ctx.extensions.get::<ShellTasks>().and_then(|tasks| tasks.get(&id)) else {
                return stop_other(ctx, &id).await;
            };
            let status = task.status();
            if status != Status::Running {
                return refused(format_args!("Task {id} is not running (status: {})", status.as_str()));
            }
            task.stop().await;
            let result = Stopped {
                message: format!("Successfully stopped task: {id} ({})", task.command),
                task_id: &id,
                task_type: "local_bash",
                command: &task.command,
            };
            ToolOutput::text(serde_json::to_string(&result).unwrap_or_default())
        })
    }
}

/// A background task that is not a shell command (a workflow run).
async fn stop_other(ctx: ToolContext<'_>, id: &str) -> ToolOutput {
    let Some(task) = ctx.extensions.get::<BackgroundTasks>().and_then(|tasks| tasks.get(id)) else {
        return refused(format_args!("No task found with ID: {id}"));
    };
    let status = task.status();
    if status != TaskStatus::Running {
        return refused(format_args!("Task {id} is not running (status: {})", status.as_str()));
    }
    task.stop().await;
    let result = Stopped {
        message: format!("Successfully stopped task: {id} ({})", task.description),
        task_id: id,
        task_type: &task.task_type,
        command: &task.description,
    };
    ToolOutput::text(serde_json::to_string(&result).unwrap_or_default())
}
