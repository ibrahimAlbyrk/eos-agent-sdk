//! The script's hooks into the run: everything but `agent()` and
//! `workflow()`, and how a realm settles its calls and ends.

use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{TaskEvent, TaskEventKind};
use serde_json::Value;

use super::{CHILD_MARK, End, Event, Pump, Shared, State, lock};
use crate::engine::{Host, HostError};
use crate::journal;
use crate::progress::Row;

// ---- host calls ----------------------------------------------------------

impl Host for Shared {
    fn call(&self, realm: u32, op: &str, payload: &str) -> Result<String, HostError> {
        let parse = || serde_json::from_str::<Value>(payload).unwrap_or(Value::Null);
        match op {
            "agent" => self.agent(realm, parse()),
            "log" => {
                let message = parse()["message"].as_str().unwrap_or_default().to_owned();
                self.log(message);
                Ok(String::new())
            }
            "phase" => {
                let title = parse()["title"].as_str().unwrap_or_default().to_owned();
                let mut state = lock(&self.state);
                state.current_phase = Some(title.clone());
                self.announce(&mut state, &title, None);
                Ok(String::new())
            }
            "caps" => self.caps().map(|()| String::new()),
            "fork" => {
                let v = parse();
                let kind = v["kind"].as_str().unwrap_or_default();
                Ok(lock(&self.state).lanes.fork(lane_of(&v), kind).to_string())
            }
            "slots" => {
                let v = parse();
                let (fork, n) = (index_of(&v["fork"]), index_of(&v["n"]));
                let ids = lock(&self.state).lanes.slots(fork, n.min(journal::MAX_SLOTS));
                Ok(serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into()))
            }
            "join" => {
                lock(&self.state).lanes.join(index_of(&parse()["fork"]));
                Ok(String::new())
            }
            "spent" => Ok(self.spent().to_string()),
            "failure" => {
                let v = parse();
                let message = v["message"].as_str().unwrap_or_default().to_owned();
                if v["log"].as_bool() == Some(true) {
                    self.log(message.clone());
                }
                lock(&self.state).failures.push(message);
                Ok(String::new())
            }
            "timer" => Ok(self.timer(realm, parse()["ms"].as_f64().unwrap_or(0.0)).to_string()),
            "clear" => {
                let id = parse()["id"].as_u64().unwrap_or(0);
                if let Some(timer) = lock(&self.state).timers.remove(&(realm, id)) {
                    timer.abort();
                }
                Ok(String::new())
            }
            "workflow" => self.child_workflow(realm, parse()),
            "done" | "fail" => {
                self.script_ended(realm, op == "done", payload);
                Ok(String::new())
            }
            other => Err(HostError::new(format!("unknown workflow hook {other}"))),
        }
    }
}

fn index_of(v: &Value) -> usize {
    v.as_u64().and_then(|n| usize::try_from(n).ok()).unwrap_or(0)
}

/// The lane a hook was called on.
pub(super) fn lane_of(payload: &Value) -> usize {
    index_of(&payload["lane"])
}

impl Shared {
    pub(super) fn arc(&self) -> Option<Arc<Self>> {
        self.me.upgrade()
    }

    pub(super) fn row(&self, row: Row) {
        let _ = self.rows.send(Pump::Row(row));
    }

    pub(super) fn log(&self, message: String) {
        self.row(Row::Log { message });
    }

    pub(super) fn task_event(&self, kind: TaskEventKind) -> eos_agent_core::BoxFuture<'_, ()> {
        self.subagents.task_event(TaskEvent {
            agent_id: self.spec.session.clone(),
            task_id: self.spec.task_id.clone(),
            tool_use_id: Some(self.spec.tool_use_id.clone()),
            kind,
        })
    }

    /// A phase's index; its row goes out the first time.
    pub(super) fn announce(&self, state: &mut State, title: &str, kind: Option<&str>) -> u32 {
        if let Some(entry) = state.phases.iter_mut().find(|p| p.0 == title) {
            if !entry.2 {
                entry.2 = true;
                self.row(Row::Phase { index: entry.1, title: title.to_owned(), kind: kind.map(str::to_owned) });
            }
            return entry.1;
        }
        let index = u32::try_from(state.phases.len()).unwrap_or(u32::MAX).saturating_add(1);
        state.phases.push((title.to_owned(), index, true));
        self.row(Row::Phase { index, title: title.to_owned(), kind: kind.map(str::to_owned) });
        index
    }

    /// Reserves a phase without announcing it (a child workflow's group).
    pub(super) fn reserve(&self, state: &mut State, title: &str) -> u32 {
        if let Some(entry) = state.phases.iter().find(|p| p.0 == title) {
            return entry.1;
        }
        let index = u32::try_from(state.phases.len()).unwrap_or(u32::MAX).saturating_add(1);
        state.phases.push((title.to_owned(), index, false));
        index
    }

    pub(super) fn spent(&self) -> u64 {
        self.config.budgets.as_ref().map_or(0, |b| b.spent(&self.spec.session))
    }

    /// Claude Code's `co` and `vn`: the agent cap and the token budget.
    pub(super) fn caps(&self) -> Result<(), HostError> {
        if lock(&self.state).agents >= self.config.max_agents {
            return Err(HostError::named(
                "WorkflowAgentCapError",
                format!(
                    "Workflow agent() call cap reached ({}). This usually means a loop using budget.remaining() never \
                     terminates because no token budget was set \u{2014} remaining() returns Infinity when budget.total \
                     is null. Add a hard iteration cap to the loop, or pass a token budget.",
                    self.config.max_agents
                ),
            ));
        }
        if let Some(total) = self.budget_total.filter(|&t| t > 0) {
            let spent = self.spent();
            if spent >= total {
                return Err(HostError::named(
                    "WorkflowBudgetExceededError",
                    format!(
                        "Workflow token budget exceeded ({} / {} output tokens). Stopping further agent() calls. \
                         In-flight agents will complete; their results are preserved.",
                        thousands(spent),
                        thousands(total)
                    ),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn timer(&self, realm: u32, ms: f64) -> u64 {
        let mut state = lock(&self.state);
        state.next_timer += 1;
        let id = state.next_timer;
        let events = self.events.clone();
        let wait = Duration::from_secs_f64((ms.max(0.0) / 1000.0).min(1e9));
        let task = self.handle.spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = events.send(Event::Timer { realm, id });
        });
        state.timers.insert((realm, id), task.abort_handle());
        id
    }

    pub(super) fn settle(&self, realm: u32, call: u64, outcome: Result<String, (String, String)>) {
        let event = match outcome {
            Ok(json) => Event::Settle { realm, call, ok: true, json },
            Err((name, message)) => {
                let json = serde_json::json!({ "name": name, "message": message }).to_string();
                Event::Settle { realm, call, ok: false, json }
            }
        };
        let _ = self.events.send(event);
    }

    pub(super) fn script_ended(&self, realm: u32, ok: bool, payload: &str) {
        let outcome = if ok {
            Ok((!payload.is_empty()).then(|| payload.to_owned()))
        } else {
            let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
            Err((
                v["name"].as_str().unwrap_or("Error").to_owned(),
                v["message"].as_str().unwrap_or_default().to_owned(),
                v["stack"].as_str().map(str::to_owned),
            ))
        };
        let mut state = lock(&self.state);
        if realm == 0 {
            state.outcome = Some(match outcome {
                Ok(result) => End::Completed(result),
                Err((name, message, stack)) => End::Failed(format_error(&name, &message, stack.as_deref())),
            });
            return;
        }
        let Some(child) = state.children.remove(&realm) else { return };
        drop(state);
        let _ = self.events.send(Event::CloseChild { realm });
        match outcome {
            Ok(result) => {
                self.log(format!("{CHILD_MARK} {} done", child.name));
                self.settle(0, child.parent, Ok(result.unwrap_or_else(|| "null".into())));
            }
            Err((name, message, stack)) => {
                let shown = format_error(&name, &message, stack.as_deref());
                self.child_failed_known(&child, &name, &shown);
            }
        }
    }
}

/// Claude Code's `ain`: `Name: message` and up to five `at` lines.
pub(super) fn format_error(name: &str, message: &str, stack: Option<&str>) -> String {
    let head = if message.is_empty() { name.to_owned() } else { format!("{name}: {message}") };
    let frames: Vec<String> = stack
        .unwrap_or_default()
        .lines()
        .filter(|l| l.trim_start().starts_with("at "))
        .take(5)
        .map(|l| {
            if l.chars().count() > 500 {
                format!("{}\u{2026}", l.chars().take(500).collect::<String>())
            } else {
                l.to_owned()
            }
        })
        .collect();
    if frames.is_empty() { head } else { format!("{head}\n{}", frames.join("\n")) }
}

pub(super) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
