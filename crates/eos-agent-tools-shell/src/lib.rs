//! Claude Code's shell tools for Eos agents: `Bash` (foreground and background
//! commands), `Monitor` (a command whose stdout lines become events) and
//! `TaskStop`.
//!
//! Every command is a fresh `$SHELL -c` in its own process group that sources
//! a shared snapshot of the user's shell. The tools are shared by all agents;
//! each agent's [`Workspace`](eos_agent_core::Workspace) gives the working
//! directory and the scratch space, its [`Reminders`](eos_agent_core::Reminders)
//! carry completion notices and monitor events (each also rings its
//! [`Wake`](eos_agent_core::Wake), so an idle agent takes them up in a turn
//! of its own), its [`ShellTasks`] holds
//! its background tasks, which die with it, and its [`AgentSandbox`] (if any)
//! confines its commands.

mod background;
mod bash;
mod classify;
mod common;
mod config;
mod cwd;
mod lexer;
mod monitor;
mod notify;
mod output;
mod process;
pub mod sandbox;
mod semantics;
mod snapshot;
mod task_stop;
mod tasks;

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_core::Tool;

pub use bash::Bash;
pub use classify::is_read_only as is_read_only_command;
pub use config::{DEFAULT_PROCESS_BUDGET, Limits, ShellConfig};
pub use monitor::Monitor;
pub use sandbox::{
    AgentSandbox, SandboxConfig, SandboxDirs, SandboxLayers, SandboxPolicy, SandboxRuntime, SandboxSession,
    sandbox_schema,
};
pub use snapshot::ShellSnapshots;
pub use task_stop::TaskStop;
pub use tasks::ShellTasks;

/// What Bash and Monitor share: the resolved shell and the shared config.
#[derive(Debug)]
pub(crate) struct Shell {
    pub path: PathBuf,
    pub config: ShellConfig,
}

#[derive(Debug)]
pub struct ShellTools;

impl ShellTools {
    /// Bash, Monitor and TaskStop over one shared config.
    #[expect(clippy::new_ret_no_self, reason = "a tool family, not a value: the toolbox takes the tools")]
    pub fn new(config: ShellConfig) -> Vec<Arc<dyn Tool>> {
        let shell = Arc::new(Shell { path: config.resolve_shell(), config });
        vec![Arc::new(Bash::new(Arc::clone(&shell))), Arc::new(Monitor::new(shell)), Arc::new(TaskStop::new())]
    }
}
