//! Pieces Bash, Monitor and TaskStop share.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{Extensions, ToolContext, ToolOutput, ToolSpec, Workspace};
use tokio::sync::OwnedSemaphorePermit;

use crate::Shell;
use crate::sandbox::{AgentSandbox, Prepare, Prepared, SandboxSession};

pub(crate) fn spec(name: &str, prompt: &str, schema: &str) -> ToolSpec {
    let schema = serde_json::from_str(schema).expect("bundled schemas are valid JSON");
    ToolSpec::new(name, prompt, schema)
}

/// A refused call, worded as Claude Code words validation failures.
pub(crate) fn refused(message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

pub(crate) fn invalid_input(err: &serde_json::Error) -> ToolOutput {
    refused(format_args!("InputValidationError: {err}"))
}

pub(crate) fn workspace(ctx: &ToolContext<'_>) -> Result<Arc<Workspace>, ToolOutput> {
    ctx.extensions
        .get::<Workspace>()
        .ok_or_else(|| ToolOutput::error("Error: this agent has no workspace to run commands in"))
}

/// `<scratch>/tasks`, where task output files live.
pub(crate) async fn tasks_dir(workspace: &Workspace) -> Result<PathBuf, ToolOutput> {
    let dir = workspace.scratch().join("tasks");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| ToolOutput::error(format!("Error: could not create {}: {e}", dir.display())))?;
    Ok(dir)
}

impl Shell {
    pub(crate) async fn snapshot(&self) -> Option<PathBuf> {
        self.config.snapshots.get(&self.path, &self.config.budget).await
    }

    /// What a command needs to run sandboxed; `None` when it runs as is.
    pub(crate) async fn sandboxing(
        &self,
        command: &str,
        sandboxed: bool,
        ctx: &ToolContext<'_>,
        workspace: &Workspace,
        write_files: &[PathBuf],
    ) -> Result<Option<Prepared>, ToolOutput> {
        let Some(sandbox) = ctx.extensions.get::<AgentSandbox>().filter(|_| sandboxed) else { return Ok(None) };
        let session = ctx.extensions.get::<SandboxSession>().map_or_else(|| ctx.agent_id.to_owned(), |s| s.0.clone());
        let prepare = Prepare {
            command,
            agent_id: ctx.agent_id,
            session: &session,
            cwd: &workspace.cwd(),
            roots: workspace.roots(),
            tmpdir: &workspace.scratch().join("tmp"),
            write_files,
        };
        sandbox
            .0
            .prepare(prepare)
            .await
            .map(Some)
            .map_err(|e| ToolOutput::error(format!("Error: could not set up the sandbox: {e}")))
    }

    /// One slot of the shared process budget, held while the process runs.
    pub(crate) async fn permit(&self) -> Result<OwnedSemaphorePermit, ToolOutput> {
        Arc::clone(&self.config.budget)
            .acquire_owned()
            .await
            .map_err(|_| ToolOutput::error("Error: the process budget is closed"))
    }
}

/// Would the agent with these extensions run `command` sandboxed?
pub(crate) fn sandboxed(extensions: &Extensions, command: &str, disable_sandbox: bool) -> bool {
    extensions.get::<AgentSandbox>().is_some_and(|s| s.0.should_sandbox(command, disable_sandbox))
}

/// `10m`, `1h 30m`, `45s`: the largest units, trailing zero units left out.
pub(crate) fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut parts = Vec::with_capacity(3);
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 || (h > 0 && s > 0) {
        parts.push(format!("{m}m"));
    }
    if s > 0 {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_drop_trailing_zero_units() {
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(600)), "10m");
        assert_eq!(human_duration(Duration::from_secs(5400)), "1h 30m");
        assert_eq!(human_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(human_duration(Duration::from_secs(3601)), "1h 0m 1s");
    }
}
