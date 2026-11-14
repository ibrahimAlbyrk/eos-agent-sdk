//! Watching a backgrounded command until it ends, then telling the model.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::TaskNotices;
use tokio::process::Child;
use tokio::sync::OwnedSemaphorePermit;

use crate::notify::{self, Notification};
use crate::process::exit_code;
use crate::sandbox::SandboxGuard;
use crate::semantics::interpret_exit;
use crate::tasks::{Status, Task};

const DEADLINE_NOTE: &str = "If the work in progress still needs it, start it again with `run_in_background` and a \
     longer `timeout`. If it already had the longest `timeout` allowed, do not restart it. Either way, report that it \
     was stopped.";

/// A running command handed to the background.
pub(crate) struct Background {
    pub child: Child,
    pub permit: OwnedSemaphorePermit,
    pub task: Arc<Task>,
    pub tool_use_id: String,
    pub description: String,
    pub deadline: Duration,
    /// Its agent's notes, which wake the agent if it is idle.
    pub notices: TaskNotices,
    /// Removed when the command ends (the cwd file of a command that started
    /// in the foreground).
    pub leftover: Option<PathBuf>,
    /// A sandboxed command's proxy credentials, valid until it ends.
    pub sandbox: Option<SandboxGuard>,
}

/// How often a followed task's output file is checked for growth.
const OUTPUT_LOOK: Duration = Duration::from_secs(1);

enum End {
    Exited(i32),
    Deadline,
    Stopped,
}

impl Background {
    pub(crate) fn spawn(self) {
        tokio::spawn(self.watch());
    }

    async fn watch(mut self) {
        self.task.announce(&self.description).await;
        let deadline = tokio::time::sleep(self.deadline);
        tokio::pin!(deadline);
        // The command writes its output file itself: a followed task's file
        // is looked at once a second.
        let mut look = tokio::time::interval(OUTPUT_LOOK);
        let mut written = 0;
        let end = loop {
            tokio::select! {
                status = self.child.wait() => break End::Exited(status.map(exit_code).unwrap_or(-1)),
                () = &mut deadline => break End::Deadline,
                () = self.task.stop_requested().cancelled() => break End::Stopped,
                _ = look.tick(), if self.task.is_followed() => self.report_output(&mut written).await,
            }
        };
        if !matches!(end, End::Exited(_)) {
            self.task.kill();
            let _ = self.child.wait().await;
        }
        self.report_output(&mut written).await;
        drop(self.permit);
        // Claude Code reports violations for foreground results only.
        drop(self.sandbox.take());
        if let Some(path) = &self.leftover {
            let _ = tokio::fs::remove_file(path).await;
        }
        let desc = &self.description;
        let (status, summary, body) = match end {
            End::Exited(code) => {
                let exit = interpret_exit(&self.task.command, code);
                if exit.is_error {
                    (
                        Status::Failed,
                        format!("Background command \"{desc}\" failed with exit code {code}"),
                        String::new(),
                    )
                } else {
                    let note = exit.note.map(|n| format!(": {n}")).unwrap_or_default();
                    let summary = format!("Background command \"{desc}\" completed (exit code {code}{note})");
                    (Status::Completed, summary, String::new())
                }
            }
            End::Deadline => (
                Status::Killed,
                format!("Background command \"{desc}\" was stopped after reaching its background time limit"),
                notify::note(DEADLINE_NOTE),
            ),
            // TaskStop's own result tells the model.
            End::Stopped => {
                let summary = format!("Background command \"{desc}\" was stopped");
                self.task.end(Status::Killed, None, &summary).await;
                return;
            }
        };
        let exit_code = match end {
            End::Exited(code) => Some(code),
            _ => None,
        };
        let notification = Notification {
            task_id: &self.task.id,
            tool_use_id: Some(&self.tool_use_id),
            output_file: Some(&self.task.output_file),
            status: Some(status.as_str()),
            summary: &summary,
            body,
        };
        self.notices.push(notification.render());
        self.task.end(status, exit_code, &summary).await;
    }

    async fn report_output(&self, written: &mut u64) {
        if !self.task.is_followed() {
            return;
        }
        let size = tokio::fs::metadata(&self.task.output_file).await.map_or(*written, |m| m.len());
        if size != *written {
            *written = size;
            self.task.wrote(size).await;
        }
    }
}
