//! The host's grip on one running agent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{
    AgentConfig, BoxFuture, CancellationToken, Extensions, LiveRequest, ModelRequest, RewindError, StoredMessage,
    SystemPrompt, Toolbox, TurnError, TurnOutcome,
};
use eos_agent_types::ContentBlock;
use tokio::sync::{mpsc, oneshot, watch};

pub type TurnResult = Result<TurnOutcome, TurnError>;

// Every agent's command channel holds slots the size of the largest
// variant; boxing the turn keeps them small.
pub(crate) enum Command {
    Turn(Box<TurnRequest>),
    /// A note for the model; `wake` runs a turn with it if the agent is idle.
    Notify(String, bool),
    /// Runs a turn on the notes its [`Wake`](eos_agent_core::Wake) asked
    /// for, unless a request read them meanwhile.
    Wake,
    /// Summarize the conversation now, with the user's `focus` if any.
    Compact(Box<CompactRequest>),
    /// Start the conversation over from the host's seed.
    ReplaceContext(Vec<ContentBlock>, oneshot::Sender<Result<(), TurnError>>),
    /// A turn whose work runs here instead of a model request.
    Local(Box<LocalRequest>),
    /// The request the agent would send now.
    Context(oneshot::Sender<std::io::Result<ModelRequest>>),
    /// Carry on the turn the conversation stopped in.
    Resume(oneshot::Sender<Result<Option<TurnOutcome>, TurnError>>),
    /// New settings, prompt or tools from the next turn on.
    Reconfigure(Box<AgentUpdate>, oneshot::Sender<()>),
    /// Cut the conversation back to its first messages.
    Rewind(usize, oneshot::Sender<Result<(), RewindError>>),
    /// The conversation as it is now.
    Transcript(oneshot::Sender<std::io::Result<Vec<StoredMessage>>>),
}

/// What changes for an agent from its next turn on; `None` keeps a part.
#[derive(Debug, Default)]
pub struct AgentUpdate {
    pub config: Option<AgentConfig>,
    pub system: Option<SystemPrompt>,
    pub toolbox: Option<Arc<Toolbox>>,
}

pub(crate) struct LocalRequest {
    pub(crate) work: LocalWork,
    pub(crate) reply: oneshot::Sender<TurnResult>,
}

/// What a turn that made no model request adds to the conversation.
#[derive(Debug, Clone, Default)]
pub struct LocalTurn {
    /// The turn's user input, read by the model with the next request.
    pub input: Vec<ContentBlock>,
    /// The work was stopped by [`AgentHandle::interrupt`].
    pub interrupted: bool,
}

/// Work standing in for a turn's model request (a slash command that runs a
/// subagent). It runs as the agent's turn, given the turn's cancellation.
pub type LocalWork = Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, LocalTurn> + Send>;

pub(crate) struct CompactRequest {
    pub(crate) focus: Option<String>,
    pub(crate) reply: oneshot::Sender<Result<bool, TurnError>>,
}

pub(crate) struct TurnRequest {
    pub(crate) input: Vec<ContentBlock>,
    pub(crate) overrides: TurnOverrides,
    pub(crate) reply: oneshot::Sender<TurnResult>,
}

/// Settings that hold for one turn only (a slash command's `model:` and
/// `effort:`), then revert.
#[derive(Debug, Clone, Default)]
pub struct TurnOverrides {
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Whether the turn's model takes mid-conversation system messages.
    pub system_turns: Option<bool>,
}

impl TurnOverrides {
    /// Applies these to `config` and returns what undoes them.
    pub(crate) fn apply(self, config: &mut AgentConfig) -> Restore {
        Restore {
            model: self.model.map(|m| std::mem::replace(&mut config.model, m)),
            effort: self.effort.map(|e| config.effort.replace(e)),
            system_turns: self.system_turns.map(|t| std::mem::replace(&mut config.system_turns, t)),
        }
    }
}

/// The settings a turn's overrides replaced.
pub(crate) struct Restore {
    model: Option<String>,
    effort: Option<Option<String>>,
    system_turns: Option<bool>,
}

impl Restore {
    pub(crate) fn apply(self, config: &mut AgentConfig) {
        if let Some(model) = self.model {
            config.model = model;
        }
        if let Some(effort) = self.effort {
            config.effort = effort;
        }
        if let Some(turns) = self.system_turns {
            config.system_turns = turns;
        }
    }
}

/// State the agent task and its handles share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) current: Mutex<Option<CancellationToken>>,
    /// Commands sent and not yet finished.
    pub(crate) pending: watch::Sender<usize>,
    /// The history is out of memory, kept only by the transcript store.
    pub(crate) evicted: AtomicBool,
    /// A [`Command::Wake`] is queued; later wakes join it.
    pub(crate) wake_queued: AtomicBool,
    /// The agent's own state for its tools.
    pub(crate) extensions: Arc<Extensions>,
    /// The running turn's latest model request.
    pub(crate) live: Arc<LiveRequest>,
}

impl Shared {
    pub(crate) fn done(&self) {
        self.pending.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Cloning shares the agent; it runs until the engine and every clone are
/// dropped. [`WeakAgentHandle`] reaches it without keeping it alive.
#[derive(Debug, Clone)]
pub struct AgentHandle {
    pub(crate) id: Arc<str>,
    pub(crate) commands: mpsc::Sender<Command>,
    pub(crate) shared: Arc<Shared>,
}

#[derive(Debug, thiserror::Error)]
#[error("the agent has stopped")]
pub struct Gone;

impl AgentHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Runs one turn and waits for its end.
    pub async fn send(&self, input: Vec<ContentBlock>) -> Result<TurnResult, Gone> {
        self.send_with(input, TurnOverrides::default()).await
    }

    /// Runs one turn with settings that revert after it.
    pub async fn send_with(&self, input: Vec<ContentBlock>, overrides: TurnOverrides) -> Result<TurnResult, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Turn(Box::new(TurnRequest { input, overrides, reply }))).await?;
        rx.await.map_err(|_| Gone)
    }

    pub async fn send_text(&self, text: impl Into<String>) -> Result<TurnResult, Gone> {
        self.send(vec![ContentBlock::Text { text: text.into(), citations: None }]).await
    }

    /// Runs `work` as a turn that makes no model request (Claude Code's
    /// `shouldQuery: false`), then records what it returns. It waits behind
    /// queued turns, and [`interrupt`](Self::interrupt) cancels it.
    pub async fn run_local(&self, work: LocalWork) -> Result<TurnResult, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Local(Box::new(LocalRequest { work, reply }))).await?;
        rx.await.map_err(|_| Gone)
    }

    /// Carries on a turn whose request failed before the model answered (a
    /// plan limit, a sign-in that ran out) once that is fixed: the model is
    /// asked again with no new input. `None` when the conversation does not
    /// end with a user message, so there is nothing to carry on.
    pub async fn resume(&self) -> Result<Result<Option<TurnOutcome>, TurnError>, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Resume(reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// The agent's context as a model request: its system prompt, tools and
    /// conversation, as its own next request would carry them (so a request
    /// built on it reads the agent's prompt cache). While a turn runs this
    /// is the turn's latest request, without waiting for the turn.
    pub async fn context_request(&self) -> Result<std::io::Result<ModelRequest>, Gone> {
        if let Some(request) = self.shared.live.get() {
            return Ok(Ok(request));
        }
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Context(reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// Leaves a note for the model (a finished background task, a message
    /// from another agent). Delivered with the next request; with `wake`, an
    /// idle agent starts a turn for it right away.
    pub async fn notify(&self, text: impl Into<String>, wake: bool) -> Result<(), Gone> {
        self.submit(Command::Notify(text.into(), wake)).await
    }

    /// Asks for a turn on the notes the agent's background work left (see
    /// [`Wake`](eos_agent_core::Wake)). It runs once the agent is idle, and
    /// only if no request has read them by then; wakes that arrive before it
    /// runs make one turn.
    pub fn wake(&self) {
        if self.shared.wake_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        // Counted at once, so the agent is never seen idle with a wake on its way.
        self.shared.pending.send_modify(|n| *n += 1);
        match self.commands.try_send(Command::Wake) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(command)) => {
                let (commands, shared) = (self.commands.clone(), Arc::clone(&self.shared));
                tokio::spawn(async move {
                    if commands.send(command).await.is_err() {
                        shared.done();
                    }
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => self.shared.done(),
        }
    }

    async fn submit(&self, command: Command) -> Result<(), Gone> {
        self.shared.pending.send_modify(|n| *n += 1);
        let sent = self.commands.send(command).await;
        if sent.is_err() {
            self.shared.done();
        }
        sent.map_err(|_| Gone)
    }

    /// Replaces the conversation with a summary of it (after any queued
    /// turn). False when there was nothing to summarize.
    pub async fn compact(&self, focus: Option<String>) -> Result<Result<bool, TurnError>, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Compact(Box::new(CompactRequest { focus, reply }))).await?;
        rx.await.map_err(|_| Gone)
    }

    /// Replaces the conversation with `seed` (after any queued turn), in
    /// memory and in the transcript store; it runs no turn. See
    /// [`Agent::replace_context`](eos_agent_core::Agent::replace_context).
    pub async fn replace_context(&self, seed: Vec<ContentBlock>) -> Result<Result<(), TurnError>, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::ReplaceContext(seed, reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// Changes the agent's settings, system prompt or tools for every later
    /// turn (after any queued one); returns once they hold.
    pub async fn reconfigure(&self, update: AgentUpdate) -> Result<(), Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Reconfigure(Box::new(update), reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// Cuts the conversation back to its first `keep` messages (after any
    /// queued turn), in memory and in the transcript store; see
    /// [`Agent::truncate`](eos_agent_core::Agent::truncate).
    pub async fn rewind(&self, keep: usize) -> Result<Result<(), RewindError>, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Rewind(keep, reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// The conversation as it is now (after any queued turn), read back from
    /// the transcript store when it is out of memory. Messages share their
    /// bytes with the agent's.
    pub async fn transcript(&self) -> Result<std::io::Result<Vec<StoredMessage>>, Gone> {
        let (reply, rx) = oneshot::channel();
        self.submit(Command::Transcript(reply)).await?;
        rx.await.map_err(|_| Gone)
    }

    /// The agent's own state for its tools (its background tasks, its
    /// working directory, what a host seeded it with), reachable while a
    /// turn runs.
    pub fn extensions(&self) -> &Extensions {
        &self.shared.extensions
    }

    /// Stops the running turn (if any) at the next safe point.
    pub fn interrupt(&self) {
        if let Some(token) = self.shared.current.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            token.cancel();
        }
    }

    /// No turn running and nothing queued.
    pub fn is_idle(&self) -> bool {
        *self.shared.pending.borrow() == 0
    }

    /// Its history was dropped from memory after it sat idle; the next turn
    /// reads it back from the transcript store.
    pub fn is_evicted(&self) -> bool {
        self.shared.evicted.load(Ordering::Relaxed)
    }

    /// Waits until the agent has nothing running or queued.
    pub async fn wait_idle(&self) {
        let mut rx = self.shared.pending.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }

    pub fn downgrade(&self) -> WeakAgentHandle {
        WeakAgentHandle {
            id: Arc::clone(&self.id),
            commands: self.commands.downgrade(),
            shared: Arc::downgrade(&self.shared),
        }
    }
}

/// Reaches an agent while something else keeps it alive.
#[derive(Debug, Clone)]
pub struct WeakAgentHandle {
    id: Arc<str>,
    commands: mpsc::WeakSender<Command>,
    shared: std::sync::Weak<Shared>,
}

impl WeakAgentHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn upgrade(&self) -> Option<AgentHandle> {
        Some(AgentHandle {
            id: Arc::clone(&self.id),
            commands: self.commands.upgrade()?,
            shared: self.shared.upgrade()?,
        })
    }
}
