//! Starting shell commands and stopping their process groups.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};

use crate::sandbox::{CommandEnv, Wrapped};

/// Where a command's output goes.
pub(crate) enum Sink {
    /// stdout and stderr both appended to this file, interleaved as written.
    File(File),
    /// stdout piped back to us, stderr appended to the file.
    PipeAndFile(File),
}

/// What to run and how.
pub(crate) struct Launch<'a> {
    pub shell: &'a Path,
    pub snapshot: Option<&'a Path>,
    pub command: &'a str,
    pub cwd: &'a Path,
    /// Where `pwd -P` lands after the command succeeds; `None` for commands
    /// that may not move the working directory.
    pub cwd_file: Option<&'a Path>,
    /// Run under the sandbox: this program first, with these variables.
    pub sandbox: Option<(&'a Wrapped, &'a CommandEnv)>,
}

impl Launch<'_> {
    /// The shell runs in its own process group (its pid is the group id), so
    /// stopping it reaches every process it started.
    pub(crate) fn spawn(&self, sink: Sink) -> io::Result<Child> {
        let mut cmd = match self.sandbox {
            Some((wrapped, env)) => {
                let mut cmd = Command::new(&wrapped.program);
                cmd.args(&wrapped.args).arg(self.shell);
                for name in &env.unset {
                    cmd.env_remove(name);
                }
                cmd.envs(env.set.iter().map(|(k, v)| (k, v)));
                cmd
            }
            None => Command::new(self.shell),
        };
        cmd.arg("-c");
        if self.snapshot.is_none() {
            // Without a snapshot the login shell is the only way to get the user's profile.
            cmd.arg("-l");
        }
        cmd.arg(self.command_string())
            .current_dir(self.cwd)
            .stdin(Stdio::null())
            .env("SHELL", self.shell)
            .env("GIT_EDITOR", "true")
            .env("CLAUDECODE", "1")
            .process_group(0)
            .kill_on_drop(false);
        match sink {
            Sink::File(file) => {
                let err = file.try_clone()?;
                cmd.stdout(file).stderr(err);
            }
            Sink::PipeAndFile(file) => {
                cmd.stdout(Stdio::piped()).stderr(file);
            }
        }
        cmd.spawn()
    }

    fn command_string(&self) -> String {
        let mut parts = Vec::with_capacity(4);
        if let Some(snapshot) = self.snapshot {
            parts.push(format!("source {} 2>/dev/null || true", quote(&snapshot.to_string_lossy())));
        }
        let name = self.shell.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains("bash") {
            parts.push("shopt -u extglob 2>/dev/null || true".to_owned());
        } else if name.contains("zsh") {
            parts.push("setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL 2>/dev/null || true".to_owned());
        }
        parts.push(format!("eval {}", quote(self.command)));
        if let Some(file) = self.cwd_file {
            parts.push(format!("pwd -P >| {}", quote(&file.to_string_lossy())));
        }
        parts.join(" && ")
    }
}

/// Single-quotes `s` for any POSIX shell.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

pub(crate) fn append_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// Sends SIGKILL to the process group led by `pgid`.
pub(crate) fn kill_group(pgid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else { return };
    // 0 and 1 would mean our own group and init.
    if pgid > 1 {
        killpg(pgid);
    }
}

#[allow(unsafe_code)]
fn killpg(pgid: libc::pid_t) {
    // SAFETY: killpg takes plain integers and only sends a signal; it touches
    // no memory of ours. A failure (the group is already gone) is harmless.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

/// Kills the group when dropped unless disarmed: a command whose call is
/// dropped (the agent was interrupted) must not keep running.
#[derive(Debug)]
pub(crate) struct GroupGuard(Option<u32>);

impl GroupGuard {
    pub(crate) fn new(pgid: Option<u32>) -> Self {
        Self(pgid)
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            kill_group(pgid);
        }
    }
}

/// The shell's exit code, with death by signal reported as 128 + signal the
/// way shells do.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(quote("it's"), r#"'it'"'"'s'"#);
    }

    #[test]
    fn command_string_sources_snapshot_and_records_cwd() {
        let launch = Launch {
            shell: Path::new("/bin/zsh"),
            snapshot: Some(Path::new("/s/snap.sh")),
            command: "echo hi",
            cwd: Path::new("/"),
            cwd_file: Some(Path::new("/t/cwd")),
            sandbox: None,
        };
        assert_eq!(
            launch.command_string(),
            "source '/s/snap.sh' 2>/dev/null || true && setopt NO_EXTENDED_GLOB NO_BARE_GLOB_QUAL 2>/dev/null || true \
             && eval 'echo hi' && pwd -P >| '/t/cwd'"
        );
    }
}
