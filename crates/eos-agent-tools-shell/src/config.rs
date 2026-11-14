use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::snapshot::ShellSnapshots;

/// Child processes the shell tools may run at once across every agent sharing
/// one budget. macOS allows about 2666 processes per user.
pub const DEFAULT_PROCESS_BUDGET: usize = 256;

/// Claude Code's limits. The tool descriptions state the defaults, so changing
/// one here makes the description less accurate.
#[derive(Debug, Clone)]
pub struct Limits {
    pub timeout_default: Duration,
    pub timeout_max: Duration,
    pub background_default: Duration,
    pub background_max: Duration,
    pub monitor_default: Duration,
    pub monitor_max: Duration,
    /// Results longer than this many chars are saved to a file and previewed.
    pub output_max_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout_default: Duration::from_millis(120_000),
            timeout_max: Duration::from_millis(600_000),
            background_default: Duration::from_millis(600_000),
            background_max: Duration::from_millis(7_200_000),
            monitor_default: Duration::from_millis(300_000),
            monitor_max: Duration::from_millis(3_600_000),
            output_max_chars: 30_000,
        }
    }
}

/// What the shell tools share between agents. Build one per process and hand
/// clones of it to every toolbox, so all agents share the snapshot and the
/// process budget.
#[derive(Debug, Clone)]
pub struct ShellConfig {
    /// The user's shell; `None` takes `$SHELL` when it is bash or zsh, else
    /// `/bin/zsh`, else `/bin/bash`.
    pub shell: Option<PathBuf>,
    pub snapshots: Arc<ShellSnapshots>,
    pub budget: Arc<Semaphore>,
    pub limits: Limits,
}

impl ShellConfig {
    /// Snapshots go under `snapshot_dir`; the budget is [`DEFAULT_PROCESS_BUDGET`].
    pub fn new(snapshot_dir: impl Into<PathBuf>) -> Self {
        Self {
            shell: None,
            snapshots: Arc::new(ShellSnapshots::new(snapshot_dir.into())),
            budget: Arc::new(Semaphore::new(DEFAULT_PROCESS_BUDGET)),
            limits: Limits::default(),
        }
    }

    pub(crate) fn resolve_shell(&self) -> PathBuf {
        if let Some(shell) = &self.shell {
            return shell.clone();
        }
        let from_env = std::env::var_os("SHELL").map(PathBuf::from);
        let usable = |p: &Path| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            (name.contains("zsh") || name.contains("bash")) && p.is_file()
        };
        from_env
            .filter(|p| usable(p))
            .or_else(|| {
                [Path::new("/bin/zsh"), Path::new("/bin/bash")].into_iter().find(|p| p.is_file()).map(Into::into)
            })
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
}
