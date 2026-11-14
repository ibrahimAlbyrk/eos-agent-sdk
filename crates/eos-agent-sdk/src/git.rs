//! Claude Code's `# gitStatus` snapshot for the first message.

use std::path::Path;
use std::process::{Command, Stdio};

/// Longer `git status --short` output is cut, as Claude Code cuts it.
const MAX_STATUS_CHARS: usize = 2000;

/// The snapshot text, or `None` outside a git work tree (or without git).
/// Blocking: five short git commands.
pub(crate) fn status(cwd: &Path) -> Option<String> {
    if git(cwd, &["rev-parse", "--is-inside-work-tree"])? != "true" {
        return None;
    }
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "HEAD".into());
    let main = main_branch(cwd);
    let user = git(cwd, &["config", "user.name"]).unwrap_or_default();
    let mut changes = git(cwd, &["--no-optional-locks", "status", "--short"]).unwrap_or_default();
    if changes.chars().count() > MAX_STATUS_CHARS {
        changes = changes.chars().take(MAX_STATUS_CHARS).collect::<String>()
            + " ... (truncated because it exceeds 2k characters. If you need more information, run \"git status\" \
               using Bash)";
    }
    let commits = git(cwd, &["--no-optional-locks", "log", "--oneline", "-n", "5"]).unwrap_or_default();
    let mut parts = vec![
        "This is the git status at the start of the conversation. Note that this status is a snapshot in time, and \
         will not update during the conversation."
            .to_owned(),
        format!("Current branch: {branch}"),
        format!("Main branch (you will usually use this for PRs): {main}"),
    ];
    if !user.is_empty() {
        parts.push(format!("Git user: {user}"));
    }
    parts.push(format!("Status:\n{}", if changes.is_empty() { "(clean)" } else { &changes }));
    parts.push(format!("Recent commits:\n{commits}"));
    Some(parts.join("\n\n"))
}

/// The remote's default branch, else `main` or `master` if one exists.
fn main_branch(cwd: &Path) -> String {
    if let Some(head) = git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return head.strip_prefix("origin/").unwrap_or(&head).to_owned();
    }
    ["main", "master"]
        .into_iter()
        .find(|b| git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_some())
        .unwrap_or("main")
        .to_owned()
}

/// Trimmed stdout of a successful git command.
fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_of_a_fresh_repo() {
        let dir = std::env::temp_dir().join(format!("eos-git-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert!(status(&dir).is_none(), "not a repo");
        let run = |args: &[&str]| {
            Command::new("git").args(args).current_dir(&dir).stdout(Stdio::null()).status().expect("git");
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "a").expect("write");
        run(&["add", "."]);
        run(&["-c", "user.name=T", "-c", "user.email=t@t", "commit", "-qm", "first"]);
        std::fs::write(dir.join("b.txt"), "b").expect("write");
        let text = status(&dir).expect("a repo");
        assert!(text.starts_with("This is the git status at the start of the conversation."));
        assert!(text.contains("\n\nCurrent branch: main\n\nMain branch (you will usually use this for PRs): main\n\n"));
        assert!(text.contains("Status:\n?? b.txt\n\nRecent commits:\n"));
        assert!(text.ends_with(" first"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
