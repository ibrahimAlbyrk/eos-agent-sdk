//! Runs many agents in one process.
//!
//! Each agent is a tokio task that sleeps on its command channel between
//! turns, so an idle agent costs its history and a few hundred bytes, never a
//! thread or a process. With a transcript store, an agent idle longer than
//! [`EngineBuilder::idle_eviction`] keeps even its history only on disk.
//! Dropping the [`Engine`] and every [`AgentHandle`] ends all agent tasks.

mod handle;
mod store;
mod task;

pub use handle::{AgentHandle, AgentUpdate, Gone, LocalTurn, LocalWork, TurnOverrides, TurnResult, WeakAgentHandle};
pub use store::JsonlStore;

use handle::Shared;
use task::ChannelSink;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eos_agent_core::{
    Agent, AgentConfig, AgentEvent, AgentIdent, AllowAll, BoxFuture, Clock, Extensions, InternalPaths, LiveRequest,
    ModelClient, PermissionGate, SessionContext, StoredMessage, SystemPrompt, Toolbox, TranscriptStore, Wake,
    Workspace,
};
use tokio::sync::{mpsc, watch};

/// Events buffered per agent before the agent waits for the host to read.
const EVENT_BUFFER: usize = 256;
/// How long an agent sits idle before its history leaves memory.
pub const DEFAULT_IDLE_EVICTION: Duration = Duration::from_secs(10 * 60);

/// Shared services every agent runs on.
#[derive(Clone)]
pub struct Engine {
    model: Arc<dyn ModelClient>,
    permissions: Arc<dyn PermissionGate>,
    store: Option<Arc<dyn TranscriptStore>>,
    clock: Arc<dyn Clock>,
    idle_eviction: Option<Duration>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct EngineBuilder {
    engine: Engine,
}

impl Engine {
    pub fn builder(model: Arc<dyn ModelClient>) -> EngineBuilder {
        EngineBuilder {
            engine: Engine {
                model,
                permissions: Arc::new(AllowAll),
                store: None,
                clock: Arc::new(TokioClock),
                idle_eviction: Some(DEFAULT_IDLE_EVICTION),
            },
        }
    }

    /// The model client every agent of this engine sends through.
    pub fn model(&self) -> &Arc<dyn ModelClient> {
        &self.model
    }

    /// Starts an agent. Drop the event receiver if you do not want events;
    /// the agent then never waits on delivery.
    pub fn spawn(&self, spec: AgentSpec) -> (AgentHandle, mpsc::Receiver<AgentEvent>) {
        let id = spec.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let agent = Agent::new(id.clone(), spec.config, spec.system, spec.toolbox)
            .with_history(spec.history)
            .with_extensions(spec.extensions);
        let agent = match spec.session {
            Some(context) => agent.with_session_context(context),
            None => agent,
        };
        let scratch = spec.scratch.unwrap_or_else(|| std::env::temp_dir().join("eos-agent").join(&id));
        agent.extensions().insert(Workspace::new(spec.cwd, spec.roots, scratch).with_internal(spec.internal));
        agent.extensions().insert(AgentIdent(id.clone()));
        let (commands, inbox) = mpsc::channel(8);
        let (events_tx, events) = mpsc::channel(EVENT_BUFFER);
        let shared = Arc::new(Shared {
            current: Mutex::new(None),
            pending: watch::Sender::new(0),
            evicted: Default::default(),
            wake_queued: Default::default(),
            extensions: agent.shared_extensions(),
            live: agent.extensions().get_or_default::<LiveRequest>(),
        });
        let ready = spec.ready;
        let handle = AgentHandle { id: id.into(), commands, shared: Arc::clone(&shared) };
        // Unless its host wakes it another way (a subagent, through its
        // registry), a note from its own background work starts a turn.
        if agent.extensions().get::<Wake>().is_none() {
            let weak = handle.downgrade();
            agent.extensions().insert(Wake::new(move || {
                if let Some(handle) = weak.upgrade() {
                    handle.wake();
                }
            }));
        }
        tokio::spawn(task::run(agent, self.clone(), inbox, ChannelSink(events_tx), shared, ready));
        (handle, events)
    }
}

impl EngineBuilder {
    pub fn permissions(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.engine.permissions = gate;
        self
    }

    pub fn store(mut self, store: Arc<dyn TranscriptStore>) -> Self {
        self.engine.store = Some(store);
        self
    }

    /// How long an agent may sit idle before its history leaves memory
    /// (`None` = never). Only applies with a [`store`](Self::store), from
    /// which the next turn reads the history back.
    pub fn idle_eviction(mut self, after: Option<Duration>) -> Self {
        self.engine.idle_eviction = after;
        self
    }

    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.engine.clock = clock;
        self
    }

    pub fn build(self) -> Engine {
        self.engine
    }
}

/// How to start one agent.
#[derive(Debug)]
pub struct AgentSpec {
    /// A fixed id (e.g. to resume); a fresh UUID when `None`.
    pub id: Option<String>,
    pub config: AgentConfig,
    pub system: SystemPrompt,
    pub toolbox: Arc<Toolbox>,
    pub history: Vec<StoredMessage>,
    /// Working directory (also the first root the agent's tools may touch).
    pub cwd: PathBuf,
    /// More directories the tools may touch.
    pub roots: Vec<PathBuf>,
    /// Where tools keep this agent's spilled output and task logs
    /// (default: a per-agent folder in the system temp dir).
    pub scratch: Option<PathBuf>,
    /// The engine's files the tools may read or write outside the roots.
    pub internal: InternalPaths,
    /// State the agent starts with (the engine adds its [`Workspace`]).
    pub extensions: Extensions,
    /// Sent with the first message of a new conversation.
    pub session: Option<SessionContext>,
    /// The agent rebuilds its tools' state from its history once this fires
    /// (the host has registered it by then); at once without one.
    pub ready: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl AgentSpec {
    pub fn new(config: AgentConfig, system: SystemPrompt, toolbox: Arc<Toolbox>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Self {
            id: None,
            config,
            system,
            toolbox,
            history: Vec::new(),
            cwd,
            roots: Vec::new(),
            scratch: None,
            internal: InternalPaths::default(),
            extensions: Extensions::default(),
            session: None,
            ready: None,
        }
    }
}

/// Real time on the tokio timer.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn sleep(&self, duration: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(duration))
    }
}
