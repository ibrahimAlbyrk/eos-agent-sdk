//! A running monitor: reads stdout, delivers batches as events, reports how
//! the script ended.

use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::TaskNotices;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout};
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, sleep_until};

use super::batch::{BATCH_WINDOW, Lines, RateLimit, Verdict};
use crate::notify::{self, Notification};
use crate::process::exit_code;
use crate::sandbox::SandboxGuard;
use crate::tasks::{Status, Task};

const TIMED_OUT: &str = "[Monitor timed out \u{2014} re-arm if needed.]";
/// After the script exits, how long its stdout may stay open (held by a
/// process it left behind) before we stop reading.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(200);

pub(super) struct MonitorRun {
    pub child: Child,
    pub task: Arc<Task>,
    pub tool_use_id: String,
    pub description: String,
    /// `None` for a persistent monitor.
    pub timeout: Option<Duration>,
    /// Its agent's notes, which wake the agent if it is idle.
    pub notices: TaskNotices,
    /// A sandboxed script's proxy credentials, valid until it ends.
    pub sandbox: Option<SandboxGuard>,
}

enum End {
    Exited(i32),
    TimedOut,
    Stopped,
    Flooded(String),
}

impl MonitorRun {
    /// The permit is held until the script has exited.
    pub(super) fn spawn(self, permit: OwnedSemaphorePermit) {
        tokio::spawn(self.watch(permit));
    }

    async fn watch(mut self, permit: OwnedSemaphorePermit) {
        self.task.announce(&self.description).await;
        let started = Instant::now();
        let mut stdout = self.child.stdout.take();
        let mut copy = File::options().append(true).open(&self.task.output_file).await.ok();
        let mut lines = Lines::default();
        let mut limit = RateLimit::new(started.into_std());
        let mut flush_at: Option<Instant> = None;
        let mut exited: Option<(i32, Instant)> = None;
        let mut stdout_bytes = 0u64;
        let mut reported = 0u64;
        let mut buf = vec![0u8; 8192];
        let far = started + Duration::from_secs(86_400 * 365);
        let deadline = self.timeout.map_or(far, |t| started + t);

        let end = loop {
            let drain_until = exited.map_or(far, |(_, at)| at + DRAIN_AFTER_EXIT);
            tokio::select! {
                read = read_some(&mut stdout, &mut buf) => match read {
                    Some(n) => {
                        stdout_bytes += n as u64;
                        if let Some(f) = copy.as_mut() {
                            let _ = f.write_all(&buf[..n]).await;
                        }
                        lines.push(&buf[..n]);
                        if lines.has_pending() && flush_at.is_none() {
                            flush_at = Some(Instant::now() + BATCH_WINDOW);
                        }
                    }
                    None => stdout = None,
                },
                () = sleep_until(flush_at.unwrap_or(far)), if flush_at.is_some() => {
                    flush_at = None;
                    reported = stdout_bytes;
                    self.task.wrote(stdout_bytes).await;
                    if let Some(stop) = self.deliver(&mut lines, &mut limit, false).await {
                        break End::Flooded(stop);
                    }
                }
                status = self.child.wait(), if exited.is_none() => {
                    exited = Some((status.map(exit_code).unwrap_or(-1), Instant::now()));
                }
                () = sleep_until(drain_until), if exited.is_some() => stdout = None,
                () = sleep_until(deadline), if exited.is_none() => break End::TimedOut,
                () = self.task.stop_requested().cancelled(), if exited.is_none() => break End::Stopped,
            }
            if let (Some((code, _)), None) = (exited, &stdout) {
                break End::Exited(code);
            }
        };

        if exited.is_none() {
            self.task.kill();
            let _ = self.child.wait().await;
        }
        drop(permit);
        drop(self.sandbox.take());
        if stdout_bytes != reported {
            self.task.wrote(stdout_bytes).await;
        }
        match end {
            End::Exited(code) => {
                let rest = lines
                    .take(true)
                    .filter(|_| matches!(limit.judge(Instant::now().into_std()), Verdict::Deliver { .. }));
                let status = if code == 0 { Status::Completed } else { Status::Failed };
                let summary = self.report_exit(status, code, stdout_bytes, rest);
                self.task.end(status, Some(code), &summary).await;
            }
            End::TimedOut => {
                let _ = self.deliver(&mut lines, &mut limit, true).await;
                self.event(TIMED_OUT).await;
                self.task.end(Status::Killed, None, &self.ended("timed out")).await;
            }
            End::Flooded(notice) => {
                self.event(&notice).await;
                self.task.end(Status::Killed, None, &self.ended("was stopped for writing too much")).await;
            }
            End::Stopped => self.task.end(Status::Killed, None, &self.ended("was stopped")).await,
        }
    }

    /// The host's summary of an end the script did not choose.
    fn ended(&self, how: &str) -> String {
        format!("Monitor \"{}\" {how}", self.description)
    }

    /// Sends the pending lines as an event unless rate-limited. Returns the
    /// stop notice when the monitor has flooded too long.
    async fn deliver(&self, lines: &mut Lines, limit: &mut RateLimit, last: bool) -> Option<String> {
        let batch = lines.take(last)?;
        match limit.judge(Instant::now().into_std()) {
            Verdict::Deliver { notice } => {
                if let Some(notice) = notice {
                    self.event(&notice).await;
                }
                self.event(&batch).await;
                None
            }
            Verdict::Suppress => None,
            Verdict::Stop(notice) => Some(notice),
        }
    }

    /// Tells the agent, then the host.
    async fn event(&self, text: &str) {
        let summary = format!("Monitor event: \"{}\"", self.description);
        let notification =
            Notification { task_id: &self.task.id, summary: &summary, body: notify::event(text), ..Default::default() };
        self.notices.push(notification.render());
        self.task.event(text).await;
    }

    /// Tells the model the script exited; returns the summary it was given.
    fn report_exit(&self, status: Status, code: i32, stdout_bytes: u64, rest: Option<String>) -> String {
        let desc = &self.description;
        let summary = match status {
            Status::Failed => format!("Monitor \"{desc}\" script failed (exit {code})"),
            _ if stdout_bytes == 0 => format!("Monitor \"{desc}\" ended without producing output (exit {code})"),
            _ => format!("Monitor \"{desc}\" stream ended"),
        };
        let notification = Notification {
            task_id: &self.task.id,
            tool_use_id: Some(&self.tool_use_id),
            output_file: Some(&self.task.output_file),
            status: Some(status.as_str()),
            summary: &summary,
            body: rest.map(|r| notify::event(&r)).unwrap_or_default(),
        };
        self.notices.push(notification.render());
        summary
    }
}

/// `None` at end of stream (or with no stream); pends forever once closed.
async fn read_some(stdout: &mut Option<ChildStdout>, buf: &mut [u8]) -> Option<usize> {
    match stdout {
        Some(out) => match out.read(buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(n),
        },
        None => std::future::pending().await,
    }
}
