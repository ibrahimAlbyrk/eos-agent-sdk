//! Every live agent of the SDK as a tree: a top-level agent and the
//! subagents it started, with what a subagent needs from its parent.
//!
//! Top-level agents are held weakly (their [`SdkAgent`](crate::SdkAgent)
//! owns them); subagents strongly while they work. A finished subagent stays
//! listed and addressable; with a transcript store it is let go after the
//! idle limit and started again from its transcript when messaged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use eos_agent_core::{
    AgentConfig, AgentOrigin, BackgroundTasks, Reminders, StopOrigin, SubagentInfo, SystemPrompt, Toolbox, Wake,
};
use eos_agent_engine::{AgentHandle, WeakAgentHandle};
use eos_agent_tools_shell::ShellTasks;
use eos_agent_tools_web::SessionSearchBudget;
use tokio::sync::mpsc;

use crate::child::{SubagentEvent, Tracker};
use crate::project::Project;

/// What an agent's subagents start from.
#[derive(Debug, Clone)]
pub(crate) struct Blueprint {
    pub(crate) project: Arc<Project>,
    pub(crate) toolbox: Arc<Toolbox>,
    pub(crate) system: SystemPrompt,
    pub(crate) config: AgentConfig,
    pub(crate) roots: Vec<PathBuf>,
    /// The task list and search budget of the whole session.
    pub(crate) task_list: Arc<str>,
    pub(crate) search_budget: SessionSearchBudget,
    /// The top-level agent's MCP servers (not kept alive by its subagents).
    pub(crate) mcp: Option<std::sync::Weak<eos_agent_mcp::McpToolset>>,
    pub(crate) context_window: u64,
    /// The effort the host chose for the top-level agent (`None`: each
    /// model's default); subagents fall back to it.
    pub(crate) session_effort: Option<String>,
    pub(crate) thinking_summaries: bool,
    pub(crate) include_claude_md: bool,
    /// Where the tree's subagent events go, if the host asked for them.
    pub(crate) subagent_events: Option<mpsc::Sender<SubagentEvent>>,
    /// Where the tree's background task events go.
    pub(crate) task_events: Option<mpsc::Sender<eos_agent_core::TaskEvent>>,
    /// The tree's sandbox: the top-level agent's directory's, which its
    /// subagents keep wherever they work (Claude Code reads the settings of
    /// the session's directory once).
    pub(crate) sandbox: Option<Arc<eos_agent_tools_shell::SandboxPolicy>>,
    /// The session's files every agent of the tree may read or write.
    pub(crate) internal: eos_agent_core::InternalPaths,
    /// The tree runs as Claude Code's interactive terminal.
    pub(crate) interactive: bool,
    /// The top-level agent's MCP servers as its ToolSearch (and its forks')
    /// waits for them; subagents keep the tools they started with.
    pub(crate) connecting: Option<eos_agent_tools_session::McpConnecting>,
    /// A top-level agent's toolbox as its MCP servers' tools change.
    pub(crate) mcp_tools: Option<Arc<crate::spawn::McpToolUpdates>>,
    /// What the host asked for the whole tree.
    pub(crate) asked: Arc<crate::spawn::Asked>,
    /// The host's values every agent of the tree starts with.
    pub(crate) seed: Option<Arc<eos_agent_core::Extensions>>,
}

#[derive(Debug)]
enum Link {
    Owned(AgentHandle),
    Borrowed(WeakAgentHandle),
    /// A finished subagent whose task ended; its transcript holds it.
    Released,
}

#[derive(Debug)]
struct Node {
    parent: Option<Arc<str>>,
    link: Link,
    blueprint: Arc<Blueprint>,
    children: Vec<Arc<str>>,
    /// Subagents only: how humans and SendMessage know it.
    info: Option<ChildInfo>,
}

#[derive(Debug, Clone)]
pub(crate) struct ChildInfo {
    pub(crate) name: Option<String>,
    pub(crate) description: String,
    pub(crate) agent_type: String,
    /// The Agent call that started it, named in its notifications.
    pub(crate) tool_use_id: String,
    /// Who started it, as its permission requests say.
    pub(crate) origin: AgentOrigin,
    pub(crate) running: bool,
    /// How its last run ended: `completed`, `failed` or `killed`.
    pub(crate) ended: &'static str,
    /// When its last run ended (Unix ms; 0 while it has not).
    pub(crate) ended_ms: u64,
    /// Its last run went on in the background (the caller did not wait).
    pub(crate) background: bool,
    /// When it was started (Unix ms).
    pub(crate) started_ms: u64,
    /// Run by a workflow's `agent()`.
    pub(crate) workflow: bool,
    /// The `context: fork` skill a typed `/name` launched it for in the
    /// background (Claude Code's `forkedSkillName`).
    pub(crate) forked_skill: Option<String>,
    /// Started as a fork (it may not fork again).
    pub(crate) fork: bool,
    /// Bumped whenever it starts working, so a pending release can tell it
    /// went back to work meanwhile.
    pub(crate) generation: u64,
    pub(crate) tracker: Arc<Tracker>,
    /// Stopped (TaskStop or the host) and not resumed since: its run ends
    /// as `killed` and no wake starts it again.
    pub(crate) stop: Option<Stop>,
    /// The host stopped it for its user: another agent's message never
    /// resumes it.
    pub(crate) stopped_by_user: bool,
}

/// How a subagent was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stop {
    pub(crate) origin: StopOrigin,
    /// Stopped with an agent above it: its parent is not told.
    pub(crate) quiet: bool,
}

/// What [`Registry::claim_stop`] found.
#[derive(Debug)]
pub(crate) enum Claim {
    /// It works: its turn is to be interrupted.
    Running(Option<AgentHandle>),
    /// It had finished, its own background work (or a subagent it started)
    /// still running; it now counts as stopped.
    Waiting(Option<AgentHandle>),
    /// Nothing to stop: how its last run ended.
    Ended(&'static str),
}

/// One live agent as a host sees it.
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    /// The agent that started it (subagents only).
    pub parent_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub agent_type: Option<String>,
    pub model: String,
    /// A turn is running or queued.
    pub busy: bool,
    /// Its history (or, for a finished subagent, the whole agent) is only on disk.
    pub evicted: bool,
}

#[derive(Debug)]
pub(crate) struct Registry {
    nodes: Mutex<HashMap<Arc<str>, Node>>,
    /// How long a finished subagent stays on the task list.
    retention_ms: AtomicU64,
}

impl Default for Registry {
    fn default() -> Self {
        Self { nodes: Mutex::default(), retention_ms: AtomicU64::new(LISTED_AFTER_END_MS) }
    }
}

impl Registry {
    pub(crate) fn set_retention(&self, after: std::time::Duration) {
        self.retention_ms.store(u64::try_from(after.as_millis()).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn nodes(&self) -> MutexGuard<'_, HashMap<Arc<str>, Node>> {
        self.nodes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn insert_root(&self, handle: &AgentHandle, blueprint: Arc<Blueprint>) {
        let node = Node {
            parent: None,
            link: Link::Borrowed(handle.downgrade()),
            blueprint,
            children: Vec::new(),
            info: None,
        };
        self.nodes().insert(handle.id().into(), node);
    }

    /// False when the parent is gone (its tree was removed meanwhile).
    pub(crate) fn insert_child(
        &self,
        parent: &str,
        handle: AgentHandle,
        blueprint: Arc<Blueprint>,
        info: ChildInfo,
    ) -> bool {
        let mut nodes = self.nodes();
        let id: Arc<str> = handle.id().into();
        let Some(parent_node) = nodes.get_mut(parent) else { return false };
        parent_node.children.push(Arc::clone(&id));
        let parent = Some(Arc::from(parent));
        nodes.insert(id, Node { parent, link: Link::Owned(handle), blueprint, children: Vec::new(), info: Some(info) });
        true
    }

    pub(crate) fn blueprint(&self, id: &str) -> Option<Arc<Blueprint>> {
        self.nodes().get(id).map(|n| Arc::clone(&n.blueprint))
    }

    /// What `id` and its later subagents go by from now on (a host changed
    /// its model or effort).
    pub(crate) fn set_blueprint(&self, id: &str, blueprint: Arc<Blueprint>) {
        if let Some(node) = self.nodes().get_mut(id) {
            node.blueprint = blueprint;
        }
    }

    pub(crate) fn handle(&self, id: &str) -> Option<AgentHandle> {
        match &self.nodes().get(id)?.link {
            Link::Owned(handle) => Some(handle.clone()),
            Link::Borrowed(weak) => weak.upgrade(),
            Link::Released => None,
        }
    }

    pub(crate) fn is_released(&self, id: &str) -> bool {
        self.nodes().get(id).is_some_and(|n| matches!(n.link, Link::Released))
    }

    /// Lets a finished subagent's task end, if it is still the idle run
    /// `generation` left behind. False when it is busy and should be retried.
    pub(crate) fn release(&self, id: &str, generation: u64) -> bool {
        let handle = {
            let mut nodes = self.nodes();
            let Some(node) = nodes.get_mut(id) else { return true };
            let Some(info) = &node.info else { return true };
            if info.running || info.generation != generation {
                return true;
            }
            match &node.link {
                // Its own commands and monitors would die with it.
                Link::Owned(handle) if !handle.is_idle() || has_running_tasks(handle) => return false,
                Link::Owned(_) => std::mem::replace(&mut node.link, Link::Released),
                _ => return true,
            }
        };
        drop(handle);
        true
    }

    /// Marks an idle subagent in memory as working on a run of its own,
    /// going on in the background; its handle and tracker. `None` when it
    /// already works (or is not in memory). A `wake` claims it only for the
    /// unread notes its [`Wake`] asked for, and never once it was stopped.
    pub(crate) fn start_run(&self, id: &str, wake: bool) -> Option<(AgentHandle, Arc<Tracker>)> {
        let mut nodes = self.nodes();
        let node = nodes.get_mut(id)?;
        let Link::Owned(handle) = &node.link else { return None };
        let info = node.info.as_mut()?;
        if info.running || (wake && (info.ended == "killed" || info.stop.is_some() || !has_unread_notes(handle))) {
            return None;
        }
        if !wake {
            // A message resumes it.
            info.stop = None;
        }
        info.running = true;
        info.generation += 1;
        info.ended_ms = 0;
        info.background = true;
        Some((handle.clone(), Arc::clone(&info.tracker)))
    }

    /// Puts a restarted subagent back in place of its released entry; hands
    /// back the live one if another caller restarted it first.
    pub(crate) fn reattach(&self, id: &str, handle: AgentHandle, tracker: Arc<Tracker>) -> AgentHandle {
        let mut nodes = self.nodes();
        let Some(node) = nodes.get_mut(id) else { return handle };
        match &node.link {
            Link::Owned(live) => live.clone(),
            _ => {
                node.link = Link::Owned(handle.clone());
                if let Some(info) = node.info.as_mut() {
                    info.tracker = tracker;
                }
                handle
            }
        }
    }

    /// Marks the subagent `id` stopped if it works, or if it finished while
    /// its own background work (or a subagent it started) still runs.
    /// Decided under the lock, so a wake or a message claiming it at the
    /// same time either sees it stopped or is stopped with it.
    pub(crate) fn claim_stop(&self, id: &str, stop: Stop) -> Claim {
        let mut nodes = self.nodes();
        let waiting = waiting_on_work(&nodes, id);
        let Some(node) = nodes.get_mut(id) else { return Claim::Ended("killed") };
        let handle = live(&node.link);
        let Some(info) = node.info.as_mut() else { return Claim::Ended("completed") };
        if !info.running && !waiting {
            return Claim::Ended(info.ended);
        }
        info.stop = Some(stop);
        info.stopped_by_user |= stop.origin == StopOrigin::Host;
        if info.running {
            return Claim::Running(handle);
        }
        info.ended = "killed";
        info.ended_ms = now_ms();
        Claim::Waiting(handle)
    }

    /// Whether [`claim_stop`](Self::claim_stop) would stop `id`; else how
    /// its last run ended.
    pub(crate) fn stoppable(&self, id: &str) -> Result<(), &'static str> {
        let nodes = self.nodes();
        let Some(info) = nodes.get(id).and_then(|n| n.info.as_ref()) else { return Err("killed") };
        if info.running || waiting_on_work(&nodes, id) { Ok(()) } else { Err(info.ended) }
    }

    /// How `id` was stopped, if it was and has not been resumed since.
    pub(crate) fn stop_of(&self, id: &str) -> Option<Stop> {
        self.nodes().get(id)?.info.as_ref()?.stop
    }

    /// The subagents `id` started.
    pub(crate) fn children(&self, id: &str) -> Vec<Arc<str>> {
        self.nodes().get(id).map(|n| n.children.clone()).unwrap_or_default()
    }

    pub(crate) fn child(&self, id: &str) -> Option<ChildInfo> {
        self.nodes().get(id)?.info.clone()
    }

    /// Who started `id` (`None` for a top-level agent or one that is gone).
    pub(crate) fn origin(&self, id: &str) -> Option<AgentOrigin> {
        self.nodes().get(id)?.info.as_ref().map(|i| i.origin.clone())
    }

    /// Marks a subagent working or done; returns its generation.
    pub(crate) fn set_running(&self, id: &str, running: bool) -> u64 {
        let mut nodes = self.nodes();
        let Some(info) = nodes.get_mut(id).and_then(|n| n.info.as_mut()) else { return 0 };
        if running && !info.running {
            info.generation += 1;
        }
        info.ended_ms = if running { 0 } else { now_ms() };
        info.running = running;
        info.generation
    }

    /// Records how a subagent's run ended (`completed`, `failed`, `killed`).
    pub(crate) fn set_ended(&self, id: &str, how: &'static str) {
        if let Some(info) = self.nodes().get_mut(id).and_then(|n| n.info.as_mut()) {
            info.ended = how;
        }
    }

    /// A live agent that no other agent started.
    pub(crate) fn is_top_level(&self, id: &str) -> bool {
        self.nodes().get(id).is_some_and(|n| n.parent.is_none())
    }

    /// The parent of `id`, if `id` is a subagent.
    pub(crate) fn parent(&self, id: &str) -> Option<Arc<str>> {
        self.nodes().get(id)?.parent.clone()
    }

    /// How many agents started `id`, one inside the other (0 for a top-level agent).
    pub(crate) fn depth(&self, id: &str) -> usize {
        let nodes = self.nodes();
        let mut depth = 0;
        let mut node = nodes.get(id);
        while let Some(parent) = node.and_then(|n| n.parent.as_deref()) {
            depth += 1;
            node = nodes.get(parent);
        }
        depth
    }

    /// The top-level agent of `id`'s tree.
    pub(crate) fn root(&self, id: &str) -> Option<Arc<str>> {
        let nodes = self.nodes();
        let (mut key, mut node) = nodes.get_key_value(id)?;
        while let Some(parent) = &node.parent {
            (key, node) = nodes.get_key_value(parent.as_ref())?;
        }
        Some(Arc::clone(key))
    }

    /// The subagents `caller` started, oldest first.
    /// Every subagent of `caller`'s session (its top-level agent's tree, at
    /// any depth, the caller included), in the order they started: Claude
    /// Code's task list, which ListAgents shows and SendMessage reaches.
    pub(crate) fn session_agents(&self, caller: &str) -> Vec<SubagentInfo> {
        let Some(root) = self.root(caller) else { return Vec::new() };
        let now = now_ms();
        let nodes = self.nodes();
        let mut ids: Vec<&Arc<str>> = Vec::new();
        let mut stack: Vec<&Arc<str>> = nodes.get(&root).map(|n| n.children.iter().rev().collect()).unwrap_or_default();
        while let Some(id) = stack.pop() {
            ids.push(id);
            if let Some(node) = nodes.get(id) {
                stack.extend(node.children.iter().rev());
            }
        }
        let mut agents: Vec<SubagentInfo> = ids
            .into_iter()
            .filter_map(|id| {
                let info = nodes.get(id)?.info.as_ref()?;
                Some(SubagentInfo {
                    agent_id: id.to_string(),
                    name: info.name.clone(),
                    description: info.description.clone(),
                    agent_type: info.agent_type.clone(),
                    running: info.running,
                    status: if info.running { "running" } else { info.ended },
                    started_ms: info.started_ms,
                    workflow: info.workflow,
                    delisted: delisted(&nodes, id, info, now, self.retention_ms.load(Ordering::Relaxed)),
                })
            })
            .collect();
        agents.sort_by_key(|a| a.started_ms);
        latest_hold_names(&mut agents);
        agents
    }

    /// Whether a subagent of `caller`'s session launched for the fork skill
    /// `skill` is still working (Claude Code then runs another typed one in
    /// the foreground).
    pub(crate) fn forked_skill_running(&self, caller: &str, skill: &str) -> bool {
        let Some(root) = self.root(caller) else { return false };
        let nodes = self.nodes();
        let mut stack: Vec<&Arc<str>> = nodes.get(&root).map(|n| n.children.iter().collect()).unwrap_or_default();
        while let Some(id) = stack.pop() {
            let Some(node) = nodes.get(id) else { continue };
            if node.info.as_ref().is_some_and(|i| i.running && i.forked_skill.as_deref() == Some(skill)) {
                return true;
            }
            stack.extend(node.children.iter());
        }
        false
    }

    /// Subagents below `id` that are still working.
    pub(crate) fn running_below(&self, id: &str) -> usize {
        let nodes = self.nodes();
        let mut stack: Vec<&Arc<str>> = nodes.get(id).map(|n| n.children.iter().collect()).unwrap_or_default();
        let mut running = 0;
        while let Some(child) = stack.pop() {
            if let Some(node) = nodes.get(child) {
                running += usize::from(node.info.as_ref().is_some_and(|i| i.running));
                stack.extend(node.children.iter());
            }
        }
        running
    }

    pub(crate) fn len(&self) -> usize {
        self.nodes().len()
    }

    /// The ids of `root` and every agent below it.
    pub(crate) fn tree_ids(&self, root: &str) -> Vec<Arc<str>> {
        let nodes = self.nodes();
        let mut out = Vec::new();
        let mut stack: Vec<&Arc<str>> = nodes.get_key_value(root).map(|(k, _)| vec![k]).unwrap_or_default();
        while let Some(id) = stack.pop() {
            out.push(Arc::clone(id));
            stack.extend(nodes.get(id).map(|n| n.children.iter()).into_iter().flatten());
        }
        out
    }

    /// Every agent of `root`'s tree (all trees when `None`).
    pub(crate) fn infos(&self, root: Option<&str>) -> Vec<AgentInfo> {
        let nodes = self.nodes();
        let ids: Vec<&Arc<str>> = match root {
            None => nodes.keys().collect(),
            Some(root) => {
                let mut out = Vec::new();
                let mut stack: Vec<&Arc<str>> = nodes.get_key_value(root).map(|(k, _)| vec![k]).unwrap_or_default();
                while let Some(id) = stack.pop() {
                    out.push(id);
                    stack.extend(nodes.get(id).map(|n| n.children.iter()).into_iter().flatten());
                }
                out
            }
        };
        ids.into_iter().filter_map(|id| Some(info_of(id, nodes.get(id)?))).collect()
    }

    /// Forgets `id` and everything it started; their handles drop outside the
    /// lock (an agent's task ends once the last handle is gone).
    pub(crate) fn remove_tree(&self, id: &str) {
        let mut removed = Vec::new();
        {
            let mut nodes = self.nodes();
            let mut stack = vec![Arc::<str>::from(id)];
            while let Some(next) = stack.pop() {
                if let Some(node) = nodes.remove(&next) {
                    stack.extend(node.children.iter().cloned());
                    removed.push(node);
                }
            }
            if let Some(parent) = removed.first().and_then(|n| n.parent.clone())
                && let Some(parent) = nodes.get_mut(&parent)
            {
                parent.children.retain(|c| c.as_ref() != id);
            }
        }
        drop(removed);
    }
}

/// A command or monitor the agent started still runs.
pub(crate) fn has_running_tasks(handle: &AgentHandle) -> bool {
    handle.extensions().get::<ShellTasks>().is_some_and(|tasks| tasks.any_running())
}

/// A subagent that finished well while a command, monitor or workflow run
/// of its own, or a subagent it started, still runs.
fn waiting_on_work(nodes: &HashMap<Arc<str>, Node>, id: &str) -> bool {
    let Some(node) = nodes.get(id) else { return false };
    let Some(info) = &node.info else { return false };
    if info.running || info.ended != "completed" {
        return false;
    }
    let child_runs =
        node.children.iter().any(|c| nodes.get(c).and_then(|n| n.info.as_ref()).is_some_and(|i| i.running));
    let own_work = live(&node.link).is_some_and(|h| {
        has_running_tasks(&h) || h.extensions().get::<BackgroundTasks>().is_some_and(|t| !t.all_running().is_empty())
    });
    child_runs || own_work
}

fn live(link: &Link) -> Option<AgentHandle> {
    match link {
        Link::Owned(handle) => Some(handle.clone()),
        Link::Borrowed(weak) => weak.upgrade(),
        Link::Released => None,
    }
}

fn has_unread_notes(handle: &AgentHandle) -> bool {
    let wanted = handle.extensions().get::<Wake>().is_some_and(|w| w.is_wanted());
    wanted && handle.extensions().get::<Reminders>().is_some_and(|r| !r.is_empty())
}

fn info_of(id: &str, node: &Node) -> AgentInfo {
    let handle = match &node.link {
        Link::Owned(handle) => Some(handle.clone()),
        Link::Borrowed(weak) => weak.upgrade(),
        Link::Released => None,
    };
    let info = node.info.as_ref();
    AgentInfo {
        agent_id: id.to_owned(),
        parent_id: node.parent.as_deref().map(str::to_owned),
        name: info.and_then(|i| i.name.clone()),
        description: info.map(|i| i.description.clone()),
        agent_type: info.map(|i| i.agent_type.clone()),
        model: node.blueprint.config.model.clone(),
        busy: handle.as_ref().is_some_and(|h| !h.is_idle()),
        evicted: handle.as_ref().is_none_or(AgentHandle::is_evicted),
    }
}

/// How long Claude Code keeps a finished subagent on its task list (`CI`).
pub(crate) const LISTED_AFTER_END_MS: u64 = 30_000;

/// Whether a finished subagent has left Claude Code's task list. Its end was
/// reported at once here (a background one's notification is queued before
/// it counts as finished). Each subagent it started keeps it on the list
/// until that one's end is reported too (Claude Code's `agent:<id>`
/// keepalive, held by the starting agent only). Then it stays 30 s: from its
/// own end when it ran in the foreground or nothing kept it, from the last
/// keepalive's end when it ran in the background. A failed or stopped one
/// drops its keepalives and leaves 30 s after it ended.
/// Claude Code keeps one agent per name (`agentNameRegistry`: registering a
/// name again moves it), so of the agents given the same name only the one
/// started last keeps it; the others are known by their ids alone.
fn latest_hold_names(agents: &mut [SubagentInfo]) {
    let mut holder: HashMap<&str, usize> = HashMap::new();
    for (i, agent) in agents.iter().enumerate() {
        if let Some(name) = agent.name.as_deref() {
            holder.insert(name, i);
        }
    }
    let superseded: Vec<usize> = agents
        .iter()
        .enumerate()
        .filter(|(i, a)| a.name.as_deref().is_some_and(|n| holder.get(n) != Some(i)))
        .map(|(i, _)| i)
        .collect();
    for i in superseded {
        agents[i].name = None;
    }
}

fn delisted(nodes: &HashMap<Arc<str>, Node>, id: &str, info: &ChildInfo, now: u64, retention_ms: u64) -> bool {
    if info.running {
        return false;
    }
    let children = nodes.get(id).map(|n| n.children.as_slice()).unwrap_or_default();
    let children = children.iter().filter_map(|c| nodes.get(c)?.info.as_ref()).map(|c| (c.running, c.ended_ms));
    listed_from(info.ended_ms, info.background, info.ended == "completed", children)
        .is_some_and(|since| now >= since.saturating_add(retention_ms))
}

/// When a finished subagent's 30 s on the list start (`None`: a subagent it
/// started still keeps it there), from its end, how it ran and ended, and
/// its subagents' `(running, ended_ms)`.
fn listed_from(
    ended_ms: u64,
    background: bool,
    completed: bool,
    children: impl Iterator<Item = (bool, u64)>,
) -> Option<u64> {
    if !completed {
        return Some(ended_ms);
    }
    let mut since = ended_ms;
    for (running, ended) in children {
        if running {
            return None;
        }
        if background {
            since = since.max(ended);
        }
    }
    Some(since)
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::listed_from;

    #[test]
    fn a_finished_subagent_stays_listed_as_claude_code_keeps_its_task() {
        // Nothing kept it: 30 s from its own end.
        assert_eq!(listed_from(100, true, true, std::iter::empty()), Some(100));
        assert_eq!(listed_from(100, true, true, [(false, 40)].into_iter()), Some(100));
        // A subagent it started that still works keeps it listed.
        assert_eq!(listed_from(100, true, true, [(false, 40), (true, 0)].into_iter()), None);
        assert_eq!(listed_from(100, false, true, [(true, 0)].into_iter()), None);
        // Once that one ended: from then in the background, from its own end
        // in the foreground (Claude Code set that when it reported the end).
        assert_eq!(listed_from(100, true, true, [(false, 160), (false, 130)].into_iter()), Some(160));
        assert_eq!(listed_from(100, false, true, [(false, 160)].into_iter()), Some(100));
        // Failed or stopped: its keepalives are dropped.
        assert_eq!(listed_from(100, true, false, [(true, 0)].into_iter()), Some(100));
    }
}
