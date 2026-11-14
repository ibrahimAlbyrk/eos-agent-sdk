//! Carrying `cd` over to the next call.

use std::path::{Path, PathBuf};

use eos_agent_core::Workspace;

/// Applies the directory a command ended in (`pwd -P`, so symlinks resolved).
/// Inside the roots it becomes the cwd; outside, the cwd goes back to the
/// start folder and the returned note says so.
pub(crate) async fn carry_over(workspace: &Workspace, ended_in: &Path) -> Option<String> {
    let current = workspace.cwd();
    if tokio::fs::canonicalize(&current).await.is_ok_and(|c| c == ended_in) {
        return None;
    }
    if let Some(dir) = logical(workspace, ended_in).await
        && workspace.set_cwd(&dir)
    {
        return None;
    }
    let start = workspace.roots().first()?.clone();
    workspace.set_cwd(&start);
    Some(format!("Shell cwd was reset to {}", start.display()))
}

/// `physical` spelled under the root it lies in, as the roots are spelled
/// (`/tmp/x` rather than `/private/tmp/x`).
async fn logical(workspace: &Workspace, physical: &Path) -> Option<PathBuf> {
    for root in workspace.roots() {
        if physical.starts_with(root) {
            return Some(physical.to_path_buf());
        }
        if let Ok(real) = tokio::fs::canonicalize(root).await
            && let Ok(rest) = physical.strip_prefix(&real)
        {
            return Some(root.join(rest));
        }
    }
    None
}
