//! Git worktrees for subagents that asked for `isolation: "worktree"`, where
//! Claude Code makes them: `<repo>/.claude/worktrees/<slug>` on a branch
//! `worktree-<slug>` from the current `HEAD`, removed afterwards unless the
//! agent changed something in it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eos_agent_core::Worktree;

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git could not run: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Claude Code's slug rule: letters, digits, `.`, `_`, `-`, `+` kept.
fn clean(slug: &str) -> String {
    slug.chars().map(|c| if c.is_ascii_alphanumeric() || "._-+".contains(c) { c } else { '-' }).collect()
}

/// Makes (or reuses) the worktree `slug` of the repository `cwd` is in.
/// Blocking: a few git commands.
pub(crate) fn create(cwd: &Path, slug: &str) -> Result<Worktree, String> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| format!("Cannot create a worktree: {} is not in a git repository.", cwd.display()))?;
    let root = PathBuf::from(root);
    let head = git(&root, &["rev-parse", "HEAD"]).map_err(|e| format!("Cannot create a worktree: {e}"))?;
    let slug = clean(slug);
    let path = root.join(".claude").join("worktrees").join(&slug);
    let branch = format!("worktree-{slug}");
    if path.join(".git").exists() {
        return Ok(Worktree { path, branch, head_commit: head, git_root: root });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create a worktree: {e}"))?;
    }
    let target = path.to_string_lossy().into_owned();
    let made = git(&root, &["worktree", "add", "-b", &branch, &target, "HEAD"])
        .or_else(|_| git(&root, &["worktree", "add", &target, &branch]));
    made.map_err(|e| format!("Cannot create a worktree: {e}"))?;
    Ok(Worktree { path, branch, head_commit: head, git_root: root })
}

/// Removes the worktree and its branch when nothing changed in it (no
/// uncommitted changes, no new commits); true when it was kept.
pub(crate) fn release(worktree: &Worktree) -> bool {
    let dirty = git(&worktree.path, &["status", "--porcelain"]).map_or(true, |s| !s.is_empty());
    let ahead = git(&worktree.path, &["rev-list", "--count", &format!("{}..HEAD", worktree.head_commit)])
        .ok()
        .and_then(|n| n.parse::<u64>().ok())
        .is_none_or(|n| n > 0);
    if dirty || ahead {
        return true;
    }
    let target = worktree.path.to_string_lossy().into_owned();
    if git(&worktree.git_root, &["worktree", "remove", "--force", &target]).is_err() {
        return true;
    }
    let _ = git(&worktree.git_root, &["branch", "-D", &worktree.branch]);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eos-worktree-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).expect("dir");
        for args in [
            vec!["init", "-q"],
            vec!["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            git(&dir, &args).expect("git");
        }
        dir
    }

    #[test]
    fn unchanged_worktrees_go_changed_ones_stay() {
        let dir = repo();
        let wt = create(&dir, "agent-a1").expect("worktree");
        assert!(wt.path.ends_with(".claude/worktrees/agent-a1") && wt.path.join(".git").exists());
        assert_eq!(wt.branch, "worktree-agent-a1");
        assert!(!release(&wt));
        assert!(!wt.path.exists());

        let wt = create(&dir, "wf_1a2b3c4d-5e6-2").expect("worktree");
        std::fs::write(wt.path.join("new.txt"), "x").expect("write");
        assert!(release(&wt));
        assert!(wt.path.exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
