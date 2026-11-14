//! Follows one attempt of an `agent()` call while it runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use eos_agent_core::{AgentEvent, SubagentObserver};
use eos_agent_types::Role;
use serde_json::Value;

use super::call::{AgentCall, collapse};
use super::{STRUCTURED_RETRIES, Shared, epoch_ms, lock};
use crate::journal::Entry;
use crate::progress::{AgentRow, AgentState, Row, preview};

/// Follows one attempt of a workflow agent: its progress rows, its journal
/// `started` line, and when it last did anything (the stall watchdog).
#[derive(Debug)]
pub(super) struct Watch {
    shared: Arc<Shared>,
    call: AgentCall,
    /// The label its rows show (a retry's names the retry).
    label: String,
    attempt: u32,
    reason: Option<&'static str>,
    started_at: u64,
    carried: (u64, u64, u64),
    inner: Mutex<WatchState>,
    activity: tokio::sync::Notify,
    cap: tokio::sync::Notify,
}

#[derive(Debug)]
struct WatchState {
    agent_id: Option<String>,
    model: Option<String>,
    last: Instant,
    tokens: u64,
    /// The last response's output tokens.
    output_tokens: Option<u64>,
    /// Whether the turn ended on a stated stop reason.
    stated: bool,
    tool_calls: u64,
    last_tool: Option<(String, Option<String>)>,
    pending_structured: Vec<String>,
    structured_failures: u32,
    last_structured_error: Option<String>,
    structured_ok: bool,
}

impl Watch {
    pub(super) fn new(
        shared: &Arc<Shared>,
        call: &AgentCall,
        label: String,
        attempt: u32,
        reason: Option<&'static str>,
        started_at: u64,
        carried: (u64, u64, u64),
    ) -> Self {
        Self {
            shared: Arc::clone(shared),
            call: call.clone(),
            label,
            attempt,
            reason,
            started_at,
            carried,
            inner: Mutex::new(WatchState {
                agent_id: None,
                model: call.model.clone(),
                last: Instant::now(),
                tokens: 0,
                output_tokens: None,
                stated: true,
                tool_calls: 0,
                last_tool: None,
                pending_structured: Vec::new(),
                structured_failures: 0,
                last_structured_error: None,
                structured_ok: false,
            }),
            activity: tokio::sync::Notify::new(),
            cap: tokio::sync::Notify::new(),
        }
    }

    pub(super) fn counts(&self) -> (u64, u64) {
        let s = lock(&self.inner);
        (s.tokens, s.tool_calls)
    }

    /// Claude Code's throttle signs: the reply had no stop reason, and how
    /// many output tokens its response had.
    pub(super) fn reply_signs(&self) -> (bool, Option<u64>) {
        let s = lock(&self.inner);
        (s.stated, s.output_tokens)
    }

    pub(super) fn agent_id(&self) -> Option<String> {
        lock(&self.inner).agent_id.clone()
    }

    pub(super) fn row(&self, state: AgentState, extra: impl FnOnce(&mut AgentRow)) {
        let s = lock(&self.inner);
        let mut row = AgentRow {
            index: self.call.index,
            label: self.label.clone(),
            phase_index: self.call.phase_index,
            phase_title: self.call.phase_title.clone(),
            agent_id: s.agent_id.clone(),
            agent_type: self.call.opts.agent_type.clone(),
            isolation: self.call.opts.worktree.then(|| "worktree".to_owned()),
            model: s.model.clone(),
            state: Some(state),
            started_at: Some(self.started_at),
            queued_at: Some(self.call.queued_at),
            attempt: Some(self.attempt),
            last_attempt_reason: self.reason.map(str::to_owned),
            last_tool_name: s.last_tool.as_ref().map(|t| t.0.clone()),
            last_tool_summary: s.last_tool.as_ref().and_then(|t| t.1.clone()),
            prompt_preview: preview(&self.call.prompt),
            last_progress_at: Some(epoch_ms()),
            ..AgentRow::default()
        };
        let (tokens, tools) = (self.carried.0 + s.tokens, self.carried.1 + s.tool_calls);
        if tokens > 0 || tools > 0 || state != AgentState::Start {
            row.tokens = Some(tokens);
            row.tool_calls = Some(tools);
        }
        drop(s);
        extra(&mut row);
        self.shared.row(Row::Agent(Box::new(row)));
    }

    pub(super) fn finish_row(
        &self,
        state: AgentState,
        error: Option<String>,
        result: Option<String>,
        duration_ms: u64,
    ) {
        self.row(state, |row| {
            row.error = error;
            row.result_preview = result;
            row.duration_ms = Some(duration_ms);
        });
    }

    /// Resolves once nothing happened for `window` (never without one).
    pub(super) async fn stalled(&self, window: Option<Duration>) {
        let Some(window) = window else { return std::future::pending().await };
        loop {
            let last = lock(&self.inner).last;
            let deadline = last + window;
            if Instant::now() >= deadline {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {}
                () = self.activity.notified() => {}
            }
        }
    }

    /// Resolves when StructuredOutput failed too often to go on.
    pub(super) async fn gave_up(&self) {
        self.cap.notified().await;
    }

    pub(super) fn structured_cap(&self) -> Option<String> {
        let s = lock(&self.inner);
        if s.structured_ok || s.structured_failures < STRUCTURED_RETRIES {
            return None;
        }
        let last = s
            .last_structured_error
            .as_ref()
            .map(|e| format!(" \u{2014} last StructuredOutput error: {e}"))
            .unwrap_or_default();
        Some(format!(
            "agent({{schema}}): StructuredOutput retry cap ({STRUCTURED_RETRIES}) exceeded \u{2014} {} failed {} with no \
             valid output{last}",
            s.structured_failures,
            if s.structured_failures == 1 { "call" } else { "calls" }
        ))
    }
}

impl SubagentObserver for Watch {
    fn started(&self, agent_id: &str, model: &str) {
        {
            let mut s = lock(&self.inner);
            s.agent_id = Some(agent_id.to_owned());
            *lock(&self.call.agent_id) = Some(agent_id.to_owned());
            s.model = Some(model.to_owned());
            s.last = Instant::now();
        }
        let _ = self.shared.journal.append(&Entry::Started {
            key: self.call.key.clone(),
            agent_id: agent_id.to_owned(),
            label: Some(self.call.label.clone()),
            phase: self.call.phase_title.clone(),
        });
        self.row(AgentState::Start, |_| {});
    }

    fn event(&self, _agent_id: &str, event: &AgentEvent) {
        let mut tick = false;
        {
            let mut s = lock(&self.inner);
            s.last = Instant::now();
            match event {
                AgentEvent::ToolUse { id, name, input } => {
                    s.tool_calls += 1;
                    s.last_tool = Some((name.clone(), tool_summary(name, input.get())));
                    if name == "StructuredOutput" {
                        s.pending_structured.push(id.clone());
                    }
                    tick = true;
                }
                AgentEvent::ToolResult { id, output } => {
                    if let Some(i) = s.pending_structured.iter().position(|p| p == id) {
                        s.pending_structured.remove(i);
                        if output.is_error {
                            s.structured_failures += 1;
                            s.last_structured_error = Some(match &output.content {
                                eos_agent_types::ToolResultContent::Text(t) => t.clone(),
                                eos_agent_types::ToolResultContent::Blocks(_) => "invalid output".into(),
                            });
                        } else {
                            s.structured_ok = true;
                        }
                    }
                }
                AgentEvent::Usage(u) => {
                    let u = &u.usage;
                    s.tokens =
                        [u.input_tokens, u.cache_creation_input_tokens, u.cache_read_input_tokens, u.output_tokens]
                            .into_iter()
                            .flatten()
                            .sum();
                    s.output_tokens = u.output_tokens;
                }
                AgentEvent::TurnEnded { stated, .. } => s.stated = *stated,
                AgentEvent::Message(m) if m.role() == Role::Assistant => tick = true,
                _ => {}
            }
        }
        self.activity.notify_waiters();
        if self.structured_cap().is_some() {
            self.cap.notify_waiters();
        }
        if tick {
            self.row(AgentState::Progress, |_| {});
        }
    }
}

/// A short summary of a tool call for the progress row.
pub(super) fn tool_summary(name: &str, input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    let key = match name {
        "Bash" => "command",
        "Read" | "Edit" | "Write" | "NotebookEdit" => "file_path",
        "Grep" | "Glob" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Agent" | "Task" => "description",
        _ => ["command", "file_path", "path", "pattern", "url", "query", "description"]
            .into_iter()
            .find(|k| v[*k].is_string())?,
    };
    let text = collapse(v[key].as_str()?);
    (!text.is_empty()).then(|| {
        if text.chars().count() > 80 { format!("{}\u{2026}", text.chars().take(79).collect::<String>()) } else { text }
    })
}
