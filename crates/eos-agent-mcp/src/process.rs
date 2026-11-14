//! A stdio server's process, in its own process group so that whatever it
//! starts (npx → node → …) goes down with it.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessGroup};
use tokio::process::{ChildStdin, ChildStdout};

/// How long a server gets to exit on SIGTERM before the group is killed.
const TERM_GRACE: Duration = Duration::from_millis(500);

/// Owns the child. Dropping it kills the whole group at once;
/// [`shutdown`](Self::shutdown) asks politely first.
pub(crate) struct ChildGuard {
    child: Option<Box<dyn ChildWrapper>>,
}

impl std::fmt::Debug for ChildGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildGuard").field("pid", &self.child.as_ref().and_then(|c| c.id())).finish()
    }
}

pub(crate) fn spawn(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: Option<&Path>,
) -> std::io::Result<(ChildGuard, ChildStdout, ChildStdin)> {
    let mut wrap = CommandWrap::with_new(command, |cmd| {
        cmd.args(args).envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
    });
    let mut child = wrap.wrap(ProcessGroup::leader()).spawn()?;
    let stdout = child.stdout().take();
    let stdin = child.stdin().take();
    let guard = ChildGuard { child: Some(child) };
    match (stdout, stdin) {
        (Some(stdout), Some(stdin)) => Ok((guard, stdout, stdin)),
        _ => Err(std::io::Error::other("the server's stdio pipes were not opened")),
    }
}

impl ChildGuard {
    /// SIGTERM to the group, a short grace, then SIGKILL to whatever is left.
    /// The leader is reaped only after the last signal, so its pid (the group
    /// id) cannot have been reused by then.
    pub(crate) async fn shutdown(mut self) {
        let Some(mut child) = self.child.take() else { return };
        let _ = child.signal(libc::SIGTERM);
        tokio::time::sleep(TERM_GRACE).await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else { return };
        let _ = child.start_kill();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}
