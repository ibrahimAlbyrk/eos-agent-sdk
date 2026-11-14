//! The progress pump: rows batched every 16 ms into task events.

use std::sync::{Arc, Weak};
use std::time::Instant;

use eos_agent_core::{TaskEventKind, TaskUsage};
use tokio::sync::mpsc as tmpsc;

use super::agent::elapsed_ms;
use super::{BATCH, LOGS_KEPT, Pump, Shared, TICKS_EVERY};
use crate::progress::{Progress, Row};

pub(super) async fn pump(shared: Weak<Shared>, mut rx: tmpsc::UnboundedReceiver<Pump>, started: Instant) {
    let mut progress = Progress::default();
    let mut logs: Vec<String> = Vec::new();
    let mut batch: Vec<Row> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut last_full: Option<Instant> = None;
    loop {
        let flush_due = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(Pump::Row(row)) => {
                    if deadline.is_none() {
                        deadline = Some(tokio::time::Instant::now() + BATCH);
                    }
                    batch.push(row);
                }
                Some(Pump::Finish(reply)) => {
                    flush(&shared, &mut progress, &mut logs, &mut batch, &mut last_full, started).await;
                    let _ = reply.send((progress, logs));
                    return;
                }
                None => return,
            },
            () = flush_due => {
                deadline = None;
                flush(&shared, &mut progress, &mut logs, &mut batch, &mut last_full, started).await;
            }
        }
    }
}

async fn flush(
    shared: &Weak<Shared>,
    progress: &mut Progress,
    logs: &mut Vec<String>,
    batch: &mut Vec<Row>,
    last_full: &mut Option<Instant>,
    started: Instant,
) {
    if batch.is_empty() {
        return;
    }
    let rows = std::mem::take(batch);
    for row in &rows {
        if let Row::Log { message } = row
            && logs.len() < LOGS_KEPT
        {
            logs.push(message.clone());
        }
    }
    let visible: Vec<&Row> = rows.iter().filter(|r| !matches!(r, Row::Log { .. })).collect();
    let last_agent = visible.iter().rev().find_map(|r| match r {
        Row::Agent(a) => Some(a.as_ref().clone()),
        _ => None,
    });
    let only_ticks = !visible.is_empty() && visible.iter().all(|r| r.is_tick());
    let has_visible = !visible.is_empty();
    progress.merge(rows);
    let Some(shared) = shared.upgrade() else { return };
    if !has_visible {
        return;
    }
    let full = !only_ticks || last_full.is_none_or(|t| t.elapsed() >= TICKS_EVERY);
    if full {
        *last_full = Some(Instant::now());
    }
    let (tokens, tools) = progress.totals();
    let description = last_agent.as_ref().map_or_else(
        || shared.spec.meta.description.clone(),
        |a| a.phase_title.as_ref().map_or_else(|| a.label.clone(), |p| format!("{p}: {}", a.label)),
    );
    let kind = TaskEventKind::Progress {
        description,
        usage: TaskUsage { total_tokens: tokens, tool_uses: tools, duration_ms: elapsed_ms(started) },
        last_tool_name: last_agent.map(|a| a.label),
        summary: Some(shared.spec.meta.description.clone()),
        workflow_progress: full.then(|| Arc::from(progress.json())),
    };
    shared.task_event(kind).await;
}
