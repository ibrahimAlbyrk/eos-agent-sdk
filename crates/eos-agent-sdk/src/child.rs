//! Running a subagent's turns and reading what it did from its own events.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use eos_agent_core::{AgentEvent, SubagentObserver, SubagentReport, TurnOutcome};
use eos_agent_engine::AgentHandle;
use eos_agent_types::{ContentBlock, Role, Usage};
use serde_json::value::RawValue;
use tokio::sync::{mpsc, watch};

/// One event of a subagent, for a host that watches what subagents do.
#[derive(Debug, Clone)]
pub struct SubagentEvent {
    pub agent_id: Arc<str>,
    /// The agent that started it (a top-level agent or another subagent).
    pub parent_id: Arc<str>,
    pub event: SubagentEventKind,
}

/// A subagent's life as its host sees it: each turn it runs is announced
/// with `Started`, followed by the turn's own events and closed by `Ended`
/// (its first turn, and any later one a message woke it for).
#[derive(Debug, Clone)]
pub enum SubagentEventKind {
    Started(Arc<SubagentStarted>),
    /// One of the subagent's own events.
    Agent(AgentEvent),
    Ended(Box<SubagentEnded>),
}

/// What a subagent is, told with every `Started`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentStarted {
    /// The parent's tool call that started it (its Agent call).
    pub tool_use_id: String,
    pub agent_type: String,
    /// The model it runs on.
    pub model: String,
    /// The effort it runs at; `None` on a model that takes none.
    pub effort: Option<String>,
    pub description: String,
    /// The name other agents address it by.
    pub name: Option<String>,
    /// The parent does not wait for it; its result arrives as a notification.
    pub background: bool,
}

/// How one turn of a subagent ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentEnded {
    /// `completed`, `killed` (stopped or abandoned) or `failed`.
    pub status: &'static str,
    /// Why it failed.
    pub error: Option<String>,
    /// The usage of its last response (its context size at the end).
    pub usage: Usage,
    /// Tool calls it made in this turn.
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// Who else hears a subagent's events: the request's observer and the
/// session's token budget.
#[derive(Clone, Default)]
pub(crate) struct Listeners {
    pub(crate) observer: Option<Arc<dyn SubagentObserver>>,
    /// Told each response's output tokens.
    pub(crate) spend: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

impl std::fmt::Debug for Listeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listeners").field("observer", &self.observer).finish_non_exhaustive()
    }
}

/// Where a subagent's events are passed on to.
#[derive(Debug, Clone)]
pub(crate) struct Forward {
    pub(crate) to: mpsc::Sender<SubagentEvent>,
    pub(crate) agent_id: Arc<str>,
    pub(crate) parent_id: Arc<str>,
    pub(crate) about: Arc<SubagentStarted>,
}

impl Forward {
    fn event(&self, event: SubagentEventKind) -> SubagentEvent {
        SubagentEvent { agent_id: Arc::clone(&self.agent_id), parent_id: Arc::clone(&self.parent_id), event }
    }
}

/// Tells the host a turn ended as `killed` when its caller stopped waiting
/// (the future running it was dropped).
struct Announced<'a> {
    forward: Option<&'a Forward>,
    started: Instant,
    open: bool,
}

impl Announced<'_> {
    async fn end(mut self, status: &'static str, error: Option<String>, report: Option<&SubagentReport>) {
        self.open = false;
        if let Some(forward) = self.forward {
            let _ = forward.to.send(forward.event(self.ended(status, error, report))).await;
        }
    }

    fn ended(&self, status: &'static str, error: Option<String>, report: Option<&SubagentReport>) -> SubagentEventKind {
        SubagentEventKind::Ended(Box::new(SubagentEnded {
            status,
            error,
            usage: report.map(|r| r.usage.clone()).unwrap_or_default(),
            tool_uses: report.map_or(0, |r| r.tool_uses),
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }))
    }
}

impl Drop for Announced<'_> {
    fn drop(&mut self) {
        if let (true, Some(forward)) = (self.open, self.forward) {
            let _ = forward.to.try_send(forward.event(self.ended("killed", None, None)));
        }
    }
}

/// Follows one subagent's event stream for as long as the agent lives: its
/// tool calls, latest usage and latest reply. Reading the stream all the time
/// also keeps the agent from ever waiting on event delivery, unless the host
/// asked for the events and stops reading them.
#[derive(Debug)]
pub(crate) struct Tracker {
    stats: Mutex<Stats>,
    /// Turns that reached their end event.
    ended: watch::Sender<u64>,
    /// Where the host hears of the subagent, if it asked.
    forward: Option<Forward>,
}

#[derive(Debug, Default)]
struct Stats {
    tool_uses: u32,
    usage: Usage,
    reply: String,
    /// StructuredOutput calls waiting for their result.
    pending_structured: Vec<(String, Box<RawValue>)>,
    /// The input of the last StructuredOutput call that succeeded.
    structured: Option<Box<RawValue>>,
}

/// How a subagent's turn ended.
#[derive(Debug)]
pub(crate) enum RunEnd {
    Finished(SubagentReport),
    Stopped(SubagentReport),
    Failed(String),
}

impl Tracker {
    pub(crate) fn follow(
        mut events: mpsc::Receiver<AgentEvent>,
        forward: Option<Forward>,
        agent_id: Arc<str>,
        listeners: Listeners,
    ) -> Arc<Self> {
        let tracker =
            Arc::new(Self { stats: Mutex::default(), ended: watch::Sender::new(0), forward: forward.clone() });
        let follower = Arc::clone(&tracker);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let Some(observer) = &listeners.observer {
                    observer.event(&agent_id, &event);
                }
                if let (Some(spend), AgentEvent::Message(m)) = (&listeners.spend, &event)
                    && m.role() == Role::Assistant
                {
                    let output = follower.stats.lock().unwrap_or_else(PoisonError::into_inner).usage.output_tokens;
                    spend(output.unwrap_or(0));
                }
                let Some(fwd) = &forward else {
                    follower.observe(event);
                    continue;
                };
                // Passed on before the turn counts as ended, so a host sees
                // every event of a turn before the parent hears its result.
                let _ = fwd.to.send(fwd.event(SubagentEventKind::Agent(event.clone()))).await;
                follower.observe(event);
            }
        });
        tracker
    }

    fn observe(&self, event: AgentEvent) {
        let mut stats = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
        match event {
            AgentEvent::ToolUse { id, name, input } => {
                stats.tool_uses += 1;
                if name == eos_agent_agents::STRUCTURED_OUTPUT {
                    stats.pending_structured.push((id, (*input).to_owned()));
                }
            }
            AgentEvent::ToolResult { id, output } => {
                if let Some(i) = stats.pending_structured.iter().position(|(p, _)| *p == id) {
                    let (_, input) = stats.pending_structured.remove(i);
                    if !output.is_error {
                        stats.structured = Some(input);
                    }
                }
            }
            AgentEvent::RequestStarted { .. } => stats.usage = Usage::default(),
            AgentEvent::Usage(usage) => merge(&mut stats.usage, &usage.usage),
            AgentEvent::Message(message) if message.role() == Role::Assistant => {
                if let Ok(blocks) = message.blocks() {
                    stats.reply = reply_text(&blocks);
                }
            }
            AgentEvent::TurnEnded { .. } | AgentEvent::Interrupted => {
                drop(stats);
                self.ended.send_modify(|n| *n += 1);
            }
            _ => {}
        }
    }

    /// Tells the host a subagent that was not working ended as `status`
    /// (one stopped while it waited on its own background work, or before
    /// a turn it was claimed for began).
    pub(crate) async fn announce_end(&self, status: &'static str) {
        let Some(forward) = &self.forward else { return };
        let announced = Announced { forward: Some(forward), started: Instant::now(), open: false };
        let _ = forward.to.send(forward.event(announced.ended(status, None, None))).await;
    }

    /// Runs one turn of `agent` and reports it.
    pub(crate) async fn run(&self, agent: &AgentHandle, input: Vec<ContentBlock>) -> RunEnd {
        let mut ended = self.ended.subscribe();
        let seen = *ended.borrow_and_update();
        let tools_before = {
            let mut stats = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
            stats.structured = None;
            stats.tool_uses
        };
        let started = Instant::now();
        let forward = self.forward.as_ref();
        if let Some(forward) = forward {
            let _ = forward.to.send(forward.event(SubagentEventKind::Started(Arc::clone(&forward.about)))).await;
        }
        let announced = Announced { forward, started, open: true };
        let failure = match agent.send(input).await {
            Err(gone) => Err(gone.to_string()),
            Ok(Err(e)) => Err(e.to_string()),
            Ok(Ok(outcome)) => Ok(outcome),
        };
        let outcome = match failure {
            Ok(outcome) => outcome,
            Err(error) => {
                announced.end("failed", Some(error.clone()), None).await;
                return RunEnd::Failed(error);
            }
        };
        // The end event is in the stream before the reply; wait until it is read.
        let _ = ended.wait_for(|n| *n > seen).await;
        let report = {
            let stats = self.stats.lock().unwrap_or_else(PoisonError::into_inner);
            SubagentReport {
                agent_id: agent.id().to_owned(),
                text: stats.reply.clone(),
                usage: stats.usage.clone(),
                tool_uses: stats.tool_uses - tools_before,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                structured: stats.structured.clone(),
                worktree: None,
            }
        };
        match outcome {
            TurnOutcome::Finished(_) => {
                announced.end("completed", None, Some(&report)).await;
                RunEnd::Finished(report)
            }
            TurnOutcome::Interrupted => {
                announced.end("killed", None, Some(&report)).await;
                RunEnd::Stopped(report)
            }
        }
    }
}

impl Tracker {
    /// Runs a turn of an agent that must answer through StructuredOutput:
    /// when the turn ends without a valid call, Claude Code's nudge goes in
    /// once more as a new turn.
    pub(crate) async fn run_structured(&self, agent: &AgentHandle, input: Vec<ContentBlock>) -> RunEnd {
        let mut end = self.run(agent, input).await;
        if let RunEnd::Finished(report) = &end
            && report.structured.is_none()
        {
            let nudge =
                vec![ContentBlock::Text { text: eos_agent_agents::STRUCTURED_OUTPUT_NUDGE.into(), citations: None }];
            let before = report.clone();
            end = match self.run(agent, nudge).await {
                RunEnd::Finished(mut again) => {
                    again.tool_uses += before.tool_uses;
                    again.duration_ms += before.duration_ms;
                    RunEnd::Finished(again)
                }
                other => other,
            };
        }
        end
    }
}

fn merge(into: &mut Usage, from: &Usage) {
    let fields = [
        (&mut into.input_tokens, from.input_tokens),
        (&mut into.output_tokens, from.output_tokens),
        (&mut into.cache_creation_input_tokens, from.cache_creation_input_tokens),
        (&mut into.cache_read_input_tokens, from.cache_read_input_tokens),
    ];
    for (slot, value) in fields {
        if value.is_some() {
            *slot = value;
        }
    }
}

fn reply_text(blocks: &[ContentBlock]) -> String {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    texts.join("\n")
}
