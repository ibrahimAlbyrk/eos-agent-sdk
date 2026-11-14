//! The port subagent tools use to start and talk to other agents. The host
//! (which owns the engine, toolsets and prompts) implements it.

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_types::Usage;

use crate::BoxFuture;
use crate::message::StoredMessage;
use crate::modes::{DeclaredMode, PermissionMode};
use crate::ports::AgentEvent;
use crate::tasks::TaskEvent;

#[derive(Debug, Clone, Default)]
pub struct SubagentRequest {
    /// The agent asking.
    pub parent_id: String,
    /// The Agent call that asked (names the fork's own call, tags notifications).
    pub tool_use_id: String,
    /// Short label for humans (3–5 words).
    pub description: String,
    pub prompt: String,
    /// Agent definition to run (`general-purpose` when `None`).
    pub agent_type: Option<String>,
    pub model: Option<String>,
    /// `low` … `max`; `None` keeps the definition's or the parent's.
    pub effort: Option<String>,
    /// `worktree` or `remote`, as the call asked; the host decides what it offers.
    pub isolation: Option<String>,
    /// Addressable name for later messages.
    pub name: Option<String>,
    /// Start from a copy of the parent's conversation (shares its prompt cache).
    pub fork: bool,
    /// For a fork: the parent's conversation up to and including the message
    /// holding the Agent call (message clones share their bytes).
    pub history: Vec<StoredMessage>,
    /// Return at once; the parent is notified when the subagent finishes.
    pub background: bool,
    /// The agent type's system prompt replaced or extended (a workflow's
    /// subagents are told their reply is a return value).
    pub system: Option<SubagentSystem>,
    /// The subagent must answer through a `StructuredOutput` tool whose input
    /// is this JSON Schema; the report carries the validated object.
    pub output_schema: Option<serde_json::Value>,
    /// Tools taken from the subagent on top of its type's own denials. A
    /// trailing `*` matches by prefix (`mcp__*`).
    pub deny_tools: Vec<String>,
    /// Told what the subagent does while it runs.
    pub observer: Option<Arc<dyn SubagentObserver>>,
    /// Work there instead of the parent's directory (an isolated worktree).
    pub cwd: Option<PathBuf>,
    /// The workflow run and `agent()` call that started it, for hosts.
    pub workflow: Option<WorkflowTag>,
    /// The Agent call's `mode` (`None`: not given, or not a mode name).
    pub mode: Option<PermissionMode>,
    /// Claude Code's `forkedSkillName`: the `context: fork` skill a typed
    /// `/name` launched it for in the background.
    pub forked_skill: Option<String>,
}

/// How a request changes its agent type's system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentSystem {
    /// Instead of the type's prompt (Claude Code's identity line stays first).
    Replace(String),
    /// After the type's prompt.
    Append(String),
}

/// Who started a subagent. Whoever starts it puts this in its
/// [`Extensions`](crate::Extensions); the permission gate then sees it with
/// every call of the agent ([`ToolCall::origin`](crate::ToolCall::origin)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOrigin {
    /// The top-level agent of the tree.
    pub root_id: Arc<str>,
    /// The agent that started it.
    pub parent_id: Arc<str>,
    /// The parent's tool call that started it (a typed command's fork has none).
    pub tool_use_id: Option<Arc<str>>,
    /// The workflow `agent()` call it runs, or the one an agent above it runs.
    pub workflow: Option<WorkflowTag>,
    /// What each subagent from the top-level agent down to this one
    /// declared about its permission mode, in that order (see
    /// [`resolve_chain`](crate::resolve_chain)).
    pub modes: Arc<[DeclaredMode]>,
}

/// Which workflow `agent()` call a subagent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowTag {
    pub run_id: String,
    /// 1-based, in call order across the run.
    pub agent_index: u32,
}

/// Follows a subagent from the outside while it runs.
pub trait SubagentObserver: Send + Sync + std::fmt::Debug {
    /// The subagent exists and runs on `model`.
    fn started(&self, _agent_id: &str, _model: &str) {}

    /// One of its events, as it happens.
    fn event(&self, _agent_id: &str, _event: &AgentEvent) {}
}

/// A git worktree made for one subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// The commit it was made from (changes are counted against it).
    pub head_commit: String,
    pub git_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub agent_id: String,
    /// The subagent's final reply.
    pub text: String,
    pub usage: Usage,
    pub tool_uses: u32,
    pub duration_ms: u64,
    /// With an output schema: the object its `StructuredOutput` call
    /// returned, as the model wrote it (key order kept).
    pub structured: Option<Box<serde_json::value::RawValue>>,
    /// The worktree it ran in, when that was kept because it changed files.
    pub worktree: Option<Box<Worktree>>,
}

#[derive(Debug, Clone)]
pub enum SubagentStart {
    Finished(SubagentReport),
    Launched { agent_id: String },
}

#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub agent_id: String,
    pub name: Option<String>,
    pub description: String,
    pub agent_type: String,
    pub running: bool,
    /// Claude Code's task status: `running`, else how its last run ended
    /// (`completed`, `failed`, `killed`).
    pub status: &'static str,
    /// When it was started (Unix ms).
    pub started_ms: u64,
    /// Started by a workflow's `agent()`: never listed (Claude Code runs them
    /// outside its task list), though SendMessage still reaches one by id.
    pub workflow: bool,
    /// Finished and dropped from Claude Code's task list (its eviction): no
    /// longer listed, its name free for a new agent, though SendMessage
    /// still resumes it.
    pub delisted: bool,
}

/// Who asks [`Subagents::stop_task`] to stop a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOrigin {
    /// An agent, through its TaskStop tool. The session's top-level agent
    /// may stop any task of the session; a subagent only its own (its
    /// commands and monitors, its workflow runs, itself).
    Agent,
    /// The host, for its user: any task of the agent it names or of an
    /// agent below it. A subagent the user stopped is not resumed by
    /// another agent's message.
    Host,
}

/// A task [`Subagents::stop_task`] stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStopped {
    /// The task's own id (a subagent's agent id, also when it was named).
    pub task_id: String,
    /// `local_bash` (a background command or a monitor), `local_workflow`,
    /// `local_agent` (a subagent).
    pub task_type: String,
    /// Its command, or its description.
    pub description: String,
    /// The agent whose task it was (a subagent is its own).
    pub owner: String,
}

/// Why [`Subagents::stop_task`] stopped nothing, as the model reads it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StopRefused {
    /// No task goes by that handle: the whole message, with what runs.
    #[error("{0}")]
    NotFound(String),
    #[error("Task {id} is not running (status: {status})")]
    NotRunning { id: String, status: &'static str },
    /// A subagent asked to stop a task that is not its own.
    #[error("{0}")]
    NotOwner(String),
}

/// The [`Subagents`] of the agent whose [`Extensions`](crate::Extensions)
/// hold it: how a tool every agent shares (TaskStop) reaches the host's
/// agents.
#[derive(Clone)]
pub struct SubagentPort(pub Arc<dyn Subagents>);

impl std::fmt::Debug for SubagentPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentPort").finish_non_exhaustive()
    }
}

pub trait Subagents: Send + Sync {
    fn start(&self, request: SubagentRequest) -> BoxFuture<'_, Result<SubagentStart, String>>;

    /// Delivers `message` to an agent the caller may address (a finished one resumes).
    fn send<'a>(&'a self, from: &'a str, to: &'a str, message: String) -> BoxFuture<'a, Result<(), String>>;

    /// Agents the caller may address.
    fn list<'a>(&'a self, caller: &'a str) -> BoxFuture<'a, Vec<SubagentInfo>>;

    /// Queues `text` for `agent` as a note it reads with its next request;
    /// `wake` starts a turn for it when it is idle (a background task ended).
    fn notify<'a>(&'a self, agent: &'a str, text: String, wake: bool) -> BoxFuture<'a, Result<(), String>> {
        let _ = (agent, text, wake);
        Box::pin(async { Err("This host cannot deliver notifications.".to_owned()) })
    }

    /// Stops a background task named by `handle` and waits until it has
    /// ended: a command or monitor, a workflow run (by task id), or a
    /// subagent (by its agent id, or the name it was started with). A
    /// stopped subagent's running turn is interrupted, its own commands,
    /// monitors and workflow runs end with it, it is not woken again and
    /// its parent is told it was stopped. One that finished while its own
    /// background work runs on is stopped with that work and with every
    /// subagent below it. `caller` is the agent asking
    /// ([`StopOrigin::Agent`]) or the agent whose tree the host searches
    /// ([`StopOrigin::Host`]). `None`: this host does not stop tasks
    /// (TaskStop then reaches the caller's own commands and workflow runs).
    fn stop_task<'a>(
        &'a self,
        caller: &'a str,
        handle: &'a str,
        origin: StopOrigin,
    ) -> BoxFuture<'a, Option<Result<TaskStopped, StopRefused>>> {
        let _ = (caller, handle, origin);
        Box::pin(async { None })
    }

    /// Tells the host about a background task of one of its agents.
    fn task_event(&self, event: TaskEvent) -> BoxFuture<'_, ()> {
        let _ = event;
        Box::pin(async {})
    }

    /// The top-level agent of `agent`'s tree (the session SendMessage's
    /// `main` names).
    fn session_of(&self, agent: &str) -> Option<String> {
        let _ = agent;
        None
    }

    /// Claude Code's interactive terminal launches a `context: fork` skill
    /// (typed or through the Skill tool) in the background: the name that
    /// fork of `skill` started by `caller` goes by, or `None` when it runs in
    /// the foreground (`-p` and the SDK, `wanted == Some(false)` (the skill's
    /// `background: false`), background tasks off, the nesting limit
    /// reached, or a fork of the same skill still working).
    fn background_fork(&self, caller: &str, skill: &str, wanted: Option<bool>) -> Option<String> {
        let _ = (caller, skill, wanted);
        None
    }

    /// How deep `agent` is nested: 0 for a top-level agent, 1 for its
    /// subagents, and so on (Claude Code's spawn depth).
    fn depth(&self, agent: &str) -> usize {
        let _ = agent;
        0
    }

    /// The model `agent` runs on.
    fn model_of(&self, agent: &str) -> Option<String> {
        let _ = agent;
        None
    }

    /// The agent types `caller` may start, by name.
    fn agent_types(&self, caller: &str) -> Vec<String> {
        let _ = caller;
        Vec::new()
    }

    /// The names of the tools `caller` has (MCP ones as
    /// `mcp__<server>__<tool>`): the pool a workflow agent's `disallowedTools`
    /// are checked against.
    fn tool_names(&self, caller: &str) -> Vec<String> {
        let _ = caller;
        Vec::new()
    }

    /// Makes a git worktree named `slug` of the repository `caller` works in.
    fn create_worktree<'a>(&'a self, caller: &'a str, slug: &'a str) -> BoxFuture<'a, Result<Worktree, String>> {
        let _ = (caller, slug);
        Box::pin(async {
            Err("Isolation is not available in this session. Omit isolation to run the agent in the current \
                 working directory."
                .to_owned())
        })
    }

    /// Removes `worktree` when nothing changed in it; true when it was kept.
    fn release_worktree(&self, worktree: Worktree) -> BoxFuture<'_, bool> {
        let _ = worktree;
        Box::pin(async { true })
    }
}
