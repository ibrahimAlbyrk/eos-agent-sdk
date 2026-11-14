//! What a host decides about workflows: where run files go, the size
//! guideline, the limits, the engine, the token budget.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::engine::{QuickJs, SLICE_LIMIT, ScriptEngine};

/// Claude Code's run-wide cap on `agent()` calls, children included.
pub const MAX_AGENTS: u32 = 1000;
/// Claude Code's default stall window for a workflow agent.
pub const STALL: Duration = Duration::from_secs(600);

/// Claude Code's "Dynamic workflow size" setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeGuideline {
    Unrestricted,
    Small,
    Medium,
    Large,
}

impl SizeGuideline {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "unrestricted" => Some(Self::Unrestricted),
            "small" => Some(Self::Small),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Small => "small",
            Self::Medium => "medium",
            Self::Large => "large",
        }
    }

    fn limit(self) -> Option<u32> {
        match self {
            Self::Small => Some(5),
            Self::Medium => Some(10),
            Self::Large => Some(50),
            Self::Unrestricted => None,
        }
    }

    fn phrase(self) -> String {
        match self.limit() {
            Some(n) => format!("{} \u{2014} keep workflows under {n} agents", self.as_str()),
            None => self.as_str().to_owned(),
        }
    }

    /// The sentence the tool description ends with (Claude Code's `wzi`),
    /// `None` when unrestricted.
    pub fn description(self, is_default: bool) -> Option<String> {
        if self == Self::Unrestricted {
            return None;
        }
        let head = if is_default {
            "This session has the default workflow size guideline:"
        } else {
            "A workflow size guideline is configured for this session:"
        };
        let tail =
            if is_default { " The user can raise or remove it with \"Dynamic workflow size\" in /config." } else { "" };
        Some(format!("{head} {}. {GUIDELINE_NOTE}{tail}", self.phrase()))
    }

    /// The reminder when the guideline changed mid-session (Claude Code's `Ezi`).
    pub fn change_reminder(self) -> String {
        if self == Self::Unrestricted {
            return "Workflow size is now unrestricted \u{2014} no size guideline applies.".into();
        }
        format!("The workflow size guideline for this session changed: {}. {GUIDELINE_NOTE}", self.phrase())
    }
}

const GUIDELINE_NOTE: &str =
    "This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different scale.";

/// The turn's token target and what was spent toward it (`budget` in a
/// script). Claude Code shares one pool between the main loop and every
/// workflow of the session.
pub trait TokenBudgets: Send + Sync + std::fmt::Debug {
    /// The target set for `session`'s current turn, if any.
    fn total(&self, session: &str) -> Option<u64>;

    /// Output tokens spent in `session`'s current turn so far, every agent
    /// of the session included.
    fn spent(&self, session: &str) -> u64;
}

#[derive(Debug, Clone)]
pub struct WorkflowConfig {
    /// Run files go under `<root>/<session id>/` in Claude Code's layout:
    /// `workflows/scripts/<name>-<runId>.js`, `workflows/<runId>.json`,
    /// `subagents/workflows/<runId>/journal.jsonl`, `tasks/<taskId>.output`.
    pub root: PathBuf,
    /// Claude Code's config directory (`~/.claude`), for saved workflows in
    /// its `workflows/`.
    pub user_dir: Option<PathBuf>,
    pub size: SizeGuideline,
    /// The guideline is the default one (not set by the user).
    pub size_is_default: bool,
    /// The `workflow-authoring` skill is available: the description points
    /// at it instead of carrying the whole reference.
    pub authoring_skill: bool,
    /// Agents of one run working at once.
    pub max_concurrent: usize,
    pub max_agents: u32,
    pub slice_limit: Duration,
    /// An agent with no progress for this long is stopped and tried again.
    pub stall: Duration,
    pub engine: Arc<dyn ScriptEngine>,
    pub budgets: Option<Arc<dyn TokenBudgets>>,
}

impl WorkflowConfig {
    /// Claude Code's defaults (its environment overrides included).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
        let env_num = |k: &str| std::env::var(k).ok().and_then(|v| v.trim().parse::<u64>().ok()).filter(|&n| n > 0);
        Self {
            root: root.into(),
            user_dir: None,
            size: SizeGuideline::Medium,
            size_is_default: true,
            authoring_skill: true,
            max_concurrent: env_num("CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS")
                .map_or_else(|| default_concurrency(cpus), |n| usize::try_from(n).unwrap_or(usize::MAX)),
            max_agents: MAX_AGENTS,
            slice_limit: SLICE_LIMIT,
            stall: env_num("CLAUDE_ASYNC_AGENT_STALL_TIMEOUT_MS").map_or(STALL, Duration::from_millis),
            engine: Arc::new(QuickJs),
            budgets: None,
        }
    }
}

/// Claude Code's `min(16, max(2, cpus - 2))`.
pub fn default_concurrency(cpus: usize) -> usize {
    cpus.saturating_sub(2).clamp(2, 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guideline_texts() {
        assert_eq!(
            SizeGuideline::Medium.description(true).as_deref(),
            Some(
                "This session has the default workflow size guideline: medium \u{2014} keep workflows under 10 agents. \
                 This is a guideline, not a hard limit \u{2014} follow it unless the user's prompt calls for a different \
                 scale. The user can raise or remove it with \"Dynamic workflow size\" in /config."
            )
        );
        assert_eq!(SizeGuideline::Unrestricted.description(false), None);
        assert!(
            SizeGuideline::Large
                .description(false)
                .expect("text")
                .starts_with("A workflow size guideline is configured")
        );
        assert_eq!(default_concurrency(1), 2);
        assert_eq!(default_concurrency(10), 8);
        assert_eq!(default_concurrency(64), 16);
    }
}
