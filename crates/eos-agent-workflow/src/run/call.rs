//! `agent(prompt, opts)`: its options, its journal key, a cached result
//! on resume, and the task that runs it.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::agent::agent_task;
use super::hooks::lane_of;
use super::{Event, Shared, epoch_ms, lock};
use crate::engine::HostError;
use crate::journal;
use crate::progress::{AgentRow, AgentState, Row, Shape, preview};

/// The options of one `agent()` call that change what runs.
#[derive(Debug, Clone, Default)]
pub(super) struct AgentOpts {
    pub(super) schema: Option<Value>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<String>,
    pub(super) worktree: bool,
    pub(super) agent_type: Option<String>,
    pub(super) disallowed: Vec<String>,
    /// `Number(opts.stallMs)`, when given.
    pub(super) stall_ms: Option<f64>,
}

#[derive(Debug, Clone)]
pub(super) struct AgentCall {
    pub(super) realm: u32,
    pub(super) call: u64,
    pub(super) index: u32,
    pub(super) prompt: String,
    pub(super) opts: AgentOpts,
    pub(super) label: String,
    pub(super) phase_title: Option<String>,
    pub(super) phase_index: Option<u32>,
    pub(super) key: String,
    pub(super) lane: usize,
    /// The key already has a journal line: once this call has a result, its
    /// lane goes on from a new hash (Claude Code's `gn`).
    pub(super) rekey: bool,
    pub(super) queued_at: u64,
    pub(super) model: Option<String>,
    /// The agent id of the attempt that started last (Claude Code's `Ee`).
    pub(super) agent_id: Arc<Mutex<Option<String>>>,
}

/// A string-list option; `null` is refused like any other non-array.
fn string_list(v: Option<&Value>, what: &str, example: &str) -> Result<Option<Vec<String>>, HostError> {
    let Some(v) = v else { return Ok(None) };
    let bad = |detail: &str| {
        HostError::new(format!(
            "agent() opts.{what} must be an array of non-empty {example}; got {detail}. Refusing the spawn rather than \
             running it {}.",
            if what == "disallowedTools" { "un-narrowed" } else { "un-clamped" }
        ))
    };
    let Some(list) = v.as_array() else {
        let kind = match v {
            Value::String(_) => "string",
            Value::Number(_) => "number",
            Value::Bool(_) => "boolean",
            Value::Object(_) | Value::Array(_) | Value::Null => "object",
        };
        return Err(bad(kind));
    };
    let mut out = Vec::new();
    for item in list {
        match item.as_str() {
            Some(s) if !s.is_empty() && s == s.trim() => out.push(s.to_owned()),
            _ => return Err(bad("an array with non-string, empty, or whitespace-padded entries")),
        }
    }
    out.sort();
    out.dedup();
    Ok(Some(out))
}

impl Shared {
    pub(super) fn agent(&self, realm: u32, payload: Value) -> Result<String, HostError> {
        let prompt = payload["prompt"].as_str().unwrap_or_default().to_owned();
        let raw_opts = payload.get("opts").filter(|o| o.is_object()).cloned();
        let opts_value = raw_opts.clone().unwrap_or(Value::Null);
        self.caps()?;
        let disallowed = string_list(
            opts_value.get("disallowedTools"),
            "disallowedTools",
            "tool-name strings (e.g. ['Bash', 'Write'])",
        )?;
        let clamp = string_list(opts_value.get("bashCommandClamp"), "bashCommandClamp", "strings")?;
        if clamp.is_some_and(|c| !c.is_empty()) {
            return Err(HostError::new(
                "agent() opts.bashCommandClamp is not available in this build. Refusing the spawn rather than running \
                 it un-clamped.",
            ));
        }
        let text = |k: &str| match &opts_value[k] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        };
        let opts = AgentOpts {
            schema: opts_value.get("schema").filter(|s| s.is_object()).cloned(),
            model: text("model"),
            effort: text("effort"),
            worktree: opts_value["isolation"].as_str() == Some("worktree"),
            agent_type: text("agentType"),
            disallowed: disallowed.clone().unwrap_or_default(),
            stall_ms: opts_value.get("stallMs").filter(|v| !v.is_null()).map(js_number),
        };
        // The key options as the journal hashes them (deny lists normalized).
        let mut key_opts = raw_opts.clone().unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if let Some(obj) = key_opts.as_object_mut() {
            match disallowed {
                Some(list) if !list.is_empty() => {
                    obj.insert("disallowedTools".into(), list.into_iter().map(Value::String).collect());
                }
                _ => {
                    obj.remove("disallowedTools");
                }
            }
            obj.remove("bashCommandClamp");
        }
        let mut state = lock(&self.state);
        state.agents += 1;
        let index = state.agents;
        state.next_call += 1;
        let call = state.next_call;
        let label = match &opts_value["label"] {
            Value::Null => {
                let head: String = prompt.chars().take(60).collect();
                collapse(&head)
            }
            Value::String(s) => collapse(s),
            other => collapse(&other.to_string()),
        };
        let child_phase = state.children.get(&realm).map(|c| c.phase.clone());
        let phase_title = child_phase.or_else(|| text("phase")).or_else(|| state.current_phase.clone());
        let phase_index = phase_title.as_deref().map(|t| self.announce(&mut state, t, None));
        let lane = lane_of(&payload);
        let key = journal::key(state.lanes.prev(lane), &prompt, Some(&key_opts));
        state.lanes.set(lane, key.clone());
        let model = opts.model.clone().or_else(|| self.spec.default_model.clone());
        let now = epoch_ms();
        if !state.live
            && let Some(replay) = &self.spec.replay
        {
            if let Some(cached) = replay.results.get(&key) {
                if replay.replaced.contains(&key) {
                    state.lanes.set(lane, journal::replaced(&key, &cached.agent_id));
                }
                let result = cached.result.get().to_owned();
                self.row(Row::Agent(Box::new(AgentRow {
                    shape: Shape::Cached,
                    index,
                    label,
                    agent_id: Some(cached.agent_id.clone()),
                    phase_index,
                    phase_title,
                    model,
                    state: Some(AgentState::Done),
                    started_at: Some(now),
                    last_progress_at: Some(now),
                    cached: Some(true),
                    result_preview: preview_json(&result),
                    prompt_preview: preview(&prompt),
                    ..AgentRow::default()
                })));
                let _ = self.events.send(Event::Settle { realm, call, ok: true, json: result });
                return Ok(call.to_string());
            }
            if !replay.was_in_flight(&key) {
                state.live = true;
            }
        } else {
            state.live = true;
        }
        let rekey = self
            .spec
            .replay
            .as_ref()
            .is_some_and(|r| r.results.contains_key(&key) || r.demoted.contains(&key) || r.failed.contains(&key));
        drop(state);
        if opts_value["isolation"].as_str() == Some("remote") {
            return Err(HostError::new("agent({isolation:'remote'}) is not available in this build"));
        }
        self.row(Row::Agent(Box::new(AgentRow {
            index,
            label: label.clone(),
            phase_index,
            phase_title: phase_title.clone(),
            agent_type: opts.agent_type.clone(),
            isolation: opts.worktree.then(|| "worktree".to_owned()),
            model: model.clone(),
            state: Some(AgentState::Start),
            queued_at: Some(now),
            prompt_preview: preview(&prompt),
            last_progress_at: Some(now),
            ..AgentRow::default()
        })));
        let agent = AgentCall {
            realm,
            call,
            index,
            prompt,
            opts,
            label,
            phase_title,
            phase_index,
            key,
            lane,
            rekey,
            queued_at: now,
            model,
            agent_id: Arc::default(),
        };
        if let Some(me) = self.arc() {
            self.handle.spawn(agent_task(me, agent));
        }
        Ok(call.to_string())
    }
}

pub(super) fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A JSON value's preview: strings as their text, anything else as JSON.
fn preview_json(json: &str) -> Option<String> {
    match serde_json::from_str::<Value>(json) {
        Ok(Value::String(s)) => preview(&s),
        Ok(Value::Null) => None,
        _ => preview(json),
    }
}

/// JavaScript's `Number(v)` for what an option can hold.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) if s.trim().is_empty() => 0.0,
        Value::String(s) => s.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}
