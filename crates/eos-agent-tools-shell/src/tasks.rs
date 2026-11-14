//! An agent's background tasks (backgrounded commands and monitors).

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{CancellationToken, TaskEvent, TaskEventKind, TaskEvents, TaskUsage, ToolContext};
use tokio::sync::watch;

use crate::process::kill_group;

/// Finished tasks kept so TaskStop can say they are no longer running.
const FINISHED_KEPT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Running,
    Completed,
    Failed,
    Killed,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

/// One background process group and how it ended.
#[derive(Debug)]
pub(crate) struct Task {
    pub id: String,
    pub command: String,
    pub output_file: PathBuf,
    pgid: u32,
    /// Fired by TaskStop; the task's own watcher kills the group, so the kill
    /// never races the reaping of the shell.
    stop: CancellationToken,
    status: watch::Sender<Status>,
    /// Who hears about the task, when the host listens.
    report: Option<Report>,
    started: std::time::Instant,
}

#[derive(Debug)]
struct Report {
    events: TaskEvents,
    agent_id: String,
    tool_use_id: String,
}

impl Task {
    /// A task of the agent making the call `ctx`.
    pub(crate) fn new(id: String, command: String, output_file: PathBuf, pgid: u32, ctx: &ToolContext<'_>) -> Self {
        let report = ctx.extensions.get::<TaskEvents>().map(|events| Report {
            events: (*events).clone(),
            agent_id: ctx.agent_id.to_owned(),
            tool_use_id: ctx.tool_use_id.to_owned(),
        });
        Self {
            id,
            command,
            output_file,
            pgid,
            stop: CancellationToken::new(),
            status: watch::Sender::new(Status::Running),
            report,
            started: std::time::Instant::now(),
        }
    }

    /// Does a host follow this task's events?
    pub(crate) fn is_followed(&self) -> bool {
        self.report.is_some()
    }

    async fn tell(&self, kind: TaskEventKind) {
        if let Some(report) = &self.report {
            let event = TaskEvent {
                agent_id: report.agent_id.clone(),
                task_id: self.id.clone(),
                tool_use_id: Some(report.tool_use_id.clone()),
                kind,
            };
            report.events.send(event).await;
        }
    }

    /// Tells the host the task runs.
    pub(crate) async fn announce(&self, description: &str) {
        self.tell(TaskEventKind::Started {
            task_type: "local_bash".into(),
            description: description.to_owned(),
            workflow_name: None,
            run_id: None,
            transcript_dir: None,
            command: Some(self.command.clone()),
            output_file: Some(self.output_file.display().to_string()),
        })
        .await;
    }

    /// Tells the host the task's output has grown to `total_bytes`.
    pub(crate) async fn wrote(&self, total_bytes: u64) {
        self.tell(TaskEventKind::Output { total_bytes }).await;
    }

    /// Tells the host what a monitor told its agent.
    pub(crate) async fn event(&self, text: &str) {
        self.tell(TaskEventKind::Event { text: text.to_owned() }).await;
    }

    /// The task is over: tells the host, then lets waiters go.
    pub(crate) async fn end(&self, status: Status, exit_code: Option<i32>, summary: &str) {
        self.tell(TaskEventKind::Ended {
            status: match status {
                Status::Killed => "stopped".into(),
                other => other.as_str().into(),
            },
            summary: summary.to_owned(),
            output_file: Some(self.output_file.display().to_string()),
            usage: TaskUsage {
                duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..TaskUsage::default()
            },
            workflow_progress: None,
            exit_code,
        })
        .await;
        self.status.send_replace(status);
    }

    pub(crate) fn status(&self) -> Status {
        *self.status.borrow()
    }

    pub(crate) fn stop_requested(&self) -> &CancellationToken {
        &self.stop
    }

    /// For the watcher only, while it still holds the unreaped shell.
    pub(crate) fn kill(&self) {
        kill_group(self.pgid);
    }

    /// Asks the watcher to kill the group and waits until it has.
    pub(crate) async fn stop(&self) {
        self.stop.cancel();
        let mut rx = self.status.subscribe();
        let _ = rx.wait_for(|s| *s != Status::Running).await;
    }
}

/// The background tasks of one agent, kept in its extensions. Dropping it
/// (the agent is gone) kills every group still running: tasks never outlive
/// their agent.
#[derive(Debug, Default)]
pub struct ShellTasks {
    tasks: Mutex<Vec<Arc<Task>>>,
}

impl ShellTasks {
    pub(crate) fn insert(&self, task: Arc<Task>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        let finished = tasks.iter().filter(|t| t.status() != Status::Running).count();
        if finished >= FINISHED_KEPT
            && let Some(oldest) = tasks.iter().position(|t| t.status() != Status::Running)
        {
            tasks.remove(oldest);
        }
        tasks.push(task);
    }

    pub(crate) fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|t| t.id == id).cloned()
    }

    /// Stops the background command or monitor `id` from outside the
    /// agent's turn, as TaskStop does, and waits until it has ended: its
    /// command, or the status of one that already ended. `None`: no such
    /// task.
    pub async fn stop(&self, id: &str) -> Option<Result<String, &'static str>> {
        let task = self.get(id)?;
        let status = task.status();
        if status != Status::Running {
            return Some(Err(status.as_str()));
        }
        task.stop().await;
        Some(Ok(task.command.clone()))
    }

    /// The status (`running`, `completed`, `failed`, `killed`) and command
    /// of the background command or monitor `id`.
    pub fn describe(&self, id: &str) -> Option<(&'static str, String)> {
        self.get(id).map(|t| (t.status().as_str(), t.command.clone()))
    }

    /// Whether any task still runs.
    pub fn any_running(&self) -> bool {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|t| t.status() == Status::Running)
    }

    /// Ids of the tasks still running.
    pub fn running(&self) -> Vec<String> {
        let tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.iter().filter(|t| t.status() == Status::Running).map(|t| t.id.clone()).collect()
    }
}

impl Drop for ShellTasks {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap_or_else(PoisonError::into_inner).iter() {
            if task.status() == Status::Running {
                // Directly, not through the watcher: the runtime may be gone too.
                kill_group(task.pgid);
                task.stop.cancel();
            }
        }
    }
}

/// `b` + 8 base-36 chars, like Claude Code's shell task ids.
pub(crate) fn new_task_id() -> String {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut id = String::with_capacity(9);
    id.push('b');
    id.extend(std::iter::repeat_with(|| char::from(CHARS[fastrand::usize(..CHARS.len())])).take(8));
    id
}
