//! One `agent()` call from its queue slot to its result: the attempts,
//! the stall retries, the journal line.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eos_agent_core::{SubagentObserver, SubagentReport, SubagentRequest, SubagentStart, SubagentSystem, WorkflowTag};
use serde_json::Value;
use serde_json::value::RawValue;

use super::call::AgentCall;
use super::watch::Watch;
use super::{
    DENIED, NOTE, NOTE_SCHEMA, STALL_RETRIES, SUBAGENT, SUBAGENT_SCHEMA, Shared, WORKFLOW_SUBAGENT, epoch_ms, lock,
};
use crate::journal::{self, Entry};
use crate::progress::{AgentRow, AgentState, Row, Shape, preview};
use crate::{deny, schema};

pub(super) async fn agent_task(shared: Arc<Shared>, call: AgentCall) {
    let outcome = tokio::select! {
        () = shared.cancel.cancelled() => return,
        out = run_agent(&shared, &call) => out,
    };
    if shared.cancel.is_cancelled() {
        return;
    }
    if let Err((_, message)) = &outcome {
        // Once an attempt started, its own rows tell how it ended.
        let started = lock(&call.agent_id).clone();
        if started.is_none() {
            shared.row(Row::Agent(Box::new(AgentRow {
                shape: Shape::Refused,
                index: call.index,
                label: call.label.clone(),
                phase_index: call.phase_index,
                phase_title: call.phase_title.clone(),
                model: call.model.clone(),
                state: Some(AgentState::Error),
                error: Some(message.clone()),
                queued_at: Some(call.queued_at),
                prompt_preview: preview(&call.prompt),
                last_progress_at: Some(epoch_ms()),
                ..AgentRow::default()
            })));
        }
        let _ = shared.journal.append(&Entry::Failed { key: call.key.clone(), agent_id: started.unwrap_or_default() });
    }
    shared.settle(call.realm, call.call, outcome);
}

type Settled = Result<String, (String, String)>;

/// Claude Code's throttle backoff: a reply under this many output tokens…
const THROTTLE_TOKENS: u64 = 50;
/// …after more than half the stall window, but at most this long…
const THROTTLE_AFTER_MS: f64 = 90_000.0;
/// …is tried once more after this wait.
const THROTTLE_WAIT: Duration = Duration::from_secs(45);

fn thrown(message: impl Into<String>) -> (String, String) {
    ("Error".into(), message.into())
}

async fn run_agent(shared: &Arc<Shared>, call: &AgentCall) -> Settled {
    let Ok(_permit) = Arc::clone(&shared.semaphore).acquire_owned().await else {
        return Err(thrown("Workflow aborted"));
    };
    let opts = &call.opts;
    if let Some(wanted) = &opts.agent_type {
        let types = shared.subagents.agent_types(&shared.spec.session);
        if !types.is_empty() && !types.iter().any(|t| t == wanted) {
            return Err(thrown(format!(
                "agent({{agentType}}): agent type '{wanted}' not found. Available agents: {}",
                types.join(", ")
            )));
        }
    }
    if let Some(schema) = &opts.schema {
        if let Some(e) = schema::invalid(schema) {
            return Err(thrown(format!("agent({{schema}}) received an invalid JSON Schema: {e}")));
        }
        if let Some(finding) = schema::lint(schema) {
            return Err(thrown(format!(
                "agent({{schema}}) received an unusable JSON Schema \u{2014} {}: {}. The subagent was not started \
                 \u{2014} fix the schema and call agent() again.",
                finding.consequence(),
                finding.message
            )));
        }
    }
    if !opts.disallowed.is_empty() {
        let pool = shared.subagents.tool_names(&shared.spec.session);
        deny::check(&opts.disallowed, &pool, opts.schema.is_some()).map_err(thrown)?;
    }
    if opts.schema.is_some()
        && deny::takes_structured_output(DENIED.into_iter().chain(opts.disallowed.iter().map(String::as_str)))
    {
        return Err(thrown(
            "agent() schema mode needs the StructuredOutput tool, but the spawn's merged disallowedTools deny it \u{2014} \
             refusing the spawn instead of running an agent that cannot return its structured output.",
        ));
    }
    let worktree = if opts.worktree {
        let slug = format!("{}-{}", shared.spec.run_id, call.index);
        Some(shared.subagents.create_worktree(&shared.spec.session, &slug).await.map_err(thrown)?)
    } else {
        None
    };
    let mut prompt = call.prompt.clone();
    if let Some(wt) = &worktree {
        let main = shared.spec.cwd.as_ref().map_or_else(String::new, |c| c.display().to_string());
        prompt.push_str(&format!(
            "\n\n---\nYou are running in an isolated git worktree at `{}` (a separate working copy of the repo). Changes \
             you make here do NOT affect the main working directory (`{main}`) or other agents. Work normally \u{2014} \
             the worktree will be cleaned up automatically if you made no changes, or preserved for review if you did.",
            wt.path.display()
        ));
    }
    let result = attempts(shared, call, &prompt, worktree.as_ref().map(|w| w.path.clone())).await;
    if let Some(wt) = worktree {
        shared.subagents.release_worktree(wt).await;
    }
    result
}

/// How one attempt ended, when the call goes on.
enum Tried {
    Stalled { ms: u64 },
    Finished { report: Box<SubagentReport>, watch: Arc<Watch>, ms: u64 },
}

/// The attempts of one call, with what earlier ones used carried into the
/// rows of later ones: `(tokens, tool calls, duration)`.
struct Attempts<'a> {
    shared: &'a Arc<Shared>,
    call: &'a AgentCall,
    prompt: &'a str,
    cwd: Option<PathBuf>,
    system: SubagentSystem,
    /// Claude Code's `ge`: the stall window in ms (none unless above 0).
    stall_ms: f64,
    started_at: u64,
    carried: (u64, u64, u64),
}

impl Attempts<'_> {
    fn stall_window(&self) -> Option<Duration> {
        (self.stall_ms > 0.0).then(|| Duration::from_secs_f64((self.stall_ms / 1000.0).min(1e9)))
    }

    fn carry(&mut self, tokens: u64, tools: u64, ms: u64) {
        self.carried = (self.carried.0 + tokens, self.carried.1 + tools, self.carried.2 + ms);
    }

    /// One attempt; `Err` is the call's outcome when it cannot go on.
    async fn once(&mut self, attempt: u32, label: String, reason: Option<&'static str>) -> Result<Tried, Settled> {
        let (shared, call, opts) = (self.shared, self.call, &self.call.opts);
        let watch = Arc::new(Watch::new(shared, call, label, attempt, reason, self.started_at, self.carried));
        let mut denied: Vec<String> = DENIED.iter().map(|s| (*s).to_owned()).collect();
        denied.extend(deny::host_denies(&opts.disallowed));
        let request = SubagentRequest {
            parent_id: shared.spec.session.clone(),
            tool_use_id: shared.spec.tool_use_id.clone(),
            description: call.label.clone(),
            prompt: self.prompt.to_owned(),
            agent_type: Some(opts.agent_type.clone().unwrap_or_else(|| WORKFLOW_SUBAGENT.to_owned())),
            model: opts.model.clone(),
            effort: opts.effort.clone(),
            isolation: None,
            system: Some(self.system.clone()),
            output_schema: opts.schema.clone(),
            deny_tools: denied,
            observer: Some(Arc::clone(&watch) as Arc<dyn SubagentObserver>),
            cwd: self.cwd.clone(),
            workflow: Some(WorkflowTag { run_id: shared.spec.run_id.clone(), agent_index: call.index }),
            background: false,
            ..SubagentRequest::default()
        };
        let began = tokio::time::Instant::now();
        let outcome = tokio::select! {
            out = shared.subagents.start(request) => Some(out),
            () = watch.stalled(self.stall_window()) => None,
            () = watch.gave_up() => Some(Err(String::new())),
        };
        let ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (tokens, tools) = watch.counts();
        let Some(outcome) = outcome else {
            self.carry(tokens, tools, ms);
            let error = format!("stalled \u{2014} no progress for {}ms", js_number(self.stall_ms));
            watch.finish_row(AgentState::Error, Some(error), None, self.carried.2);
            return Ok(Tried::Stalled { ms });
        };
        let duration = self.carried.2 + ms;
        if let Some(cap) = watch.structured_cap() {
            watch.finish_row(AgentState::Error, Some(cap.clone()), None, duration);
            return Err(Err(thrown(cap)));
        }
        match outcome {
            Ok(SubagentStart::Finished(report)) => Ok(Tried::Finished { report: Box::new(report), watch, ms }),
            Ok(SubagentStart::Launched { .. }) => Err(Err(thrown("the workflow agent was started in the background"))),
            Err(message) => {
                // A terminal API error: the call resolves to null.
                let text = if message.is_empty() { "API error".to_owned() } else { message };
                watch.finish_row(AgentState::Error, Some(text.clone()), None, duration);
                let line = format!("[{}] failed: {text}", call.label);
                shared.log(line.clone());
                lock(&shared.state).failures.push(line);
                Err(Ok("null".into()))
            }
        }
    }

    /// Claude Code's `he`: a reply with no stop reason and under 50 output
    /// tokens, after more than half the stall window (at most 90 s).
    fn throttled(&self, tried: &Tried) -> bool {
        let Tried::Finished { report, watch, ms } = tried else { return false };
        let (stated, output) = watch.reply_signs();
        let window = (self.stall_ms * 0.5).min(THROTTLE_AFTER_MS);
        !self.stall_ms.is_nan()
            && !stated
            && report.structured.is_none()
            && output.is_some_and(|t| t < THROTTLE_TOKENS)
            && (*ms as f64) > window
    }

    /// A finished attempt the call does not keep: its row ends, what it
    /// used carries over.
    fn set_aside(&mut self, tried: &Tried) {
        if let Tried::Finished { report, watch, ms } = tried {
            let shown = report.structured.as_ref().map_or_else(|| preview(&report.text), |s| preview(s.get()));
            watch.finish_row(AgentState::Done, None, shown, self.carried.2 + ms);
            let (tokens, tools) = watch.counts();
            self.carry(tokens, tools, *ms);
        }
    }

    /// The final attempt's result: the journal line and the done row.
    fn keep(&self, report: &SubagentReport, watch: &Watch, ms: u64) -> Settled {
        let (shared, call) = (self.shared, self.call);
        let schema_mode = call.opts.schema.is_some();
        let duration = self.carried.2 + ms;
        let json = if schema_mode {
            match &report.structured {
                Some(raw) => raw.get().to_owned(),
                None => {
                    let message = "agent({schema}): subagent completed without calling StructuredOutput (after \
                                   in-conversation nudge)";
                    watch.finish_row(AgentState::Error, Some(message.into()), None, duration);
                    return Err(thrown(message));
                }
            }
        } else {
            Value::String(report.text.clone()).to_string()
        };
        let agent_id = watch.agent_id().unwrap_or_else(|| report.agent_id.clone());
        if let Ok(raw) = RawValue::from_string(json.clone()) {
            let _ = shared.journal.append(&Entry::Result {
                key: call.key.clone(),
                agent_id: agent_id.clone(),
                result: raw,
                tools_used: None,
            });
            if call.rekey {
                let mut state = lock(&shared.state);
                if state.lanes.prev(call.lane) == call.key {
                    state.lanes.set(call.lane, journal::replaced(&call.key, &agent_id));
                }
            }
        }
        let shown = if schema_mode { preview(&json) } else { preview(&report.text) };
        watch.finish_row(AgentState::Done, None, shown, duration);
        Ok(json)
    }
}

/// Claude Code's attempt loop: a throttled reply is tried once more after
/// 45 s, a stalled one up to five more times.
async fn attempts(shared: &Arc<Shared>, call: &AgentCall, prompt: &str, cwd: Option<PathBuf>) -> Settled {
    let opts = &call.opts;
    let system = match (&opts.agent_type, opts.schema.is_some()) {
        (Some(_), true) => SubagentSystem::Append(NOTE_SCHEMA.to_owned()),
        (Some(_), false) => SubagentSystem::Append(NOTE.to_owned()),
        (None, true) => SubagentSystem::Replace(SUBAGENT_SCHEMA.to_owned()),
        (None, false) => SubagentSystem::Replace(SUBAGENT.to_owned()),
    };
    let default_ms = shared.config.stall.as_millis() as f64;
    let mut run = Attempts {
        shared,
        call,
        prompt,
        cwd,
        system,
        stall_ms: opts.stall_ms.unwrap_or(default_ms),
        started_at: epoch_ms(),
        carried: (0, 0, 0),
    };
    let label = &call.label;
    let mut tried = match run.once(1, label.clone(), None).await {
        Ok(t) => t,
        Err(settled) => return settled,
    };
    let throttled = run.throttled(&tried);
    if throttled {
        if let Tried::Finished { watch, ms, .. } = &tried {
            let tokens = watch.reply_signs().1.map_or_else(|| "?".to_owned(), |t| t.to_string());
            shared.log(format!(
                "[{label}] throttled response (no stop_reason, {tokens} output tokens in {}s) \u{2014} sleeping 45s \
                 before retry",
                seconds(*ms)
            ));
        }
        run.set_aside(&tried);
        tokio::time::sleep(THROTTLE_WAIT).await;
        tried = match run.once(2, format!("{label} (throttle-retry)"), Some("throttled")).await {
            Ok(t) => t,
            Err(settled) => return settled,
        };
        if run.throttled(&tried) {
            shared.log(format!("[{label}] throttle-retry also degraded \u{2014} giving up on throttle backoff"));
        }
    }
    let mut stalls = 0;
    let mut retry = 1;
    while !throttled && retry <= STALL_RETRIES {
        let Tried::Stalled { ms } = tried else { break };
        stalls += 1;
        shared.log(format!(
            "[stall] agent \"{label}\" stalled (no progress) after {}s \u{2014} retrying ({retry}/{STALL_RETRIES})",
            seconds(ms)
        ));
        tried = match run.once(retry + 1, format!("{label} (retry {retry})"), Some("stalled")).await {
            Ok(t) => t,
            Err(settled) => return settled,
        };
        retry += 1;
    }
    match tried {
        Tried::Stalled { .. } => Err(thrown(format!(
            "agent stalled on all {} attempts (no progress for {}ms each)",
            stalls + 1,
            js_number(run.stall_ms)
        ))),
        Tried::Finished { report, watch, ms } => run.keep(&report, &watch, ms),
    }
}

/// `Math.round(ms / 1000)`.
fn seconds(ms: u64) -> u64 {
    (ms + 500) / 1000
}

/// A number as JavaScript prints it in a template string.
fn js_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        let whole = n as i128;
        whole.to_string()
    } else if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        n.to_string()
    }
}

pub(super) fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}
