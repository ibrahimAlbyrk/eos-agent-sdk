//! Facts about the machine and the working directory for the environment
//! message, gathered without running `git` per agent.

use std::path::Path;
use std::sync::OnceLock;

/// What the environment message states about where the agent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFacts {
    /// As Node spells it (`darwin`), which is what Claude Code prints.
    pub platform: String,
    pub shell: String,
    /// `uname -sr`, e.g. `Darwin 25.1.0`; `None` leaves the line out.
    pub os_version: Option<String>,
    pub is_git_repo: bool,
    /// The directory is a linked git worktree (its `.git` is a file).
    pub is_worktree: bool,
}

impl EnvFacts {
    /// Host facts are read once per process; the repository check is a few
    /// `stat` calls up from `cwd`.
    pub fn detect(cwd: &Path) -> Self {
        let host = host();
        let (is_git_repo, is_worktree) = git_state(cwd);
        Self {
            platform: host.platform.clone(),
            shell: host.shell.clone(),
            os_version: host.os_version.clone(),
            is_git_repo,
            is_worktree,
        }
    }
}

#[derive(Debug)]
struct Host {
    platform: String,
    shell: String,
    os_version: Option<String>,
}

fn host() -> &'static Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    HOST.get_or_init(|| Host {
        platform: node_platform(std::env::consts::OS).to_owned(),
        shell: shell_name(std::env::var("SHELL").ok().as_deref()),
        os_version: os_version(),
    })
}

fn node_platform(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Claude Code names the shell by family when it can tell.
fn shell_name(shell: Option<&str>) -> String {
    match shell {
        None | Some("") => "unknown".into(),
        Some(s) if s.contains("zsh") => "zsh".into(),
        Some(s) if s.contains("bash") => "bash".into(),
        Some(s) => s.into(),
    }
}

/// The kernel name and release are not in any plain file on macOS, and
/// reading them through `uname(3)` needs `unsafe`, so the process asks the
/// `uname` binary once.
fn os_version() -> Option<String> {
    let out = std::process::Command::new("uname").arg("-sr").output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (out.status.success() && !text.is_empty()).then(|| text.to_owned())
}

/// Walks up from `dir` looking for `.git`, like git's own discovery (without
/// `GIT_DIR` or ceiling directories).
fn git_state(dir: &Path) -> (bool, bool) {
    for d in dir.ancestors() {
        match std::fs::symlink_metadata(d.join(".git")) {
            Ok(meta) if meta.is_dir() => return (true, false),
            Ok(_) => return (true, d == dir && is_linked_worktree(&d.join(".git"))),
            Err(_) => {}
        }
    }
    (false, false)
}

fn is_linked_worktree(dot_git: &Path) -> bool {
    std::fs::read_to_string(dot_git)
        .is_ok_and(|s| s.strip_prefix("gitdir:").is_some_and(|rest| rest.contains("/worktrees/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_shells_like_claude_code() {
        assert_eq!(shell_name(Some("/bin/zsh")), "zsh");
        assert_eq!(shell_name(Some("/usr/local/bin/bash")), "bash");
        assert_eq!(shell_name(Some("/usr/bin/fish")), "/usr/bin/fish");
        assert_eq!(shell_name(None), "unknown");
        assert_eq!(node_platform("macos"), "darwin");
    }

    #[test]
    fn finds_repositories_and_worktrees() {
        let root = std::env::temp_dir().join(format!("eos-prompt-env-{}", std::process::id()));
        let nested = root.join("repo/src/deep");
        std::fs::create_dir_all(&nested).expect("mkdir");
        assert_eq!(git_state(&nested), (false, false));
        std::fs::create_dir_all(root.join("repo/.git")).expect("mkdir");
        assert_eq!(git_state(&nested), (true, false));
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(wt.join(".git"), "gitdir: /x/repo/.git/worktrees/wt\n").expect("write");
        assert_eq!(git_state(&wt), (true, true));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
