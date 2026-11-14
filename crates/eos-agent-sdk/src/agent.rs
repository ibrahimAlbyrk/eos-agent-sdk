//! A top-level agent as the host holds it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eos_agent_core::SubagentStart;
use eos_agent_core::{CancellationToken, Extensions, TokenSource, ToolContext, TurnError, TurnOutcome, Workspace};
use eos_agent_engine::{AgentHandle, Gone, LocalTurn, LocalWork, TurnOverrides, TurnResult};
use eos_agent_mcp::{McpServerStatus, McpToolset};
use eos_agent_skills::Origin as SkillOrigin;
use eos_agent_skills::{
    ExpandContext, ExpandError, Expansion, NoCommands, background_fork_request, fork_request, forked_command_id,
    forked_command_input, forked_command_launched, typed_line,
};
use eos_agent_types::ContentBlock;

use crate::auth::SessionAuth;
use crate::profile::Provider;
use crate::project::Project;
use crate::services::Services;
use crate::side::SideError;

/// One top-level agent. Dropping it stops the agent and every subagent it
/// started, and releases its MCP servers.
pub struct SdkAgent {
    pub(crate) handle: AgentHandle,
    pub(crate) services: Arc<Services>,
    pub(crate) project: Arc<Project>,
    /// Keeps the agent's MCP servers connected.
    _mcp: Option<Arc<McpToolset>>,
    /// The workflow authoring reference was loaded for "ultracode" once.
    authoring_loaded: AtomicBool,
    /// The credential the agent's tree runs on.
    auth: Option<Arc<SessionAuth>>,
}

impl std::fmt::Debug for SdkAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkAgent").field("id", &self.handle.id()).finish_non_exhaustive()
    }
}

/// A skill or slash command an agent can run, as a host's menu shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    /// What follows the `/` (`deploy`, `plugin:deploy`).
    pub name: String,
    pub description: String,
    /// What the command says about its arguments (`[issue number]`).
    pub argument_hint: Option<String>,
    /// Where it comes from: bundled, the user's or the project's files, a plugin.
    pub source: SkillOrigin,
    /// A user may type it as `/name` (what [`SdkAgent::send_input`] expands).
    pub user_invocable: bool,
    /// The model sees it in its skill listing and may load it itself.
    pub model_invocable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error(transparent)]
    Gone(#[from] Gone),
    #[error(transparent)]
    Command(#[from] ExpandError),
}

impl SdkAgent {
    pub(crate) fn new(
        handle: AgentHandle,
        services: Arc<Services>,
        project: Arc<Project>,
        mcp: Option<Arc<McpToolset>>,
        auth: Option<Arc<SessionAuth>>,
    ) -> Self {
        Self { handle, services, project, _mcp: mcp, authoring_loaded: AtomicBool::new(false), auth }
    }

    pub fn id(&self) -> &str {
        self.handle.id()
    }

    /// The skills and slash commands this agent can run (the ones found for
    /// its directory and plugin folders when it started), in Claude Code's
    /// order of precedence.
    pub fn commands(&self) -> Vec<CommandInfo> {
        let command = |skill: Arc<eos_agent_skills::Skill>| CommandInfo {
            name: skill.name.clone(),
            description: skill.description.clone(),
            argument_hint: skill.argument_hint.clone(),
            source: skill.origin.clone(),
            user_invocable: skill.user_invocable,
            model_invocable: skill.is_listed(),
        };
        self.project.skills.skills().into_iter().map(command).collect()
    }

    /// The agent's MCP servers as Claude Code's `/mcp` lists them (empty
    /// when it was started without MCP).
    pub fn mcp_status(&self) -> Vec<McpServerStatus> {
        self._mcp.as_ref().map(|m| m.status()).unwrap_or_default()
    }

    /// `/mcp` "Reconnect": a fresh connection to `server` (its tools reach
    /// the agent before its next request).
    pub async fn mcp_reconnect(&self, server: &str) -> Result<(), String> {
        let mcp = self._mcp.as_ref().ok_or("This agent has no MCP servers.")?;
        mcp.reconnect(server).await
    }

    /// `/mcp` "Clear authentication": revokes `server`'s stored sign-in at
    /// its authorization server, forgets it and connects again (the server
    /// then waits for a new sign-in). `Ok(false)` for a server without OAuth.
    pub async fn mcp_sign_out(&self, server: &str) -> Result<bool, String> {
        let mcp = self._mcp.as_ref().ok_or("This agent has no MCP servers.")?;
        mcp.sign_out(server).await
    }

    /// The engine handle (clone it to send from elsewhere; the agent still
    /// stops when this [`SdkAgent`] is dropped).
    pub fn handle(&self) -> &AgentHandle {
        &self.handle
    }

    pub async fn send(&self, input: Vec<ContentBlock>) -> Result<TurnResult, Gone> {
        self.handle.send(input).await
    }

    /// Sends what a user typed (also see [`send_input`](Self::send_input)).
    pub async fn send_text(&self, text: impl Into<String>) -> Result<TurnResult, Gone> {
        let text = text.into();
        self.before_turn(&text).await;
        self.handle.send_text(text).await
    }

    /// What Claude Code does with a typed prompt before the turn: the
    /// "+500k" token target for workflows, and the "ultracode" keyword's
    /// reminder with the workflow authoring reference.
    async fn before_turn(&self, text: &str) {
        let services = &self.services;
        if services.workflows.is_none() {
            return;
        }
        services.budgets.start_turn(self.handle.id(), crate::workflows::token_target(text));
        let has_workflow =
            services.registry.blueprint(self.handle.id()).is_some_and(|b| b.toolbox.get("Workflow").is_some());
        if !services.workflow_keyword || !has_workflow || !crate::workflows::has_keyword(text, "ultracode") {
            return;
        }
        let _ = self.handle.notify(crate::workflows::KEYWORD_REMINDER, false).await;
        if self.authoring_loaded.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut ctx = ExpandContext::new(&NoCommands);
        ctx.project_dir = Some(&self.project.cwd);
        ctx.session_id = Some(self.handle.id());
        if let Some(Ok(skill)) = self.project.skills.expand(&format!("/{}", eos_agent_workflow::SKILL), &ctx).await {
            for note in [skill.visible, skill.body].into_iter().filter(|t| !t.is_empty()) {
                let _ = self.handle.notify(note, false).await;
            }
        }
    }

    /// Sends what a user typed: `/name args` naming a user-invocable skill or
    /// command is expanded first (its visible line and instructions go in as
    /// the turn's message, its `model:` and `effort:` hold for this turn; a
    /// `context: fork` one runs as a subagent instead); anything else goes in
    /// as typed.
    pub async fn send_input(&self, text: &str) -> Result<TurnResult, InputError> {
        self.before_turn(text).await;
        let effort = self.handle_effort();
        // Inline commands run as the agent's Bash calls, in its directories.
        let runner = self.services.shell.runner();
        let extensions = Extensions::default();
        let blueprint = self.services.registry.blueprint(self.handle.id());
        let roots = blueprint.as_ref().map(|b| b.roots.clone()).unwrap_or_default();
        let session = blueprint.as_ref().map_or_else(|| self.handle.id().to_owned(), |b| b.task_list.to_string());
        let scratch = self.services.data_dir.join("scratch").join(self.handle.id());
        extensions.insert(Workspace::new(self.project.cwd.clone(), roots, scratch));
        extensions.insert(eos_agent_artifacts::ArtifactSession(session));
        if let Some(policy) = self.project.policy() {
            extensions.insert(eos_agent_tools_shell::AgentSandbox(Arc::clone(policy)));
        }
        let cancel = CancellationToken::new();
        let mut ctx = ExpandContext::new(&runner);
        ctx.project_dir = Some(&self.project.cwd);
        ctx.session_id = Some(self.handle.id());
        ctx.effort = effort.as_deref();
        ctx.call = Some(ToolContext {
            agent_id: self.handle.id(),
            tool_use_id: "slash-command",
            cancel: &cancel,
            extensions: &extensions,
            transcript: &[],
        });
        let Some(expansion) = self.project.skills.expand(text, &ctx).await else {
            return Ok(self.handle.send_text(text).await?);
        };
        let expansion = expansion?;
        if expansion.fork {
            return Ok(self.fork_command(expansion).await?);
        }
        let blocks = [expansion.visible, expansion.body]
            .into_iter()
            .filter(|t| !t.is_empty())
            .map(|text| ContentBlock::Text { text, citations: None })
            .collect();
        // A conversation never changes provider: a command's model on
        // another provider is not used (the agent's own model runs it).
        let current = self.services.registry.blueprint(self.handle.id()).map(|b| b.config.model.clone());
        let model = expansion
            .model
            .as_deref()
            .map(|m| self.services.resolve_model(m))
            .filter(|m| current.as_deref().is_none_or(|c| Provider::of(c) == Provider::of(m)));
        let system_turns = model.as_deref().map(|m| self.services.system_turns(m));
        let overrides = TurnOverrides { model, effort: expansion.effort, system_turns };
        Ok(self.handle.send_with(blocks, overrides).await?)
    }

    /// A typed `/name args` of a `context: fork` skill, as Claude Code runs
    /// it: a subagent runs the skill as this agent's turn, and the line as
    /// typed and the subagent's reply join the conversation with no model
    /// request (the model reads them with the next turn); an interrupt stops
    /// it. As Claude Code's interactive terminal, an [`interactive`] agent
    /// launches the subagent in the background instead (unless the skill says
    /// `background: false`, a fork of the same skill is still working,
    /// background tasks are off or the nesting limit is reached): the
    /// turn records the launch, and the result arrives as a task notification.
    ///
    /// [`interactive`]: crate::AgentOptions::interactive
    async fn fork_command(&self, expansion: Expansion) -> Result<TurnResult, Gone> {
        let subagents = Arc::clone(&self.services.subagents);
        let parent = self.handle.id().to_owned();
        let work: LocalWork = Box::new(move |cancel| {
            Box::pin(async move {
                let (name, args) = (expansion.name.clone(), expansion.args.clone());
                if let Some(as_name) = subagents.background_fork(&parent, &name, expansion.background) {
                    let input = match subagents.start(background_fork_request(expansion, &parent, &as_name)).await {
                        Ok(SubagentStart::Launched { agent_id }) => {
                            forked_command_launched(&name, &args, &agent_id, &as_name)
                        }
                        other => forked_command_input(&name, &args, other),
                    };
                    return LocalTurn { input, interrupted: false };
                }
                let started = subagents.start(fork_request(expansion, &parent, &forked_command_id(&name)));
                tokio::select! {
                    result = started => LocalTurn { input: forked_command_input(&name, &args, result), interrupted: false },
                    () = cancel.cancelled() => {
                        let line = ContentBlock::Text { text: typed_line(&name, &args), citations: None };
                        LocalTurn { input: vec![line], interrupted: true }
                    }
                }
            })
        });
        self.handle.run_local(work).await
    }

    fn handle_effort(&self) -> Option<String> {
        self.services.registry.blueprint(self.handle.id()).and_then(|b| b.config.effort.clone())
    }

    /// Puts this agent and its subagents on another credential (another
    /// sign-in, or an API key for this agent only), from their next request
    /// on. It must be one for the provider the agent's model runs on.
    pub fn set_auth(&self, source: Arc<dyn TokenSource>) {
        if let Some(auth) = &self.auth {
            auth.set(source);
        }
    }

    /// Carries on a turn whose request failed before the model answered (a
    /// plan limit, a sign-in that ran out), after [`set_auth`](Self::set_auth)
    /// or once the limit reset: the model is asked again with no new input,
    /// as Claude Code re-runs an interrupted turn. `None` when the
    /// conversation does not end with a user message.
    pub async fn resume(&self) -> Result<Result<Option<TurnOutcome>, TurnError>, Gone> {
        self.handle.resume().await
    }

    /// Claude Code's `/btw`: answers `question` from this agent's
    /// conversation as it stands, in one reply and with no tool run. The
    /// request is the agent's own (same system prompt, tools and messages,
    /// so it reads the agent's prompt cache, on the agent's credential) plus
    /// the question; neither the question nor the answer joins the
    /// conversation or the transcript. A running turn keeps going: the
    /// answer is drawn from what the turn last sent the model. Dropping the
    /// returned future cancels the question.
    pub async fn ask_side(&self, question: &str) -> Result<String, SideError> {
        crate::side::ask(&self.handle, &self.services, question).await
    }

    /// See [`AgentHandle::notify`].
    pub async fn notify(&self, text: impl Into<String>, wake: bool) -> Result<(), Gone> {
        self.handle.notify(text, wake).await
    }

    pub fn interrupt(&self) {
        self.handle.interrupt();
    }

    /// Summarizes the conversation now (`/compact [focus]`). False when there
    /// was nothing to summarize.
    pub async fn compact(&self, focus: Option<String>) -> Result<Result<bool, TurnError>, Gone> {
        self.handle.compact(focus).await
    }

    /// Starts the conversation over from the host's own summary of it,
    /// which runs no turn: the model reads it ahead of the next message, a
    /// rewind to that message keeps it, and it is no prompt of the user's
    /// ([`StoredMessage::is_meta`](eos_agent_core::StoredMessage::is_meta)).
    pub async fn replace_context(&self, seed: impl Into<String>) -> Result<Result<(), TurnError>, Gone> {
        let seed = ContentBlock::Text { text: seed.into(), citations: None };
        self.handle.replace_context(vec![seed]).await
    }

    /// Subagents of this agent (at any depth) still working.
    pub fn running_subagents(&self) -> usize {
        self.services.registry.running_below(self.handle.id())
    }

    /// Background tasks of this agent still running (workflow runs).
    pub fn running_tasks(&self) -> usize {
        let running = self.services.running_tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        running.get(self.handle.id()).copied().unwrap_or(0)
    }

    /// Waits until this agent is idle and no subagent or background task of
    /// it is working (their reports woke it and were answered).
    pub async fn settle(&self) {
        loop {
            self.handle.wait_idle().await;
            if self.running_subagents() == 0 && self.running_tasks() == 0 && self.handle.is_idle() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for SdkAgent {
    fn drop(&mut self) {
        if let Some(watcher) = &self.services.watcher {
            watcher.forget(self.handle.id());
        }
        self.services.budgets.forget(self.handle.id());
        self.services.registry.remove_tree(self.handle.id());
    }
}
