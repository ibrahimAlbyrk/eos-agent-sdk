//! The [`Subagents`] port over the engine: Agent, SendMessage and ListAgents
//! start, message and list real agents of the same process.

use std::sync::{Arc, Weak};

use eos_agent_agents::{AgentNotification, EXPLORE, ForkedAgent, Outcome, PLAN, StoppedBy};
use eos_agent_core::{
    AgentOrigin, BoxFuture, Extensions, StopOrigin, StopRefused, SubagentInfo, SubagentReport, SubagentRequest,
    SubagentStart, Subagents, TaskEvent, TaskNotices, TaskStopped, Worktree,
};
use eos_agent_engine::AgentHandle;
use eos_agent_skills::agent_name;
use eos_agent_types::ContentBlock;

use crate::child::{Forward, Listeners, RunEnd, SubagentStarted, Tracker};
use crate::profile::Provider;
use crate::registry::{Blueprint, ChildInfo, Stop, has_running_tasks, now_ms};
use crate::services::Services;
use crate::spawn::agent_spec;

mod plan;
mod stop;
mod wake;

use plan::{fork, fresh, session};

/// Address of the top-level conversation in SendMessage.
const MAIN: &str = "main";
pub(crate) struct EngineSubagents {
    services: Weak<Services>,
}

impl EngineSubagents {
    pub(crate) fn new(services: Weak<Services>) -> Self {
        Self { services }
    }

    async fn start_agent(&self, mut request: SubagentRequest) -> Result<SubagentStart, String> {
        let services = self.services.upgrade().ok_or("The agent service has shut down.")?;
        let registry = Arc::clone(&services.registry);
        let parent = registry.blueprint(&request.parent_id).ok_or("The calling agent is not running.")?;
        if request.isolation.as_deref() == Some("remote") {
            return Err("Isolation is not available in this session. Omit isolation to run the agent in the \
                        current working directory."
                .into());
        }
        let id = format!("a{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        // The Agent tool's worktree (a workflow makes its own and passes `cwd`).
        let mut worktree = None;
        if request.isolation.as_deref() == Some("worktree") && request.cwd.is_none() {
            let made = make_worktree(parent.project.cwd.clone(), format!("agent-{id}")).await?;
            request.cwd = Some(made.path.clone());
            worktree = Some(made);
        }
        let project = match &request.cwd {
            Some(cwd) => {
                let dir = tokio::fs::canonicalize(cwd).await.map_err(|e| format!("{}: {e}", cwd.display()))?;
                Some(services.project(&dir, &parent.project.plugin_dirs).await)
            }
            None => None,
        };
        let (mut blueprint, extensions, input, agent_type) =
            if request.fork { fork(&parent, &request)? } else { fresh(&services, &parent, &request)? };
        if let Some(project) = project {
            blueprint.project = project;
            // Isolated in its own worktree: its writes stay there, so agents
            // working in parallel cannot write over each other's files.
            blueprint.confined = true;
        }
        // A GPT subagent (a workflow's too) keys its own prompt cache: OpenAI
        // routes by the prefix's start and the key, and a tree on one key
        // overflows its ~15 requests a minute. A fork keeps the parent's key,
        // since it shares the parent's prefix.
        if !request.fork && Provider::of(&blueprint.config.model) == Provider::OpenAi {
            blueprint.config.cache_key = Some(Arc::from(id.as_str()));
        }
        let origin = AgentOrigin {
            root_id: registry.root(&request.parent_id).unwrap_or_else(|| Arc::from(request.parent_id.as_str())),
            parent_id: Arc::from(request.parent_id.as_str()),
            tool_use_id: Some(request.tool_use_id.as_str()).filter(|id| !id.is_empty()).map(Arc::from),
            // An agent started by a workflow's agent still belongs to the run.
            workflow: request.workflow.clone().or_else(|| registry.origin(&request.parent_id)?.workflow),
        };
        extensions.insert(origin.clone());
        wake::install(&services, &id, &extensions);
        let mut spec = agent_spec(&services, &blueprint, id.clone(), Some(extensions));
        if request.fork {
            spec.history = std::mem::take(&mut request.history);
        } else {
            // Claude Code's git snapshot, except for Explore and Plan.
            let git = ![EXPLORE, PLAN].contains(&agent_type.as_str());
            spec.session = Some(session(&services, &blueprint, git).await);
        }
        let model = blueprint.config.model.clone();
        let about = SubagentStarted {
            tool_use_id: request.tool_use_id.clone(),
            agent_type: agent_type.clone(),
            model: model.clone(),
            effort: blueprint.config.effort.clone(),
            description: request.description.clone(),
            name: request.name.clone(),
            background: request.background,
        };
        let forward = forward(&blueprint, &id, &request.parent_id, about);
        let (handle, events) = services.engine.spawn(spec);
        let listeners = Listeners { observer: request.observer.clone(), spend: spend(&services, &request.parent_id) };
        let tracker = Tracker::follow(events, forward, Arc::from(id.as_str()), listeners);
        if let Some(observer) = &request.observer {
            observer.started(&id, &model);
        }
        let info = ChildInfo {
            name: request.name,
            description: request.description.clone(),
            agent_type,
            tool_use_id: request.tool_use_id.clone(),
            origin,
            running: true,
            ended: "completed",
            ended_ms: 0,
            background: request.background,
            started_ms: now_ms(),
            workflow: request.workflow.is_some(),
            forked_skill: request.forked_skill.clone(),
            fork: request.fork,
            generation: 0,
            tracker: Arc::clone(&tracker),
            stop: None,
            stopped_by_user: false,
        };
        if !registry.insert_child(&request.parent_id, handle.clone(), Arc::new(blueprint), info) {
            return Err("The calling agent is not running.".into());
        }
        if request.background {
            tokio::spawn(run_in_background(services, handle, tracker, input, worktree));
            return Ok(SubagentStart::Launched { agent_id: id });
        }
        let mut stop_if_abandoned = StopIfAbandoned { handle: &handle, services: &services, armed: true };
        let end = if request.output_schema.is_some() {
            tracker.run_structured(&handle, input).await
        } else {
            tracker.run(&handle, input).await
        };
        stop_if_abandoned.armed = false;
        let stop = registry.stop_of(&id);
        let end = as_stopped(end, stop, &id);
        if stop.is_some() {
            stop::end_work(&services, &id, &handle).await;
        }
        registry.set_ended(&id, ended(&end));
        let generation = registry.set_running(&id, false);
        services.release_later(&id, generation);
        // A note of its own work that came after its last request.
        wake::wake(&services, &id);
        let kept = match worktree {
            Some(wt) => release_worktree(wt).await,
            None => None,
        };
        match end {
            RunEnd::Finished(mut report) => {
                report.worktree = kept.map(Box::new);
                Ok(SubagentStart::Finished(report))
            }
            RunEnd::Stopped(_) => Err(format!("Agent {id} was stopped before it finished.")),
            RunEnd::Failed(error) => Err(format!("Agent {id} failed: {error}")),
        }
    }

    async fn deliver(&self, from: &str, to: &str, message: String) -> Result<(), String> {
        let services = self.services.upgrade().ok_or("The agent service has shut down.")?;
        let registry = Arc::clone(&services.registry);
        if to == MAIN {
            let root = registry.root(from).ok_or("The calling agent is not running.")?;
            let main = registry.handle(&root).ok_or("The main conversation has ended.")?;
            return main.notify(message, false).await.map_err(|e| e.to_string());
        }
        let target = addressed(&registry.session_agents(from), to).ok_or_else(|| {
            format!("No agent named '{to}' is currently addressable. Spawn a new one or use the agent ID.")
        })?;
        if registry.child(&target.agent_id).is_some_and(|i| i.stopped_by_user && !i.running) {
            return Err(format!(
                "Agent {} was stopped by the user and won't be resumed. Treat its work as cancelled; only launch a \
                 new agent if the user explicitly asks.",
                target.agent_id
            ));
        }
        let handle = match registry.handle(&target.agent_id) {
            Some(handle) => handle,
            None if registry.is_released(&target.agent_id) => restart(&services, &target.agent_id).await?,
            None => return Err("That agent has ended.".into()),
        };
        // Claimed under the registry's lock, so a wake and a message never
        // both start a run: the one that loses reads the other's notes.
        let Some((handle, tracker)) = registry.start_run(&target.agent_id, false) else {
            // It works: it reads the message with its next request, or is
            // woken for it when its run ends.
            TaskNotices::of(handle.extensions()).push(message);
            return Ok(());
        };
        let input = vec![ContentBlock::Text { text: message, citations: None }];
        tokio::spawn(run_in_background(services, handle, tracker, input, None));
        Ok(())
    }
}

/// Stops a synchronous subagent whose caller stopped waiting (the parent's
/// turn was interrupted), and records it as stopped, so it no longer counts
/// as working.
struct StopIfAbandoned<'a> {
    handle: &'a AgentHandle,
    services: &'a Services,
    armed: bool,
}

impl Drop for StopIfAbandoned<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.handle.interrupt();
        let id = self.handle.id();
        self.services.registry.set_ended(id, "killed");
        let generation = self.services.registry.set_running(id, false);
        self.services.release_later(id, generation);
    }
}

/// Starts a released subagent again from its transcript.
async fn restart(services: &Arc<Services>, id: &str) -> Result<AgentHandle, String> {
    let registry = &services.registry;
    let (Some(blueprint), Some(info), Some(store)) = (registry.blueprint(id), registry.child(id), &services.store)
    else {
        return Err("That agent has ended.".into());
    };
    let history = store.load(id).await.map_err(|e| format!("Could not resume agent {id}: {e}"))?;
    let extensions = Extensions::default();
    if info.fork {
        extensions.insert(ForkedAgent);
    }
    extensions.insert(info.origin.clone());
    wake::install(services, id, &extensions);
    let mut spec = agent_spec(services, &blueprint, id.to_owned(), Some(extensions));
    spec.history = history;
    let parent = registry.parent(id).unwrap_or_else(|| Arc::from(""));
    let about = SubagentStarted {
        tool_use_id: info.tool_use_id.clone(),
        agent_type: info.agent_type.clone(),
        model: blueprint.config.model.clone(),
        effort: blueprint.config.effort.clone(),
        description: info.description.clone(),
        name: info.name.clone(),
        background: true,
    };
    let forward = forward(&blueprint, id, &parent, about);
    let (handle, events) = services.engine.spawn(spec);
    let listeners = Listeners { observer: None, spend: spend(services, &parent) };
    Ok(registry.reattach(id, handle, Tracker::follow(events, forward, Arc::from(id), listeners)))
}

fn forward(blueprint: &Blueprint, id: &str, parent: &str, about: SubagentStarted) -> Option<Forward> {
    let to = blueprint.subagent_events.clone()?;
    Some(Forward { to, agent_id: id.into(), parent_id: parent.into(), about: Arc::new(about) })
}

/// The subagent `to` names among `agents`: by id, else the latest with that name.
fn addressed(agents: &[SubagentInfo], to: &str) -> Option<SubagentInfo> {
    agents
        .iter()
        .find(|a| a.agent_id == to)
        .or_else(|| agents.iter().rev().find(|a| a.name.as_deref() == Some(to)))
        .cloned()
}

/// Runs a turn without the caller waiting, then tells the caller how it went
/// (a `<task-notification>` that wakes it). Boxed: a run ends by waking the
/// agent again when a note came too late for it, which starts another.
fn run_in_background(
    services: Arc<Services>,
    agent: AgentHandle,
    tracker: Arc<Tracker>,
    input: Vec<ContentBlock>,
    worktree: Option<Worktree>,
) -> BoxFuture<'static, ()> {
    Box::pin(run_and_report(services, agent, tracker, input, worktree))
}

async fn run_and_report(
    services: Arc<Services>,
    agent: AgentHandle,
    tracker: Arc<Tracker>,
    input: Vec<ContentBlock>,
    worktree: Option<Worktree>,
) {
    let registry = Arc::clone(&services.registry);
    let id = agent.id().to_owned();
    // Stopped after it was claimed for this run and before the run began.
    let end = if registry.stop_of(&id).is_some() {
        tracker.announce_end("killed").await;
        RunEnd::Stopped(empty_report(&id))
    } else {
        tracker.run(&agent, input).await
    };
    let stop = registry.stop_of(&id);
    let end = as_stopped(end, stop, &id);
    if stop.is_some() {
        stop::end_work(&services, &id, &agent).await;
    }
    // Its commands and monitors that still run may wake it again.
    let waiting = has_running_tasks(&agent);
    drop(agent);
    if let Some(wt) = worktree {
        release_worktree(wt).await;
    }
    let Some(info) = registry.child(&id) else { return };
    let text = match &end {
        RunEnd::Finished(report) => {
            let mut note = AgentNotification::completed(report, &info.description, called_by(&info));
            note.waiting = waiting;
            note.render()
        }
        RunEnd::Stopped(report) => notification(report, &info, Outcome::Stopped(stopped_by(stop))),
        RunEnd::Failed(error) => AgentNotification {
            agent_id: &id,
            tool_use_id: called_by(&info),
            output_file: None,
            description: &info.description,
            outcome: Outcome::Failed { error },
            result: None,
            usage: None,
            waiting: false,
        }
        .render(),
    };
    registry.set_ended(&id, ended(&end));
    // Queued before the agent counts as done, so the caller is never seen idle in between.
    // One stopped with an agent above it is not reported.
    if !stop.is_some_and(|s| s.quiet) {
        wake::report_to_parent(&services, &id, text).await;
    }
    let generation = registry.set_running(&id, false);
    services.release_later(&id, generation);
    // A note of its own work that came after its last request.
    wake::wake(&services, &id);
}

/// The tool call that started a subagent, when one did (a background fork
/// of a typed command has none).
fn called_by(info: &ChildInfo) -> Option<&str> {
    Some(info.tool_use_id.as_str()).filter(|id| !id.is_empty())
}

/// A run of a subagent that was stopped meanwhile ends as stopped, however
/// its turn came to an end.
fn as_stopped(end: RunEnd, stop: Option<Stop>, id: &str) -> RunEnd {
    match (end, stop) {
        (RunEnd::Finished(report), Some(_)) => RunEnd::Stopped(report),
        (RunEnd::Failed(_), Some(_)) => RunEnd::Stopped(empty_report(id)),
        (end, _) => end,
    }
}

fn empty_report(id: &str) -> SubagentReport {
    SubagentReport {
        agent_id: id.to_owned(),
        text: String::new(),
        usage: eos_agent_types::Usage::default(),
        tool_uses: 0,
        duration_ms: 0,
        structured: None,
        worktree: None,
    }
}

/// Who the parent's notification says stopped it.
fn stopped_by(stop: Option<Stop>) -> StoppedBy<'static> {
    match stop.map(|s| s.origin) {
        Some(StopOrigin::Agent) => StoppedBy::Parent,
        Some(StopOrigin::Host) => StoppedBy::User,
        None => StoppedBy::Unknown,
    }
}

/// Claude Code's task status for how a run ended.
fn ended(end: &RunEnd) -> &'static str {
    match end {
        RunEnd::Finished(_) => "completed",
        RunEnd::Stopped(_) => "killed",
        RunEnd::Failed(_) => "failed",
    }
}

/// Counts a subagent's output tokens toward its session's turn budget.
fn spend(services: &Services, parent: &str) -> Option<Arc<dyn Fn(u64) + Send + Sync>> {
    let root = services.registry.root(parent)?;
    let budgets = Arc::clone(&services.budgets);
    Some(Arc::new(move |tokens| budgets.spend(&root, tokens)))
}

async fn make_worktree(cwd: std::path::PathBuf, slug: String) -> Result<Worktree, String> {
    tokio::task::spawn_blocking(move || crate::worktree::create(&cwd, &slug))
        .await
        .map_err(|e| format!("Cannot create a worktree: {e}"))?
}

/// The worktree when kept (it changed files), else `None` once removed.
async fn release_worktree(worktree: Worktree) -> Option<Worktree> {
    let copy = worktree.clone();
    let kept = tokio::task::spawn_blocking(move || crate::worktree::release(&copy)).await.unwrap_or(true);
    kept.then_some(worktree)
}

fn notification(report: &SubagentReport, info: &ChildInfo, outcome: Outcome<'_>) -> String {
    let mut note = AgentNotification::completed(report, &info.description, called_by(info));
    note.outcome = outcome;
    note.render()
}

impl Subagents for EngineSubagents {
    fn start(&self, request: SubagentRequest) -> BoxFuture<'_, Result<SubagentStart, String>> {
        Box::pin(self.start_agent(request))
    }

    fn send<'a>(&'a self, from: &'a str, to: &'a str, message: String) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.deliver(from, to, message))
    }

    fn list<'a>(&'a self, caller: &'a str) -> BoxFuture<'a, Vec<SubagentInfo>> {
        let agents = self.services.upgrade().map(|s| s.registry.session_agents(caller)).unwrap_or_default();
        Box::pin(std::future::ready(agents))
    }

    fn notify<'a>(&'a self, agent: &'a str, text: String, wake: bool) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let services = self.services.upgrade().ok_or("The agent service has shut down.")?;
            let handle = services.registry.handle(agent).ok_or("That agent has ended.")?;
            handle.notify(text, wake).await.map_err(|e| e.to_string())
        })
    }

    fn stop_task<'a>(
        &'a self,
        caller: &'a str,
        handle: &'a str,
        origin: StopOrigin,
    ) -> BoxFuture<'a, Option<Result<TaskStopped, StopRefused>>> {
        Box::pin(async move {
            let services = self.services.upgrade()?;
            Some(stop::stop_task(&services, caller, handle, origin).await)
        })
    }

    fn task_event(&self, event: TaskEvent) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(services) = self.services.upgrade() else { return };
            {
                let mut running = services.running_tasks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                match &event.kind {
                    eos_agent_core::TaskEventKind::Started { .. } => {
                        *running.entry(event.agent_id.clone()).or_default() += 1
                    }
                    eos_agent_core::TaskEventKind::Ended { .. } => {
                        if let Some(n) = running.get_mut(&event.agent_id) {
                            *n = n.saturating_sub(1);
                            if *n == 0 {
                                running.remove(&event.agent_id);
                            }
                        }
                    }
                    eos_agent_core::TaskEventKind::Progress { .. }
                    | eos_agent_core::TaskEventKind::Output { .. }
                    | eos_agent_core::TaskEventKind::Event { .. } => {}
                }
            }
            let to = services.registry.blueprint(&event.agent_id).and_then(|b| b.task_events.clone());
            if let Some(to) = to {
                let _ = to.send(event).await;
            }
        })
    }

    fn background_fork(&self, caller: &str, skill: &str, wanted: Option<bool>) -> Option<String> {
        let services = self.services.upgrade()?;
        let registry = &services.registry;
        let limits = services.spawn_limits;
        let background = registry.blueprint(caller)?.interactive
            && wanted != Some(false)
            && limits.background
            && registry.depth(caller) < limits.max_depth
            && !registry.forked_skill_running(caller, skill);
        if !background {
            return None;
        }
        // A name frees once its agent left the task list.
        let agents = registry.session_agents(caller);
        Some(agent_name(skill, |n| agents.iter().any(|a| !a.delisted && a.name.as_deref() == Some(n))))
    }

    fn depth(&self, agent: &str) -> usize {
        self.services.upgrade().map_or(0, |s| s.registry.depth(agent))
    }

    fn model_of(&self, agent: &str) -> Option<String> {
        self.services.upgrade()?.registry.blueprint(agent).map(|b| b.config.model.clone())
    }

    fn session_of(&self, agent: &str) -> Option<String> {
        self.services.upgrade()?.registry.root(agent).map(|r| r.to_string())
    }

    fn agent_types(&self, caller: &str) -> Vec<String> {
        let Some(services) = self.services.upgrade() else { return Vec::new() };
        services
            .registry
            .blueprint(caller)
            .map(|b| b.project.definitions.names().into_iter().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn tool_names(&self, caller: &str) -> Vec<String> {
        let Some(services) = self.services.upgrade() else { return Vec::new() };
        services
            .registry
            .blueprint(caller)
            .map(|b| plan::live_toolbox(&b).tools().iter().map(|t| t.spec().name().to_owned()).collect())
            .unwrap_or_default()
    }

    fn create_worktree<'a>(&'a self, caller: &'a str, slug: &'a str) -> BoxFuture<'a, Result<Worktree, String>> {
        Box::pin(async move {
            let services = self.services.upgrade().ok_or("The agent service has shut down.")?;
            let blueprint = services.registry.blueprint(caller).ok_or("The calling agent is not running.")?;
            make_worktree(blueprint.project.cwd.clone(), slug.to_owned()).await
        })
    }

    fn release_worktree(&self, worktree: Worktree) -> BoxFuture<'_, bool> {
        Box::pin(async move { release_worktree(worktree).await.is_some() })
    }
}
