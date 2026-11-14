//! A run's progress in Claude Code's `workflow_progress` row shape — what
//! its SDK puts in `system/task_progress` and its run record, and what Eos's
//! `applyWorkflowProgress` reads — merged by `(type, index)` as its task
//! registry merges them.

mod row;

use serde::Serialize;
use serde_json::value::RawValue;

pub use row::{AgentRow, AgentState, Shape};

/// Claude Code keeps 500 log rows once there are over 1000.
const LOGS_KEPT: usize = 500;
/// Previews are cut to this many characters (plus an ellipsis).
const PREVIEW: usize = 400;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type")]
pub enum Row {
    #[serde(rename = "workflow_phase")]
    Phase {
        index: u32,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    #[serde(rename = "workflow_agent")]
    Agent(Box<AgentRow>),
    #[serde(rename = "workflow_log")]
    Log { message: String },
}

impl Row {
    fn slot(&self) -> Option<(u8, u32)> {
        match self {
            Self::Phase { index, .. } => Some((0, *index)),
            Self::Agent(a) => Some((1, a.index)),
            Self::Log { .. } => None,
        }
    }

    /// Only an agent's tool tick (sent at most every 10 s on its own).
    pub fn is_tick(&self) -> bool {
        matches!(self, Self::Agent(a) if a.state == Some(AgentState::Progress))
    }
}

/// Claude Code's `rn`: trimmed, cut to 400 characters plus an ellipsis.
pub fn preview(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.chars().count() > PREVIEW {
        Some(format!("{}\u{2026}", t.chars().take(PREVIEW).collect::<String>()))
    } else {
        Some(t.to_owned())
    }
}

/// The merged list of a run.
#[derive(Debug, Default, Clone)]
pub struct Progress {
    rows: Vec<Row>,
    /// Indexes of agents that ever started (they settle with the run).
    started: Vec<u32>,
}

impl Progress {
    pub fn merge(&mut self, batch: impl IntoIterator<Item = Row>) {
        let mut logged = false;
        for row in batch {
            if let Row::Agent(a) = &row
                && started(a)
                && !self.started.contains(&a.index)
            {
                self.started.push(a.index);
            }
            match row.slot() {
                Some(slot) => match self.rows.iter().position(|r| r.slot() == Some(slot)) {
                    Some(i) => self.rows[i] = row,
                    None => self.rows.push(row),
                },
                None => {
                    self.rows.push(row);
                    logged = true;
                }
            }
        }
        if logged && self.rows.len() > LOGS_KEPT * 2 {
            let mut drop = self.rows.len() - LOGS_KEPT;
            self.rows.retain(|r| {
                if drop > 0 && matches!(r, Row::Log { .. }) {
                    drop -= 1;
                    return false;
                }
                true
            });
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The rows hosts get (Claude Code leaves the log rows out of SDK events
    /// and the run record).
    pub fn without_logs(&self) -> Vec<&Row> {
        self.rows.iter().filter(|r| !matches!(r, Row::Log { .. })).collect()
    }

    /// The rows as JSON text, each with its keys in Claude Code's order.
    pub fn json(&self) -> Box<RawValue> {
        serde_json::value::to_raw_value(&self.without_logs()).unwrap_or_default()
    }

    /// Totals over the agent rows: tokens and tool calls.
    pub fn totals(&self) -> (u64, u64) {
        self.rows.iter().fold((0, 0), |(t, c), r| match r {
            Row::Agent(a) => (t + a.tokens.unwrap_or(0), c + a.tool_calls.unwrap_or(0)),
            _ => (t, c),
        })
    }

    /// Claude Code's `M`: agents still open when the run ends are settled as
    /// errors that ended with it.
    pub fn settle(&mut self, status: &str) {
        let error = match status {
            "killed" => "Stopped because the workflow was stopped",
            "failed" => "Stopped because the workflow failed",
            _ => "Stopped because the workflow finished without waiting for it",
        };
        let ended = if status == "killed" { "stopped" } else { status };
        for row in &mut self.rows {
            if let Row::Agent(a) = row
                && matches!(a.state, Some(AgentState::Start | AgentState::Progress))
                && (started(a) || self.started.contains(&a.index))
            {
                a.state = Some(AgentState::Error);
                a.error = Some(error.to_owned());
                a.ended_with_run = Some(ended.to_owned());
            }
        }
    }

    /// `(done, error, skipped, empty result)` agent counts for the notification.
    pub fn counts(&self) -> (u32, u32, u32, u32) {
        let mut out = (0, 0, 0, 0);
        for row in &self.rows {
            let Row::Agent(a) = row else { continue };
            match a.state {
                Some(AgentState::Done) => {
                    out.0 += 1;
                    if a.result_preview.as_deref().is_none_or(empty_result) {
                        out.3 += 1;
                    }
                }
                Some(AgentState::Error) if a.skipped == Some(true) => out.2 += 1,
                Some(AgentState::Error) => out.1 += 1,
                _ => {}
            }
        }
        out
    }
}

/// Claude Code's `aSn`: the agent got past the queue.
fn started(a: &AgentRow) -> bool {
    a.started_at.is_some() || a.queued_at.is_none() || a.tokens.unwrap_or(0) > 0
}

/// `[]`, `{}` or `{"key": []}`.
fn empty_result(preview: &str) -> bool {
    let p: String = preview.chars().filter(|c| !c.is_whitespace()).collect();
    if p == "[]" || p == "{}" {
        return true;
    }
    p.starts_with("{\"") && p.ends_with(":[]}") && p[2..p.len() - 4].ends_with('"') && !p[2..p.len() - 5].contains('"')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(index: u32, state: AgentState) -> Row {
        Row::Agent(Box::new(AgentRow { index, label: format!("a{index}"), state: Some(state), ..AgentRow::default() }))
    }

    #[test]
    fn rows_serialize_as_claude_code_sends_them() {
        let row = Row::Agent(Box::new(AgentRow {
            index: 2,
            label: "review:bugs".into(),
            phase_index: Some(1),
            phase_title: Some("Review".into()),
            state: Some(AgentState::Start),
            queued_at: Some(5),
            prompt_preview: preview("  look  "),
            last_progress_at: Some(5),
            ..AgentRow::default()
        }));
        assert_eq!(
            serde_json::to_string(&row).expect("json"),
            r#"{"type":"workflow_agent","index":2,"label":"review:bugs","phaseIndex":1,"phaseTitle":"Review","state":"start","queuedAt":5,"promptPreview":"look","lastProgressAt":5}"#
        );
        let mut p = Progress::default();
        p.merge([Row::Agent(Box::new(AgentRow {
            index: 5,
            label: "z".into(),
            state: Some(AgentState::Progress),
            started_at: Some(1),
            attempt: Some(1),
            tokens: Some(2),
            tool_calls: Some(0),
            ..AgentRow::default()
        }))]);
        p.settle("failed");
        assert_eq!(
            p.json().get(),
            r#"[{"type":"workflow_agent","index":5,"label":"z","state":"error","startedAt":1,"attempt":1,"tokens":2,"toolCalls":0,"error":"Stopped because the workflow failed","endedWithRun":"failed"}]"#
        );
        let phase = Row::Phase { index: 1, title: "Review".into(), kind: None };
        assert_eq!(
            serde_json::to_string(&phase).expect("json"),
            r#"{"type":"workflow_phase","index":1,"title":"Review"}"#
        );
    }

    #[test]
    fn merges_by_index_trims_logs_and_settles() {
        let mut p = Progress::default();
        p.merge([agent(1, AgentState::Start), Row::Phase { index: 1, title: "A".into(), kind: None }]);
        p.merge([agent(1, AgentState::Progress), agent(2, AgentState::Done)]);
        assert_eq!(p.rows().len(), 3);
        p.merge((0..1001).map(|i| Row::Log { message: i.to_string() }));
        assert_eq!(p.rows().iter().filter(|r| matches!(r, Row::Log { .. })).count(), 497);
        assert_eq!(p.without_logs().len(), 3);
        p.settle("killed");
        let Row::Agent(a) = &p.rows()[0] else { panic!("agent") };
        assert_eq!((a.state, a.ended_with_run.as_deref()), (Some(AgentState::Error), Some("stopped")));
        assert_eq!(p.counts(), (1, 1, 0, 1));
        assert!(preview(&"x".repeat(500)).expect("p").ends_with('\u{2026}'));
        assert!(empty_result("{\"findings\": []}") && empty_result("[ ]") && !empty_result("{\"a\":[1]}"));
    }
}
