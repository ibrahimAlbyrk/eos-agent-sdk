//! The TaskStop tool.

use eos_agent_core::{
    BackgroundTasks, BoxFuture, StopOrigin, SubagentPort, TaskStatus, Tool, ToolContext, ToolOutput, ToolSpec,
};
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

/// Stops a background task: a command or monitor, a workflow run, or a
/// subagent (by agent id or by the name it was started with). Where the
/// agent's host stops tasks ([`SubagentPort`]), it decides what the agent
/// may stop; elsewhere only the agent's own commands and workflow runs.
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
            if let Some(port) = ctx.extensions.get::<SubagentPort>()
                && let Some(outcome) = port.0.stop_task(ctx.agent_id, &id, StopOrigin::Agent).await
            {
                return match outcome {
                    Ok(stopped) => stopped_output(&stopped.task_id, &stopped.task_type, &stopped.description),
                    Err(refusal) => refused(refusal),
                };
            }
            let Some(task) = ctx.extensions.get::<ShellTasks>().and_then(|tasks| tasks.get(&id)) else {
                return stop_other(ctx, &id).await;
            };
            let status = task.status();
            if status != Status::Running {
                return refused(format_args!("Task {id} is not running (status: {})", status.as_str()));
            }
            task.stop().await;
            stopped_output(&id, "local_bash", &task.command)
        })
    }
}

fn stopped_output(id: &str, task_type: &str, command: &str) -> ToolOutput {
    let result =
        Stopped { message: format!("Successfully stopped task: {id} ({command})"), task_id: id, task_type, command };
    ToolOutput::text(serde_json::to_string(&result).unwrap_or_default())
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
    stopped_output(id, &task.task_type, &task.description)
}
