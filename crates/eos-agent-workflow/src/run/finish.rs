//! How a run ends: the record, the output file, the `<task-notification>`
//! and the last task events.

use std::sync::Arc;
use std::time::Instant;

use eos_agent_core::{BackgroundTask, TaskEventKind, TaskStatus, TaskUsage};
use serde_json::Value;
use tokio::sync::oneshot;

use super::agent::elapsed_ms;
use super::{End, Pump, Shared, epoch_ms, lock};
use crate::notify::{Notification, Status};
use crate::progress::Progress;
use crate::store::write_private;

pub(super) async fn finish(
    shared: Arc<Shared>,
    end: End,
    task: Arc<BackgroundTask>,
    started: Instant,
    started_ms: u64,
) {
    shared.cancel.cancel();
    for (_, timer) in lock(&shared.state).timers.drain() {
        timer.abort();
    }
    let (reply, settled) = oneshot::channel();
    let _ = shared.rows.send(Pump::Finish(reply));
    let (mut progress, logs) = settled.await.unwrap_or_default();
    let (status, result, error) = match end {
        End::Completed(result) => (Status::Completed, result, None),
        End::Failed(error) => (Status::Failed, None, Some(error)),
        End::Killed => (Status::Killed, None, None),
    };
    progress.settle(status.as_str());
    let (tokens, tools) = progress.totals();
    let duration_ms = elapsed_ms(started);
    let (agent_count, failures) = {
        let state = lock(&shared.state);
        (state.agents, state.failures.clone())
    };
    let spec = &shared.spec;
    let usage = TaskUsage { total_tokens: tokens, tool_uses: tools, duration_ms };
    shared
        .task_event(TaskEventKind::Progress {
            description: spec.meta.description.clone(),
            usage,
            last_tool_name: None,
            summary: Some(spec.meta.description.clone()),
            workflow_progress: Some(Arc::from(progress.json())),
        })
        .await;

    let record = record(
        &shared,
        status,
        result.as_deref(),
        error.as_deref(),
        &logs,
        &progress,
        agent_count,
        duration_ms,
        started_ms,
    );
    let _ = write_private(&spec.dir.record(&spec.run_id), &record);
    let output_file = spec.dir.output(&spec.task_id);
    if status == Status::Completed {
        let output = format!(
            "{{\"summary\":{},\"agentCount\":{agent_count},\"logs\":{},\"result\":{},\"workflowProgress\":{},\
             \"totalTokens\":{tokens},\"totalToolCalls\":{tools}}}",
            Value::String(spec.meta.description.clone()),
            Value::from(logs.clone()),
            result.as_deref().unwrap_or("null"),
            progress.json().get()
        );
        let _ = write_private(&output_file, &output);
    }
    let name = &spec.meta.description;
    let summary = match (status, &error) {
        (Status::Completed, _) => format!("Dynamic workflow \"{name}\" completed"),
        (Status::Failed, e) => {
            format!("Dynamic workflow \"{name}\" failed: {}", e.as_deref().unwrap_or("Unknown error"))
        }
        (Status::Killed, _) => format!("Dynamic workflow \"{name}\" was stopped"),
    };
    // The notification is queued before the run counts as ended, so a host
    // never sees the agent idle with nothing running in between.
    if status != Status::Killed {
        let transcripts = spec.dir.transcripts(&spec.run_id);
        let note = Notification {
            task_id: &spec.task_id,
            tool_use_id: &spec.tool_use_id,
            output_file: &output_file,
            status,
            summary: &spec.meta.description,
            error: error.as_deref(),
            result: result.as_deref(),
            failures: &failures,
            agent_count,
            counts: progress.counts(),
            total_tokens: tokens,
            tool_uses: tools,
            duration_ms,
            transcript_dir: &transcripts,
            script_path: &spec.script_path,
            run_id: &spec.run_id,
            args: spec.args.as_ref(),
        };
        let _ = shared.subagents.notify(&spec.session, note.render(), true).await;
    }
    shared
        .task_event(TaskEventKind::Ended {
            status: match status {
                Status::Killed => "stopped".into(),
                other => other.as_str().into(),
            },
            summary,
            output_file: Some(output_file.display().to_string()),
            usage,
            workflow_progress: Some(Arc::from(progress.json())),
            exit_code: None,
        })
        .await;
    task.finish(match status {
        Status::Completed => TaskStatus::Completed,
        Status::Failed => TaskStatus::Failed,
        Status::Killed => TaskStatus::Killed,
    });
}

/// `<session>/workflows/<runId>.json`, as Claude Code writes it at the end.
#[expect(clippy::too_many_arguments, reason = "the record's fields")]
fn record(
    shared: &Shared,
    status: Status,
    result: Option<&str>,
    error: Option<&str>,
    logs: &[String],
    progress: &Progress,
    agent_count: u32,
    duration_ms: u64,
    started_ms: u64,
) -> String {
    let spec = &shared.spec;
    let (tokens, tools) = progress.totals();
    let mut fields: Vec<(&str, String)> = vec![
        ("runId", Value::String(spec.run_id.clone()).to_string()),
        ("timestamp", Value::String(iso_time(epoch_ms())).to_string()),
        ("taskId", Value::String(spec.task_id.clone()).to_string()),
        ("script", Value::String(spec.script.clone()).to_string()),
        ("scriptPath", Value::String(spec.script_path.display().to_string()).to_string()),
    ];
    if let Some(args) = &spec.args {
        fields.push(("args", args.to_string()));
    }
    // `undefined` (no result, or a failed run) leaves the key out, as JSON.stringify does.
    if let Some(result) = result {
        fields.push(("result", result.to_owned()));
    }
    fields.push(("agentCount", agent_count.to_string()));
    fields.push(("logs", Value::from(logs.to_vec()).to_string()));
    fields.push(("durationMs", duration_ms.to_string()));
    if let Some(e) = error {
        fields.push(("error", Value::String(e.to_owned()).to_string()));
    }
    fields.push(("summary", Value::String(spec.meta.description.clone()).to_string()));
    fields.push(("workflowName", Value::String(spec.meta.name.clone()).to_string()));
    if let Some(title) = &spec.meta.title {
        fields.push(("title", Value::String(title.clone()).to_string()));
    }
    fields.push(("status", Value::String(status.as_str().into()).to_string()));
    fields.push(("startTime", started_ms.to_string()));
    if let Some(phases) = &spec.meta.phases {
        fields.push(("phases", serde_json::to_string(phases).unwrap_or_else(|_| "[]".into())));
    }
    if let Some(model) = &spec.default_model {
        fields.push(("defaultModel", Value::String(model.clone()).to_string()));
    }
    fields.push(("workflowProgress", progress.json().get().to_owned()));
    fields.push(("totalTokens", tokens.to_string()));
    fields.push(("totalToolCalls", tools.to_string()));
    let body: Vec<String> = fields.into_iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{{}}}", body.join(","))
}

/// RFC 3339 UTC with milliseconds, as JavaScript's `toISOString`.
pub(crate) fn iso_time(ms: u64) -> String {
    let secs = ms / 1000;
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}
