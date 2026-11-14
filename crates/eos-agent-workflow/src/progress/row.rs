//! One `workflow_agent` row and the key orders of the literals Claude Code
//! builds them with.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Start,
    Progress,
    Done,
    Error,
}

/// Which of Claude Code's row literals a row is built like: each writes
/// its keys in its own order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shape {
    /// An attempt's rows (start, progress, done, error), also the queued row.
    #[default]
    Attempt,
    /// The call failed before an attempt started.
    Refused,
    /// A result replayed from the journal on resume.
    Cached,
}

/// One `workflow_agent` row. Unset fields are left out.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRow {
    pub shape: Shape,
    pub index: u32,
    pub label: String,
    pub phase_index: Option<u32>,
    pub phase_title: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub isolation: Option<String>,
    pub model: Option<String>,
    pub state: Option<AgentState>,
    pub started_at: Option<u64>,
    pub queued_at: Option<u64>,
    pub attempt: Option<u32>,
    pub last_attempt_reason: Option<String>,
    pub last_tool_name: Option<String>,
    pub last_tool_summary: Option<String>,
    pub prompt_preview: Option<String>,
    pub last_progress_at: Option<u64>,
    pub tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub duration_ms: Option<u64>,
    pub result_preview: Option<String>,
    pub error: Option<String>,
    pub cached: Option<bool>,
    pub skipped: Option<bool>,
    pub blocked: Option<bool>,
    pub ended_with_run: Option<String>,
}

/// Claude Code's key orders. An attempt's literal ends with the fields its
/// caller adds (`error`, `skipped`, `tokens`, …); a row settled with the run
/// gets `error` and `endedWithRun` appended.
const ATTEMPT: [&str; 25] = [
    "index",
    "label",
    "phaseIndex",
    "phaseTitle",
    "agentId",
    "agentType",
    "isolation",
    "model",
    "state",
    "startedAt",
    "queuedAt",
    "attempt",
    "lastAttemptReason",
    "lastToolName",
    "lastToolSummary",
    "promptPreview",
    "lastProgressAt",
    "error",
    "skipped",
    "tokens",
    "toolCalls",
    "durationMs",
    "resultPreview",
    "cached",
    "blocked",
];
const REFUSED: [&str; 25] = [
    "index",
    "label",
    "phaseIndex",
    "phaseTitle",
    "agentId",
    "agentType",
    "isolation",
    "model",
    "state",
    "error",
    "startedAt",
    "queuedAt",
    "attempt",
    "lastAttemptReason",
    "lastToolName",
    "lastToolSummary",
    "promptPreview",
    "lastProgressAt",
    "skipped",
    "tokens",
    "toolCalls",
    "durationMs",
    "resultPreview",
    "cached",
    "blocked",
];
const CACHED: [&str; 25] = [
    "index",
    "label",
    "phaseIndex",
    "phaseTitle",
    "agentId",
    "agentType",
    "isolation",
    "model",
    "state",
    "startedAt",
    "lastProgressAt",
    "cached",
    "resultPreview",
    "promptPreview",
    "queuedAt",
    "attempt",
    "lastAttemptReason",
    "lastToolName",
    "lastToolSummary",
    "error",
    "skipped",
    "tokens",
    "toolCalls",
    "durationMs",
    "blocked",
];

impl AgentRow {
    fn put<M: SerializeMap>(&self, map: &mut M, key: &'static str) -> Result<(), M::Error> {
        fn opt<M: SerializeMap, T: Serialize>(map: &mut M, key: &str, v: Option<&T>) -> Result<(), M::Error> {
            v.map_or(Ok(()), |v| map.serialize_entry(key, v))
        }
        match key {
            "index" => map.serialize_entry(key, &self.index),
            "label" => map.serialize_entry(key, &self.label),
            "phaseIndex" => opt(map, key, self.phase_index.as_ref()),
            "phaseTitle" => opt(map, key, self.phase_title.as_ref()),
            "agentId" => opt(map, key, self.agent_id.as_ref()),
            "agentType" => opt(map, key, self.agent_type.as_ref()),
            "isolation" => opt(map, key, self.isolation.as_ref()),
            "model" => opt(map, key, self.model.as_ref()),
            "state" => opt(map, key, self.state.as_ref()),
            "startedAt" => opt(map, key, self.started_at.as_ref()),
            "queuedAt" => opt(map, key, self.queued_at.as_ref()),
            "attempt" => opt(map, key, self.attempt.as_ref()),
            "lastAttemptReason" => opt(map, key, self.last_attempt_reason.as_ref()),
            "lastToolName" => opt(map, key, self.last_tool_name.as_ref()),
            "lastToolSummary" => opt(map, key, self.last_tool_summary.as_ref()),
            "promptPreview" => opt(map, key, self.prompt_preview.as_ref()),
            "lastProgressAt" => opt(map, key, self.last_progress_at.as_ref()),
            "tokens" => opt(map, key, self.tokens.as_ref()),
            "toolCalls" => opt(map, key, self.tool_calls.as_ref()),
            "durationMs" => opt(map, key, self.duration_ms.as_ref()),
            "resultPreview" => opt(map, key, self.result_preview.as_ref()),
            "error" => opt(map, key, self.error.as_ref()),
            "cached" => opt(map, key, self.cached.as_ref()),
            "skipped" => opt(map, key, self.skipped.as_ref()),
            "blocked" => opt(map, key, self.blocked.as_ref()),
            "endedWithRun" => opt(map, key, self.ended_with_run.as_ref()),
            _ => Ok(()),
        }
    }
}

impl Serialize for AgentRow {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let order = match self.shape {
            Shape::Attempt => &ATTEMPT,
            Shape::Refused => &REFUSED,
            Shape::Cached => &CACHED,
        };
        let settled = self.ended_with_run.is_some();
        let mut map = serializer.serialize_map(None)?;
        for key in order.iter().filter(|k| !(settled && **k == "error")) {
            self.put(&mut map, key)?;
        }
        if settled {
            self.put(&mut map, "error")?;
            self.put(&mut map, "endedWithRun")?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_shape_keeps_claude_codes_key_order() {
        let queued = AgentRow {
            index: 2,
            label: "review:bugs".into(),
            phase_index: Some(1),
            phase_title: Some("Review".into()),
            state: Some(AgentState::Start),
            queued_at: Some(5),
            prompt_preview: Some("look".into()),
            last_progress_at: Some(5),
            ..AgentRow::default()
        };
        assert_eq!(
            serde_json::to_string(&queued).expect("json"),
            r#"{"index":2,"label":"review:bugs","phaseIndex":1,"phaseTitle":"Review","state":"start","queuedAt":5,"promptPreview":"look","lastProgressAt":5}"#
        );
        let refused = AgentRow {
            shape: Shape::Refused,
            index: 3,
            label: "x".into(),
            model: Some("m".into()),
            state: Some(AgentState::Error),
            error: Some("bad".into()),
            queued_at: Some(5),
            last_progress_at: Some(6),
            ..AgentRow::default()
        };
        assert_eq!(
            serde_json::to_string(&refused).expect("json"),
            r#"{"index":3,"label":"x","model":"m","state":"error","error":"bad","queuedAt":5,"lastProgressAt":6}"#
        );
        let cached = AgentRow {
            shape: Shape::Cached,
            index: 4,
            label: "y".into(),
            agent_id: Some("a1".into()),
            state: Some(AgentState::Done),
            started_at: Some(7),
            last_progress_at: Some(7),
            cached: Some(true),
            result_preview: Some("r".into()),
            prompt_preview: Some("p".into()),
            ..AgentRow::default()
        };
        assert_eq!(
            serde_json::to_string(&cached).expect("json"),
            r#"{"index":4,"label":"y","agentId":"a1","state":"done","startedAt":7,"lastProgressAt":7,"cached":true,"resultPreview":"r","promptPreview":"p"}"#
        );
    }

    #[test]
    fn every_order_names_every_key() {
        fn sorted(keys: [&'static str; 25]) -> Vec<&'static str> {
            let mut keys = keys.to_vec();
            keys.sort_unstable();
            keys
        }
        assert_eq!(sorted(ATTEMPT), sorted(REFUSED));
        assert_eq!(sorted(ATTEMPT), sorted(CACHED));
        let mut all = sorted(ATTEMPT);
        all.dedup();
        assert_eq!(all.len(), 25);
    }
}
