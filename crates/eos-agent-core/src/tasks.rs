//! Background tasks an agent's tools started (a workflow run, a backgrounded
//! command, a monitor): a registry in the agent's
//! [`Extensions`](crate::Extensions) that TaskStop reaches, and the events
//! hosts get about them (Claude Code's SDK `task_started`, `task_progress`
//! and `task_notification` messages).

use std::sync::{Arc, Mutex, PoisonError};

use tokio_util::sync::CancellationToken;

use crate::BoxFuture;

/// Finished tasks kept so TaskStop can say they are no longer running.
const FINISHED_KEPT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

/// One background task: what TaskStop shows of it and how it is stopped.
#[derive(Debug)]
pub struct BackgroundTask {
    pub id: String,
    /// Claude Code's task type (`local_workflow`, …).
    pub task_type: String,
    /// What TaskStop reports as the task's command.
    pub description: String,
    stop: CancellationToken,
    done: CancellationToken,
    status: Mutex<TaskStatus>,
}

impl BackgroundTask {
    pub fn new(id: impl Into<String>, task_type: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            task_type: task_type.into(),
            description: description.into(),
            stop: CancellationToken::new(),
            done: CancellationToken::new(),
            status: Mutex::new(TaskStatus::Running),
        }
    }

    pub fn status(&self) -> TaskStatus {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fires when TaskStop (or the owner) asks the task to stop.
    pub fn stop_requested(&self) -> &CancellationToken {
        &self.stop
    }

    /// The task's own end; wakes whoever waits in [`stop`](Self::stop).
    pub fn finish(&self, status: TaskStatus) {
        *self.status.lock().unwrap_or_else(PoisonError::into_inner) = status;
        self.done.cancel();
    }

    /// Asks the task to stop and waits until it has.
    pub fn stop(&self) -> BoxFuture<'_, ()> {
        self.stop.cancel();
        Box::pin(self.done.cancelled())
    }
}

/// An agent's background tasks other than its shell commands. Dropping it
/// (the agent is gone) stops every task still running.
#[derive(Debug, Default)]
pub struct BackgroundTasks {
    tasks: Mutex<Vec<Arc<BackgroundTask>>>,
}

impl BackgroundTasks {
    pub fn insert(&self, task: Arc<BackgroundTask>) {
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        let finished = tasks.iter().filter(|t| t.status() != TaskStatus::Running).count();
        if finished >= FINISHED_KEPT
            && let Some(oldest) = tasks.iter().position(|t| t.status() != TaskStatus::Running)
        {
            tasks.remove(oldest);
        }
        tasks.push(task);
    }

    pub fn get(&self, id: &str) -> Option<Arc<BackgroundTask>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|t| t.id == id).cloned()
    }

    /// The tasks of `task_type` still running.
    pub fn running(&self, task_type: &str) -> Vec<Arc<BackgroundTask>> {
        let tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.iter().filter(|t| t.task_type == task_type && t.status() == TaskStatus::Running).cloned().collect()
    }
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap_or_else(PoisonError::into_inner).iter() {
            task.stop.cancel();
        }
    }
}

/// Usage of a task so far, as Claude Code's SDK reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TaskUsage {
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
}

/// Something a host may want to know about a background task.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    /// The agent whose tool started the task.
    pub agent_id: String,
    pub task_id: String,
    /// The tool call that started it.
    pub tool_use_id: Option<String>,
    pub kind: TaskEventKind,
}

#[derive(Debug, Clone)]
pub enum TaskEventKind {
    /// SDK `task_started`.
    Started {
        task_type: String,
        description: String,
        /// The script's `meta.name` (workflows).
        workflow_name: Option<String>,
        /// Workflows: the run id and where its transcripts and journal go.
        run_id: Option<String>,
        transcript_dir: Option<String>,
        /// Shell tasks (`local_bash`: a backgrounded Bash command or a
        /// Monitor script): the command line, and the file its output goes to.
        command: Option<String>,
        output_file: Option<String>,
    },
    /// A shell task wrote output: its output file (a monitor: its stream)
    /// has grown to `total_bytes`. At most about once a second per task.
    Output { total_bytes: u64 },
    /// A monitor told its agent of new lines: `text` is the batch as the
    /// agent reads it, or a notice of the monitor's own (lines dropped for
    /// coming too fast, a timeout).
    Event { text: String },
    /// SDK `task_progress`. `workflow_progress` is the whole progress list
    /// (a JSON array of Claude Code's `workflow_*` rows) when it changed,
    /// as text so each row keeps Claude Code's key order.
    Progress {
        description: String,
        usage: TaskUsage,
        last_tool_name: Option<String>,
        summary: Option<String>,
        workflow_progress: Option<Arc<serde_json::value::RawValue>>,
    },
    /// SDK `task_notification`: `completed`, `failed` or `stopped`.
    Ended {
        status: String,
        summary: String,
        output_file: Option<String>,
        usage: TaskUsage,
        workflow_progress: Option<Arc<serde_json::value::RawValue>>,
        /// Shell tasks: the command's exit code, when it exited by itself.
        exit_code: Option<i32>,
    },
}

/// Where an agent's tools report its background tasks, kept in the agent's
/// [`Extensions`](crate::Extensions) when the host listens.
#[derive(Clone)]
pub struct TaskEvents(Arc<dyn Fn(TaskEvent) -> BoxFuture<'static, ()> + Send + Sync>);

impl std::fmt::Debug for TaskEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskEvents(..)")
    }
}

impl TaskEvents {
    pub fn new(send: impl Fn(TaskEvent) -> BoxFuture<'static, ()> + Send + Sync + 'static) -> Self {
        Self(Arc::new(send))
    }

    pub fn send(&self, event: TaskEvent) -> BoxFuture<'static, ()> {
        (self.0)(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_waits_for_the_task() {
        let tasks = BackgroundTasks::default();
        let task = Arc::new(BackgroundTask::new("w1", "local_workflow", "demo"));
        tasks.insert(Arc::clone(&task));
        let runner = Arc::clone(&task);
        let ran = tokio::spawn(async move {
            runner.stop_requested().cancelled().await;
            runner.finish(TaskStatus::Killed);
        });
        tasks.get("w1").expect("listed").stop().await;
        ran.await.expect("task");
        assert_eq!(task.status(), TaskStatus::Killed);
        assert!(tasks.running("local_workflow").is_empty());
    }
}
