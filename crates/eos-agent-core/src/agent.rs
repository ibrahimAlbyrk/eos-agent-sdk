use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;

use bytes::Bytes;
use eos_agent_types::{ContentBlock, Role, StopReason, Usage};
use futures_util::future::{Either, select};
use tokio_util::sync::CancellationToken;

use crate::assembler::Assembled;
use crate::auth::{ProviderId, RequestAuth};
use crate::context::{Compaction, ReminderSource, SessionContext};
use crate::extensions::Extensions;
use crate::message::{CacheTtl, MessageError, Origin, StoredMessage, SystemPrompt};
use crate::model::{LiveRequest, ModelClient, ModelError, Thinking};
use crate::ports::{AgentEvent, Clock, EventSink, PermissionGate, TranscriptStore};
use crate::retry::RetryPolicy;
use crate::tool::Toolbox;
use crate::workspace::Reminders;

mod compact;
mod deferred;
mod handoff;
mod inline;
mod input;
mod memory;
mod pairing;
mod request;
mod tools;

pub use deferred::{FailedServer, ServerInstructions, ServerNotes, instructions_section, server_paragraphs};
pub use inline::{INLINE_TOOLS_BETA, MID_CONVERSATION_TOOL_CHANGES_BETA, ToolChangeRefusals, ToolChanges};
pub use memory::check_cut;

// Claude Code's own wording, so models trained on it read them as intended.
const INTERRUPTED: &str = "[Request interrupted by user]";
const INTERRUPTED_TOOLS: &str = "[Request interrupted by user for tool use]";
const TOOL_INTERRUPTED: &str = "Interrupted by user";
/// The stop reason of a turn that used up its `max_turns` (Claude Code's).
pub const MAX_TURNS: &str = "max_turns";
const CUT_OFF: &str = "Output token limit hit. Resume directly \u{2014} no apology, no recap of what you were doing. \
Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";
const CUT_OFF_THINKING: &str = "Output token limit hit while you were still reasoning, before this response produced \
any text or tool call. That reasoning was not kept and is not part of this conversation, so there is nothing to \
resume. Continue from the conversation above: reason much more briefly and produce your answer or next tool call \
well within the output limit.";

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    /// The provider `model` runs on, for a host with several: assistant
    /// messages record it, and a request leaves out the reasoning of
    /// messages another provider wrote.
    pub provider: Option<ProviderId>,
    pub max_tokens: u32,
    pub thinking: Option<Thinking>,
    pub effort: Option<String>,
    /// Cache marker on the newest message (`None` = no message caching).
    pub cache_ttl: Option<CacheTtl>,
    pub retry: RetryPolicy,
    /// Concurrency-safe tool calls run together, at most this many at once.
    pub max_parallel_tools: usize,
    /// How many times one turn may continue after hitting the output limit.
    pub max_continuations: u32,
    /// Extra top-level request members (a JSON object).
    pub extra: Option<Bytes>,
    /// Consulted before every request for reminder text.
    pub reminder_sources: Vec<Arc<dyn ReminderSource>>,
    /// Summarizing: when (if ever) on its own, and in which words (`None` =
    /// never, not even when the host asks).
    pub compaction: Option<Compaction>,
    /// Where the tool set may change while the agent runs (an MCP server
    /// signed in to or listing new tools); consulted before every request.
    pub tool_source: Option<Arc<dyn ToolSource>>,
    /// See [`ModelRequest::cache_key`].
    pub cache_key: Option<Arc<str>>,
    /// Model requests one turn may make (Claude Code's `maxTurns` of an
    /// agent type): past it the turn ends after the tool results, with
    /// stop reason `max_turns`.
    pub max_turns: Option<u32>,
    /// The model takes mid-conversation `role: "system"` messages; when
    /// `false` they go out as `<system-reminder>` text in user messages.
    pub system_turns: bool,
    /// How tools sent in full that arrive after the first request reach the
    /// model (Claude Code's tool-change betas need system turns).
    pub tool_changes: ToolChanges,
    /// Refusals of the tool-change betas that hold for every agent sharing
    /// them (Claude Code's process-wide latches).
    pub tool_change_refusals: Arc<ToolChangeRefusals>,
    /// See [`ModelRequest::auth`](crate::ModelRequest::auth).
    pub auth: Option<RequestAuth>,
}

/// A tool set that can change under a running agent.
pub trait ToolSource: Send + Sync + std::fmt::Debug {
    /// The toolbox to use from now on, when it is no longer `current`.
    fn update(&self, current: &Arc<Toolbox>) -> Option<Arc<Toolbox>>;

    /// The toolbox as of now (what a new subagent or fork starts from), when
    /// the source builds one.
    fn current(&self) -> Option<Arc<Toolbox>> {
        None
    }

    /// The MCP servers the deferred-tools note speaks of, for an agent with
    /// `toolbox`; changes are announced before the next request.
    fn servers(&self, _toolbox: &Toolbox) -> ServerNotes {
        ServerNotes::default()
    }
}

impl AgentConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            provider: None,
            max_tokens: 32_000,
            thinking: Some(Thinking::Adaptive { summarized: true }),
            effort: None,
            cache_ttl: Some(CacheTtl::FiveMinutes),
            retry: RetryPolicy::default(),
            max_parallel_tools: 10,
            max_continuations: 3,
            extra: None,
            reminder_sources: Vec::new(),
            compaction: None,
            tool_source: None,
            cache_key: None,
            max_turns: None,
            system_turns: true,
            tool_changes: ToolChanges::InTools,
            tool_change_refusals: Arc::default(),
            auth: None,
        }
    }
}

/// The ports one turn runs on.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub model: &'a dyn ModelClient,
    pub events: &'a dyn EventSink,
    pub permissions: &'a dyn PermissionGate,
    pub clock: &'a dyn Clock,
    pub store: Option<&'a dyn TranscriptStore>,
}

impl std::fmt::Debug for Deps<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Finished(StopReason),
    Interrupted,
}

#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("could not record the conversation: {0}")]
    Store(#[from] std::io::Error),
    #[error(transparent)]
    Message(#[from] MessageError),
}

/// Why a conversation could not be cut back.
#[derive(Debug, thiserror::Error)]
pub enum RewindError {
    #[error("the conversation has only {len} messages")]
    OutOfRange { len: usize },
    /// The cut would keep a tool call without its result.
    #[error("message {keep} answers a tool call of the message before it; cut before the call or after its result")]
    SplitsToolCall { keep: usize },
    #[error("could not record the conversation: {0}")]
    Store(#[from] std::io::Error),
}

/// One conversation: its settings, prompt, tools and history.
#[derive(Debug)]
pub struct Agent {
    id: String,
    config: AgentConfig,
    system: SystemPrompt,
    toolbox: Arc<Toolbox>,
    transcript: Vec<StoredMessage>,
    extensions: Arc<Extensions>,
    last_usage: Usage,
    session: Option<SessionContext>,
    /// The conversation opened with the session context and its system
    /// notes have not followed yet (they go with its first request).
    session_notes_due: bool,
    announced: deferred::Announced,
    /// Deferred tools ToolSearch loaded (kept through a summary, as Claude
    /// Code's `preCompactDiscoveredTools`): only these are declared.
    loaded: BTreeSet<String>,
    /// Their declarations, kept for when their source goes away (the
    /// conversation still names them).
    kept: BTreeMap<String, Bytes>,
    /// Calls of the response being answered whose input was cut off: they
    /// get an error result instead of running.
    incomplete_calls: Vec<String>,
}

enum Streamed {
    Done(Assembled),
    Cancelled(Assembled),
}

impl Agent {
    pub fn new(id: impl Into<String>, config: AgentConfig, system: SystemPrompt, toolbox: Arc<Toolbox>) -> Self {
        Self {
            id: id.into(),
            config,
            system,
            toolbox,
            transcript: Vec::new(),
            extensions: Arc::default(),
            last_usage: Usage::default(),
            session: None,
            session_notes_due: false,
            announced: deferred::Announced::default(),
            loaded: BTreeSet::new(),
            kept: BTreeMap::new(),
            incomplete_calls: Vec::new(),
        }
    }

    /// Context sent with the first message of a new conversation.
    pub fn with_session_context(mut self, context: SessionContext) -> Self {
        self.announced.prime(&self.toolbox, &context.servers);
        self.session = Some(context);
        self
    }

    /// Starts with state the host prepared (a shared task list, a fork marker, …).
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = Arc::new(extensions);
        self
    }

    /// Resumes a conversation from recorded messages.
    pub fn with_history(mut self, history: Vec<StoredMessage>) -> Self {
        for message in &history {
            loaded_tools(message, &mut self.loaded);
        }
        self.transcript = history;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut AgentConfig {
        &mut self.config
    }

    pub fn transcript(&self) -> &[StoredMessage] {
        &self.transcript
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    /// The agent's extensions, for whoever must reach its tools' state while
    /// a turn runs (to stop one of its background tasks).
    pub fn shared_extensions(&self) -> Arc<Extensions> {
        Arc::clone(&self.extensions)
    }

    /// The system prompt from the next request on.
    pub fn set_system(&mut self, system: SystemPrompt) {
        self.system = system;
    }

    /// The tools from the next request on (a [`ToolSource`] still replaces
    /// them when its own set changes).
    pub fn set_toolbox(&mut self, toolbox: Arc<Toolbox>) {
        self.toolbox = toolbox;
    }

    /// Lets every tool rebuild what this agent had seen from its history
    /// (files read, artifact versions), as after resuming a conversation
    /// from its transcript. A no-op without history.
    pub async fn restore_tool_state(&self) {
        if self.transcript.is_empty() {
            return;
        }
        for tool in self.toolbox.tools() {
            tool.restore(&self.transcript, &self.extensions).await;
        }
    }

    /// Usage of the latest response; its context tokens are the live context size.
    pub fn last_usage(&self) -> &Usage {
        &self.last_usage
    }

    /// Runs one user turn to its end: model calls, tool calls, continuations.
    pub async fn run_turn(
        &mut self,
        input: Vec<ContentBlock>,
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> Result<TurnOutcome, TurnError> {
        self.push_input(input, deps).await?;
        self.run_requests(deps, cancel).await
    }

    /// Carries on a turn that stopped before the model answered the
    /// conversation's last user message (the request failed: a plan limit, a
    /// sign-in that ran out): the model is asked again, with no new input,
    /// as Claude Code re-runs an interrupted turn on resume. A turn cut off
    /// while its tools ran (the app quit) goes on the same way, once its
    /// calls are answered as cut off. `None` when the conversation ends
    /// otherwise (the host's seed is none: no turn ran on it).
    pub async fn resume_turn(
        &mut self,
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> Result<Option<TurnOutcome>, TurnError> {
        let last = self.transcript.iter().rev().find(|m| m.role() != Role::System);
        let cut_in_tools = !pairing::unanswered_calls(&self.transcript).is_empty();
        if !cut_in_tools && last.is_none_or(|m| m.role() != Role::User || m.is_meta()) {
            return Ok(None);
        }
        self.run_requests(deps, cancel).await.map(Some)
    }

    /// Results, as cut off, for the calls the conversation's last model
    /// message left unanswered (by what follows it, or by `input` about to
    /// join it): a turn that ended there (an older engine finished a turn on
    /// a response whose stop reason was not `tool_use`) or was cut off while
    /// its tools ran (the app quit, the turn's task was dropped). The host
    /// hears each call end; the caller records them ahead of anything else,
    /// so the transcript says what became of each call.
    async fn cut_off_results(&self, input: &[ContentBlock], deps: Deps<'_>) -> Vec<ContentBlock> {
        let answered = |id: &str| {
            input.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id))
        };
        let mut results = Vec::new();
        for id in pairing::unanswered_calls(&self.transcript).into_iter().filter(|id| !answered(id)) {
            let output = Arc::new(crate::tool::ToolOutput::error(pairing::UNANSWERED));
            deps.events.emit(AgentEvent::ToolResult { id: id.clone(), output }).await;
            results.push(pairing::unanswered(&id));
        }
        results
    }

    /// The turn's model requests and tool calls, from the conversation as it stands.
    async fn run_requests(&mut self, deps: Deps<'_>, cancel: &CancellationToken) -> Result<TurnOutcome, TurnError> {
        let mut continuations = 0;
        let mut compacted_for_overflow = false;
        let mut turn_count: u32 = 1;
        let cut_off = self.cut_off_results(&[], deps).await;
        if !cut_off.is_empty() {
            self.push(StoredMessage::new(Role::User, &cut_off)?, deps).await?;
        }
        loop {
            if let Some(fresh) = self.config.tool_source.as_ref().and_then(|s| s.update(&self.toolbox)) {
                self.toolbox = fresh;
            }
            let servers = self.config.tool_source.as_ref().map(|s| s.servers(&self.toolbox));
            self.latch_tool_changes();
            let loaded = &self.loaded;
            let tool_note = self.announced.update(&self.toolbox, servers, || loaded.clone());
            // The user's side first: a system turn must come right before the reply.
            self.deliver_reminders(deps).await?;
            self.push_session_notes(deps).await?;
            if let Some(note) = tool_note {
                self.push(note.message(), deps).await?;
            }
            if self.needs_compaction() {
                self.compact(None, deps, cancel).await?;
                // The summary reopened the conversation: its session notes go before the request.
                self.push_session_notes(deps).await?;
            }
            let streamed = match self.send(deps, cancel).await {
                // The provider counted more than our estimate did: summarize
                // once and try again with the summary.
                Err(TurnError::Model(ModelError::ContextWindowExceeded(_)))
                    if !compacted_for_overflow && self.compacts_itself() =>
                {
                    compacted_for_overflow = true;
                    self.compact(None, deps, cancel).await?;
                    continue;
                }
                other => other?,
            };
            let assembled = match streamed {
                Streamed::Done(a) => a,
                Streamed::Cancelled(partial) => {
                    let kept = tools::without_unanswered_calls(partial.content);
                    if !kept.is_empty() {
                        let message = StoredMessage::new(Role::Assistant, &kept)?;
                        self.record(message, Some(&partial.usage), deps).await?;
                    }
                    return self.interrupted(INTERRUPTED, deps).await;
                }
            };
            self.last_usage = assembled.usage.clone();
            let stated = assembled.stop_reason.is_some();
            let stop = assembled.stop_reason.unwrap_or(StopReason::EndTurn);
            let produced_output = assembled.content.iter().any(|b| !matches!(b, ContentBlock::Thinking { .. }));
            if !assembled.content.is_empty() {
                let message = StoredMessage::new(Role::Assistant, &assembled.content)?;
                self.record(message, Some(&assembled.usage), deps).await?;
            }

            // The response's calls are answered whatever its stop reason
            // (`end_turn`, `max_tokens`, none at all), as their presence and
            // not the stop reason asks for a follow-up: a call left without
            // its result makes every later request of the conversation invalid.
            let calls_tools = assembled.content.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. }));
            if calls_tools {
                // A response cut off at the output limit counts against the
                // continuations even when its calls are answered.
                let cut_off = stop == StopReason::MaxTokens;
                if cut_off {
                    continuations += 1;
                }
                self.incomplete_calls = assembled.incomplete_calls;
                let (results, interrupted, ends_turn) = self.run_tools(&assembled.content, deps, cancel).await;
                self.incomplete_calls.clear();
                if !results.is_empty() {
                    self.push(StoredMessage::new(Role::User, &results)?, deps).await?;
                }
                if interrupted {
                    return self.interrupted(INTERRUPTED_TOOLS, deps).await;
                }
                if ends_turn {
                    let ended = AgentEvent::TurnEnded { stop_reason: StopReason::EndTurn, stated: true };
                    deps.events.emit(ended).await;
                    return Ok(TurnOutcome::Finished(StopReason::EndTurn));
                }
                if cut_off && continuations > self.config.max_continuations {
                    deps.events.emit(AgentEvent::TurnEnded { stop_reason: stop.clone(), stated }).await;
                    return Ok(TurnOutcome::Finished(stop));
                }
                turn_count += 1;
                if self.config.max_turns.is_some_and(|max| turn_count > max) {
                    let stop = StopReason::Other(MAX_TURNS.to_owned());
                    deps.events.emit(AgentEvent::TurnEnded { stop_reason: stop.clone(), stated: true }).await;
                    return Ok(TurnOutcome::Finished(stop));
                }
                continue;
            }
            match stop {
                StopReason::PauseTurn | StopReason::Compaction => {}
                StopReason::MaxTokens if continuations < self.config.max_continuations => {
                    continuations += 1;
                    let note = if produced_output { CUT_OFF } else { CUT_OFF_THINKING };
                    self.push(StoredMessage::user_text(note), deps).await?;
                }
                other => {
                    deps.events.emit(AgentEvent::TurnEnded { stop_reason: other.clone(), stated }).await;
                    return Ok(TurnOutcome::Finished(other));
                }
            }
        }
    }

    async fn push(&mut self, message: StoredMessage, deps: Deps<'_>) -> Result<(), TurnError> {
        self.record(message, None, deps).await
    }

    /// Adds a message to the conversation; `usage` is the response's, for an
    /// assistant message (the transcript keeps it).
    async fn record(&mut self, message: StoredMessage, usage: Option<&Usage>, deps: Deps<'_>) -> Result<(), TurnError> {
        let origin = self.config.provider.clone().filter(|_| message.role() == Role::Assistant);
        let message =
            message.with_origin(origin.map(|provider| Origin { provider, model: self.config.model.as_str().into() }));
        if let Some(store) = deps.store {
            store.append(&self.id, &message, usage).await?;
        }
        deps.events.emit(AgentEvent::Message(message.clone())).await;
        loaded_tools(&message, &mut self.loaded);
        self.transcript.push(message);
        Ok(())
    }

    /// The tool-change mode for the next request (within the refusals so
    /// far), and the declarations of loaded deferred tools as of now.
    fn latch_tool_changes(&mut self) {
        let mode = self.config.tool_change_refusals.allowed(self.config.tool_changes, &self.config.model);
        self.announced.latch(mode, &self.toolbox, &self.transcript);
        for name in &self.loaded {
            if let Some(tool) = self.toolbox.get(name).filter(|t| t.spec().is_deferred()) {
                self.kept.insert(name.clone(), tool.spec().json().clone());
            }
        }
    }

    /// The next request, sent again without a tool-change beta the API
    /// refused (as Claude Code does).
    async fn send(&mut self, deps: Deps<'_>, cancel: &CancellationToken) -> Result<Streamed, TurnError> {
        loop {
            let request = self.build_request();
            if let Some(live) = self.extensions.get::<LiveRequest>() {
                live.set(Some(request.clone()));
            }
            match self.request(request, deps, cancel).await {
                Err(TurnError::Model(error)) if self.tool_changes_refused(&error) => {}
                other => return other,
            }
        }
    }

    /// Takes in a refused request: true when a tool-change beta was refused
    /// and the request should go again without it.
    fn tool_changes_refused(&mut self, error: &ModelError) -> bool {
        let refused = self.announced.refused(error, &self.config.model, &self.config.tool_change_refusals);
        if refused {
            self.latch_tool_changes();
        }
        refused
    }

    /// Queued notes go out as one user message ahead of the next request.
    async fn deliver_reminders(&mut self, deps: Deps<'_>) -> Result<(), TurnError> {
        let reminders = self.extensions.get_or_default::<Reminders>();
        // Cleared before the notes are taken: a note pushed after the take still asks for a turn.
        if let Some(wake) = self.extensions.get::<crate::Wake>() {
            wake.clear();
        }
        for source in &self.config.reminder_sources {
            for note in source.collect(&self.extensions).await {
                reminders.push(note);
            }
        }
        let notes = reminders.take();
        if notes.is_empty() {
            return Ok(());
        }
        let blocks: Vec<ContentBlock> =
            notes.into_iter().map(|text| ContentBlock::Text { text, citations: None }).collect();
        self.push(StoredMessage::new(Role::User, &blocks)?, deps).await
    }

    async fn interrupted(&mut self, note: &str, deps: Deps<'_>) -> Result<TurnOutcome, TurnError> {
        self.push(StoredMessage::user_text(note), deps).await?;
        deps.events.emit(AgentEvent::Interrupted).await;
        Ok(TurnOutcome::Interrupted)
    }
}

/// Adds the tools ToolSearch loaded in `message` (its `tool_reference` results).
fn loaded_tools(message: &StoredMessage, names: &mut BTreeSet<String>) {
    #[derive(serde::Deserialize)]
    struct Reference {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        tool_name: String,
    }
    let marker: &[u8] = b"tool_reference";
    if message.role() != Role::User || !message.json().windows(marker.len()).any(|w| w == marker) {
        return;
    }
    for block in message.blocks().unwrap_or_default() {
        let ContentBlock::ToolResult { content: Some(eos_agent_types::ToolResultContent::Blocks(blocks)), .. } = block
        else {
            continue;
        };
        for inner in blocks {
            if let ContentBlock::Unknown(raw) = inner
                && let Ok(r) = serde_json::from_str::<Reference>(raw.get())
                && r.kind == "tool_reference"
            {
                names.insert(r.tool_name);
            }
        }
    }
}

/// `None` when the token fires first.
async fn until_cancelled<F: Future>(cancel: &CancellationToken, fut: F) -> Option<F::Output> {
    let cancelled = pin!(cancel.cancelled());
    let fut = pin!(fut);
    match select(cancelled, fut).await {
        Either::Left(_) => None,
        Either::Right((out, _)) => Some(out),
    }
}
