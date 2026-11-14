//! The Monitor tool: a background command whose stdout lines become events.

mod batch;
mod run;

use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{BoxFuture, Extensions, TaskNotices, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::Shell;
use crate::bash::DOMAINS_NEED_SANDBOX;
use crate::common::{invalid_input, refused, sandboxed, tasks_dir, workspace};
use crate::process::{Launch, Sink, append_file};
use crate::tasks::{ShellTasks, Task, new_task_id};
use run::MonitorRun;

const MIN_TIMEOUT_MS: f64 = 1000.0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    description: String,
    timeout_ms: Option<f64>,
    persistent: Option<bool>,
    command: Option<String>,
    ws: Option<serde_json::Value>,
    /// Claude Code's auto-mode host list; outside auto mode it is ignored.
    allowed_domains: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CommandOnly {
    command: Option<String>,
}

/// Starts a monitor; events reach the model through its reminders.
#[derive(Debug)]
pub struct Monitor {
    spec: ToolSpec,
    shell: Arc<Shell>,
}

impl Monitor {
    pub(crate) fn new(shell: Arc<Shell>) -> Self {
        let schema: serde_json::Value = serde_json::from_str(include_str!("../../prompts/Monitor.schema.json"))
            .expect("bundled schemas are valid JSON");
        let spec = ToolSpec::new("Monitor", include_str!("../../prompts/Monitor.md"), schema);
        Self { spec, shell }
    }
}

impl Tool for Monitor {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        serde_json::from_str::<CommandOnly>(input.get())
            .is_ok_and(|i| i.command.is_some_and(|c| sandboxed(extensions, &c, false)))
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let input: Input = match serde_json::from_str(input.get()) {
                Ok(input) => input,
                Err(e) => return invalid_input(&e),
            };
            self.start(input, ctx).await.unwrap_or_else(|e| e)
        })
    }
}

impl Monitor {
    async fn start(&self, input: Input, ctx: ToolContext<'_>) -> Result<ToolOutput, ToolOutput> {
        let command = match (input.command, input.ws) {
            (Some(command), None) => command,
            (None, Some(_)) => return Err(refused("WebSocket monitors (ws) are not supported here; use command")),
            _ => return Err(refused("InputValidationError: exactly one of command or ws")),
        };
        let limits = &self.shell.config.limits;
        let persistent = input.persistent == Some(true);
        let timeout_ms = input.timeout_ms.unwrap_or(limits.monitor_default.as_secs_f64() * 1000.0);
        if timeout_ms.is_nan() || timeout_ms < MIN_TIMEOUT_MS {
            return Err(refused("InputValidationError: timeout_ms must be >= 1000"));
        }
        let timeout = Duration::try_from_secs_f64(timeout_ms / 1000.0).unwrap_or(Duration::MAX);
        if !persistent && timeout > limits.monitor_max {
            return Err(refused(format_args!(
                "InputValidationError: timeout_ms must be \u{2264} {}",
                limits.monitor_max.as_millis()
            )));
        }

        let sandboxed = sandboxed(ctx.extensions, &command, false);
        if input.allowed_domains.as_ref().is_some_and(|d| !d.is_empty()) && !sandboxed {
            return Err(refused(DOMAINS_NEED_SANDBOX));
        }
        let workspace = workspace(&ctx)?;
        let dir = tasks_dir(&workspace).await?;
        let id = new_task_id();
        let output_file = dir.join(format!("{id}.output"));
        let snapshot = self.shell.snapshot().await;
        let prepared = self.shell.sandboxing(&command, sandboxed, &ctx, &workspace, &[]).await?;
        let permit = self.shell.permit().await?;
        let cwd = workspace.cwd();
        let launch = Launch {
            shell: &self.shell.path,
            snapshot: snapshot.as_deref(),
            command: &command,
            cwd: &cwd,
            cwd_file: None,
            sandbox: prepared.as_ref().map(|p| (&p.wrapped, &p.env)),
        };
        let child = append_file(&output_file)
            .and_then(|file| launch.spawn(Sink::PipeAndFile(file)))
            .map_err(|e| ToolOutput::error(format!("Error: could not start {}: {e}", self.shell.path.display())))?;

        let task = Arc::new(Task::new(id.clone(), command, output_file, child.id().unwrap_or(0), &ctx));
        ctx.extensions.get_or_default::<ShellTasks>().insert(Arc::clone(&task));
        MonitorRun {
            child,
            task,
            tool_use_id: ctx.tool_use_id.to_owned(),
            description: input.description,
            timeout: (!persistent).then_some(timeout),
            notices: TaskNotices::of(ctx.extensions),
            sandbox: prepared.map(|p| p.guard),
        }
        .spawn(permit);

        let lifetime = if persistent {
            "persistent \u{2014} runs until TaskStop or session end".to_owned()
        } else {
            format!("timeout {}ms", timeout.as_millis())
        };
        Ok(ToolOutput::text(format!(
            "Monitor started (task {id}, {lifetime}). You will be notified on each event. Keep working \u{2014} do not \
             poll or sleep. Events may arrive while you are waiting for the user \u{2014} an event is not their reply."
        )))
    }
}
