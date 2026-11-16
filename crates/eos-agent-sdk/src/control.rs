//! What a host changes about a live agent: its model and effort, where its
//! conversation ends (rewind, fork), and its background tasks.

use std::sync::{Arc, PoisonError};

use eos_agent_core::{
    AgentEvent, RewindError, StopOrigin, StopRefused, StoredMessage, TokenSource, TurnError, check_cut,
};
use eos_agent_engine::{AgentUpdate, Gone};
use tokio::sync::mpsc;

use crate::agent::SdkAgent;
use crate::auth::SessionAuth;
use crate::options::AgentOptions;
use crate::profile::Provider;
use crate::registry::Blueprint;
use crate::sdk::{Sdk, SdkError};
use crate::spawn::{ModelSetup, model_setup};

/// Why a live agent could not be changed.
#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error(transparent)]
    Gone(#[from] Gone),
    /// The conversation did not fit the new model and could not be summarized first.
    #[error("the conversation could not be summarized for the new model: {0}")]
    Compact(TurnError),
    #[error(transparent)]
    Rewind(#[from] RewindError),
    #[error("could not read the conversation: {0}")]
    Store(std::io::Error),
    #[error(transparent)]
    Sdk(#[from] SdkError),
}

/// When an agent summarizes its conversation on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCompact {
    /// Never: only when the host calls [`SdkAgent::compact`].
    Off,
    /// Once the live context reaches this many tokens, and when a request
    /// no longer fits the model's window.
    At(u64),
}

/// What changes with an agent's model besides the model.
#[derive(Default)]
pub struct ModelChange {
    /// The agent's own credential for the new model's provider (`None`: the
    /// one it has when the provider stays, else the SDK's for that provider).
    pub auth: Option<Arc<dyn TokenSource>>,
    /// When the conversation is summarized from now on (`None`: as it is).
    pub compaction: Option<AutoCompact>,
}

impl std::fmt::Debug for ModelChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelChange").field("compaction", &self.compaction).finish_non_exhaustive()
    }
}

/// Why a background task was not stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopTaskError {
    /// Nothing goes by that handle in the agent's tree (the id).
    #[error("No task found with ID: {0}")]
    NotFound(String),
    #[error("Task {id} is not running (status: {status})")]
    NotRunning { id: String, status: &'static str },
}

/// A background task a host stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedTask {
    /// The agent whose task it was (a subagent is its own).
    pub agent_id: String,
    /// The task's own id (a subagent's agent id, also when it was named).
    pub task_id: String,
    /// Claude Code's task type: `local_bash` (a background command or a
    /// monitor), `local_workflow`, `local_agent` (a subagent).
    pub task_type: String,
    /// Its command, or its description.
    pub description: String,
}

impl SdkAgent {
    fn blueprint(&self) -> Result<Arc<Blueprint>, Gone> {
        self.services.registry.blueprint(self.handle.id()).ok_or(Gone)
    }

    /// The model the agent runs on now.
    pub fn model(&self) -> Option<String> {
        self.blueprint().ok().map(|b| b.config.model.clone())
    }

    /// The effort its requests ask for now (`None`: the model takes none).
    pub fn effort(&self) -> Option<String> {
        self.blueprint().ok().and_then(|b| b.config.effort.clone())
    }

    /// Runs the agent on `model` (an id or alias) from its next turn on:
    /// Claude Code's settings, prompt wording and tool wording for that
    /// model, within what the agent was spawned with (its own system prompt,
    /// tools, limits). It holds until changed again, through history
    /// eviction, and subagents started later inherit it; ones already
    /// running keep theirs. A host that spawns the agent again (to resume
    /// it) passes the model itself.
    pub async fn set_model(&self, model: &str) -> Result<(), ControlError> {
        self.set_model_with(model, ModelChange::default()).await.map(|_| ())
    }

    /// [`set_model`](Self::set_model), also onto another provider. The
    /// conversation is kept as it was written: the new provider gets it
    /// without the reasoning of the others (which only they can read), and
    /// each gets its own back whenever the agent returns to it. A
    /// conversation too long for the new model is summarized first, by the
    /// model it is on. True when it was.
    pub async fn set_model_with(&self, model: &str, change: ModelChange) -> Result<bool, ControlError> {
        let services = &self.services;
        let blueprint = self.blueprint()?;
        let model = services.resolve_model(model);
        let asked = match change.compaction {
            Some(compaction) => Arc::new(blueprint.asked.with_compaction(compaction)),
            None => Arc::clone(&blueprint.asked),
        };
        let effort = blueprint.session_effort.as_deref();
        let ModelSetup { mut config, system, recipe, context_window } = model_setup(
            services,
            &blueprint.project,
            &asked,
            self.handle.id(),
            &model,
            effort,
            blueprint.thinking_summaries,
        )?;
        let provider = Provider::id_of(&model);
        let moved = Provider::id_of(&blueprint.config.model) != provider;
        let own = || services.own_auth(&model).filter(|_| moved);
        let session = change
            .auth
            .or_else(own)
            .map(|source| SessionAuth::new(self.handle.id(), source, services.auth_failover.clone()));
        config.auth = match &session {
            Some(session) => Some(session.for_requests(provider.clone())),
            None => blueprint.config.auth.clone().filter(|_| !moved),
        };
        let fits = config.compaction.as_ref().and_then(|c| c.threshold_tokens).unwrap_or(context_window);
        let held: u64 = self.transcript().await?.iter().map(StoredMessage::estimated_tokens).sum();
        let summarized = held >= fits && self.handle.compact(None).await?.map_err(ControlError::Compact)?;
        let toolbox = match &blueprint.mcp_tools {
            Some(updates) => updates.set_recipe(recipe),
            None => blueprint.project.toolbox(&recipe, Vec::new()),
        };
        config.tool_source.clone_from(&blueprint.config.tool_source);
        let update = AgentUpdate {
            config: Some(config.clone()),
            system: Some(system.clone()),
            toolbox: Some(Arc::clone(&toolbox)),
        };
        self.handle.reconfigure(update).await?;
        if let Some(session) = session {
            *self.auth.write().unwrap_or_else(PoisonError::into_inner) = Some(session);
        }
        let changed = Blueprint { config, system, toolbox, context_window, asked, ..(*blueprint).clone() };
        services.registry.set_blueprint(self.handle.id(), Arc::new(changed));
        Ok(summarized)
    }

    /// Asks for `effort` (`low` … `max`; `None`: the model's default, as
    /// Claude Code sets it) from the agent's next turn on, as far as its
    /// model takes it. Holds like [`set_model`](Self::set_model), and through
    /// a later model change.
    pub async fn set_effort(&self, effort: Option<&str>) -> Result<(), Gone> {
        let blueprint = self.blueprint()?;
        let mut config = blueprint.config.clone();
        config.effort = self.services.effort_for(&config.model, effort);
        self.handle.reconfigure(AgentUpdate { config: Some(config.clone()), ..AgentUpdate::default() }).await?;
        let changed = Blueprint { config, session_effort: effort.map(str::to_owned), ..(*blueprint).clone() };
        self.services.registry.set_blueprint(self.handle.id(), Arc::new(changed));
        Ok(())
    }

    /// Changes when the agent summarizes its conversation on its own, from
    /// its next turn on. Holds like [`set_model`](Self::set_model), and
    /// through a later model change that does not say otherwise.
    pub async fn set_auto_compact(&self, change: AutoCompact) -> Result<(), Gone> {
        let blueprint = self.blueprint()?;
        let asked = Arc::new(blueprint.asked.with_compaction(change));
        let mut config = blueprint.config.clone();
        config.compaction = Some(asked.compaction(blueprint.context_window, config.max_tokens, asked.compact_at));
        self.handle.reconfigure(AgentUpdate { config: Some(config.clone()), ..AgentUpdate::default() }).await?;
        let changed = Blueprint { config, asked, ..(*blueprint).clone() };
        self.services.registry.set_blueprint(self.handle.id(), Arc::new(changed));
        Ok(())
    }

    /// The conversation as it is now, after any queued turn: what
    /// [`rewind`](Self::rewind) and [`fork_at`](Self::fork_at) count
    /// messages in.
    pub async fn transcript(&self) -> Result<Vec<StoredMessage>, ControlError> {
        self.handle.transcript().await?.map_err(ControlError::Store)
    }

    /// Cuts the conversation back to its first `keep` messages (after any
    /// queued turn), in memory and in the transcript store, and goes on from
    /// there: a rewind to the message at index `keep`, which is dropped with
    /// everything after it. `keep` may not fall between a tool call and its
    /// result. Files the dropped turns changed stay as they are.
    pub async fn rewind(&self, keep: usize) -> Result<(), ControlError> {
        Ok(self.handle.rewind(keep).await??)
    }

    /// Starts another agent from the first `keep` messages of this one's
    /// conversation (all of it with `keep` at its length), which goes on
    /// unchanged. `options` are the new agent's, as for [`Sdk::spawn`]; its
    /// `history` is replaced.
    pub async fn fork_at(
        &self,
        keep: usize,
        options: AgentOptions,
    ) -> Result<(SdkAgent, mpsc::Receiver<AgentEvent>), ControlError> {
        let mut history = self.transcript().await?;
        check_cut(&history, keep)?;
        history.truncate(keep);
        let sdk = Sdk { services: Arc::clone(&self.services) };
        Ok(sdk.spawn(options.history(history)).await?)
    }

    /// Stops one background task of this agent or of a subagent below it;
    /// see [`Sdk::stop_task`].
    pub async fn stop_task(&self, task_id: &str) -> Result<StoppedTask, StopTaskError> {
        Sdk { services: Arc::clone(&self.services) }.stop_task(self.handle.id(), task_id).await
    }
}

impl Sdk {
    /// Stops one background task of `agent_id` or of an agent below it, for
    /// the host's user, and waits until it has ended: a background command
    /// or monitor or a workflow run (by the id its events carry), or a
    /// subagent (by its agent id or the name it was started with, as
    /// TaskStop takes them). A subagent is stopped while it works or while
    /// it waits on its own background work: its turn is interrupted, its
    /// commands, monitors and workflow runs end, it is never woken again,
    /// and another agent's message no longer resumes it. Its parent is told
    /// it "was stopped by user" (with what it had written, as a
    /// notification that wakes an idle parent); the host sees its turn end
    /// as `killed`. One that was waiting is stopped with every subagent
    /// below it, and its parent, already told it finished, hears nothing
    /// more. Workflow agents are stopped with their run.
    pub async fn stop_task(&self, agent_id: &str, task_id: &str) -> Result<StoppedTask, StopTaskError> {
        let stop = self.services.subagents.stop_task(agent_id, task_id, StopOrigin::Host).await;
        match stop {
            Some(Ok(stopped)) => Ok(StoppedTask {
                agent_id: stopped.owner,
                task_id: stopped.task_id,
                task_type: stopped.task_type,
                description: stopped.description,
            }),
            Some(Err(StopRefused::NotRunning { id, status })) => Err(StopTaskError::NotRunning { id, status }),
            Some(Err(_)) | None => Err(StopTaskError::NotFound(task_id.to_owned())),
        }
    }
}
