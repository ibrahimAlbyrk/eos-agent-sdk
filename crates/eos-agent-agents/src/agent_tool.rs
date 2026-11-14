//! The Agent tool: starts a subagent through the host's [`Subagents`] port.

use std::sync::Arc;

use eos_agent_core::{
    BoxFuture, PermissionMode, SubagentReport, SubagentRequest, SubagentStart, Subagents, Tool, ToolContext,
    ToolOutput, ToolSpec,
};
use eos_agent_types::{ContentBlock, Usage};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::builtin::{EXPLORE, FORK, PLAN};
use crate::fork::ForkedAgent;
use crate::registry::{AgentDefinitions, normalize};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    description: String,
    prompt: String,
    subagent_type: Option<String>,
    model: Option<Model>,
    run_in_background: Option<bool>,
    name: Option<String>,
    /// Worktrees and remote sessions are the host's to offer; only fork's
    /// refusal of `remote` is checked here.
    isolation: Option<Isolation>,
    effort: Option<Effort>,
    /// Deprecated: a permission mode for the subagent, which only narrows
    /// its parent's (see `eos_agent_core::resolve_mode`).
    mode: Option<String>,
    #[serde(rename = "team_name")]
    _team_name: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Model {
    Sonnet,
    Opus,
    Haiku,
    Fable,
}

impl Model {
    fn alias(self) -> &'static str {
        match self {
            Self::Sonnet => "sonnet",
            Self::Opus => "opus",
            Self::Haiku => "haiku",
            Self::Fable => "fable",
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Isolation {
    Worktree,
    Remote,
}

impl Isolation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Remote => "remote",
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Explore and Plan reports come back without the agentId/usage footer.
const ONE_SHOT: [&str; 2] = [EXPLORE, PLAN];
const NO_OUTPUT: &str = "(Subagent completed but returned no output.)";

/// Claude Code's default for how deep subagents may nest (its
/// `tengu_hazel_trellis`, unset).
pub const DEFAULT_MAX_DEPTH: usize = 3;

/// What Claude Code lets the Agent tool start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnLimits {
    /// Background agents (off with `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`:
    /// every agent runs in the foreground, and the tool says nothing of
    /// backgrounds).
    pub background: bool,
    /// An agent nested this deep may not start another
    /// (`CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`).
    pub max_depth: usize,
}

impl Default for SpawnLimits {
    fn default() -> Self {
        Self { background: true, max_depth: DEFAULT_MAX_DEPTH }
    }
}

impl SpawnLimits {
    /// As Claude Code reads its environment: `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`
    /// set to `1`, `true`, `yes` or `on`; `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`
    /// a whole number of at least 1 (anything else keeps the default).
    pub fn from_env(var: impl Fn(&str) -> Option<String>) -> Self {
        let disabled = var("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS")
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        let depth = var("CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH").and_then(|v| {
            let v = v.trim();
            let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            v.parse::<i64>().ok().filter(|n| *n >= 1).map(|n| usize::try_from(n).unwrap_or(usize::MAX))
        });
        Self { background: !disabled, max_depth: depth.unwrap_or(DEFAULT_MAX_DEPTH) }
    }
}

/// Claude Code's Agent text bullets that hold only while agents may run in
/// the background, and the ones it says instead.
const REPORT_BACKGROUND: &str = "- The agent's final report is not shown to the user — relay what matters.";
const REPORT_FOREGROUND: &str = "- The agent's final message is returned to you as the tool result; it is not shown to the user — relay what matters.";
const BACKGROUND_DEFAULT: &str = "\n- Subagents run in the background by default; you'll be notified when one \
completes. Pass `run_in_background: false` only when your very next action depends on the result and nothing else \
could usefully happen while it runs — otherwise background it so the user can interject. Never fabricate or predict a \
pending agent's results — the notification is never something you write yourself; if the user asks before it \
arrives, say it's still running.";

pub struct AgentTool {
    spec: ToolSpec,
    subagents: Arc<dyn Subagents>,
    definitions: Arc<AgentDefinitions>,
    limits: SpawnLimits,
}

impl std::fmt::Debug for AgentTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentTool").field("definitions", &self.definitions.names()).finish_non_exhaustive()
    }
}

impl AgentTool {
    /// Its description points at the agent listing, which the host sends as a
    /// system note ([`AgentDefinitions::listing`]).
    pub fn new(subagents: Arc<dyn Subagents>, definitions: Arc<AgentDefinitions>) -> Self {
        let spec =
            crate::spec("Agent", include_str!("../prompts/Agent.md"), include_str!("../prompts/Agent.schema.json"))
                .with_search_hint("delegate work to a subagent");
        Self { spec, subagents, definitions, limits: SpawnLimits::default() }
    }

    /// Claude Code's limits instead of its defaults. Without background
    /// agents, its text and schema leave them out.
    #[must_use]
    pub fn with_limits(mut self, limits: SpawnLimits) -> Self {
        self.limits = limits;
        if !limits.background {
            let def: serde_json::Value = serde_json::from_slice(self.spec.json()).unwrap_or_default();
            let description = def["description"]
                .as_str()
                .unwrap_or_default()
                .replace(REPORT_BACKGROUND, REPORT_FOREGROUND)
                .replace(BACKGROUND_DEFAULT, "");
            let mut schema = def["input_schema"].clone();
            if let Some(properties) = schema["properties"].as_object_mut() {
                properties.remove("run_in_background");
            }
            let mut spec = ToolSpec::new("Agent", &description, schema);
            if let Some(hint) = self.spec.search_hint() {
                spec = spec.with_search_hint(hint);
            }
            self.spec = spec;
        }
        self
    }

    async fn run(&self, input: Input, ctx: ToolContext<'_>) -> ToolOutput {
        if let Some(problem) = input.name.as_deref().and_then(name_problem) {
            return crate::refused(format_args!("InputValidationError: {problem}"));
        }
        let depth = self.subagents.depth(ctx.agent_id);
        if depth >= self.limits.max_depth {
            return crate::refused(format!(
                "Subagent nesting limit reached (depth {depth} of {}). Complete this task directly using your tools \
                 instead of spawning another agent. If the user explicitly requested deeper nesting, ask them to raise \
                 CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH.",
                self.limits.max_depth
            ));
        }
        let fork = input.subagent_type.as_deref().is_some_and(|t| normalize(t) == FORK);
        let agent_type = if fork {
            if input.isolation == Some(Isolation::Remote) {
                return crate::refused(
                    "Fork cannot use isolation: \"remote\" — a remote session cannot inherit the conversation \
                     context. Omit isolation (or use \"worktree\"), or spawn a named agent type for remote work.",
                );
            }
            if ctx.extensions.get::<ForkedAgent>().is_some() {
                return crate::refused(
                    "Fork is not available inside a forked worker. Complete your task directly using your tools.",
                );
            }
            None
        } else {
            match self.definitions.resolve(input.subagent_type.as_deref()) {
                Ok(def) => Some(def),
                Err(message) => return crate::refused(with_fork(message)),
            }
        };
        let request = SubagentRequest {
            parent_id: ctx.agent_id.to_owned(),
            tool_use_id: ctx.tool_use_id.to_owned(),
            description: input.description.split_whitespace().collect::<Vec<_>>().join(" "),
            prompt: input.prompt,
            agent_type: Some(agent_type.map_or(FORK, |d| d.name.as_str()).to_owned()),
            model: if fork { None } else { input.model.map(|m| m.alias().to_owned()) },
            effort: input.effort.map(|e| e.as_str().to_owned()),
            // The call's isolation, else a worktree its definition asks for.
            isolation: input.isolation.map(|i| i.as_str().to_owned()).or_else(|| {
                agent_type.and_then(|d| d.isolation.as_deref()).filter(|i| *i == "worktree").map(str::to_owned)
            }),
            mode: input.mode.as_deref().and_then(PermissionMode::parse),
            name: input.name,
            fork,
            // Message clones share their bytes, so the copy is a refcount each.
            history: if fork { ctx.transcript.to_vec() } else { Vec::new() },
            // Subagents run in the background unless the call opts out; a
            // `background: true` definition always does. Without background
            // agents, every one runs in the foreground.
            background: self.limits.background
                && (input.run_in_background != Some(false) || agent_type.is_some_and(|d| d.background)),
            ..SubagentRequest::default()
        };
        match self.subagents.start(request).await {
            Ok(SubagentStart::Finished(report)) => {
                let one_shot = agent_type.is_some_and(|d| ONE_SHOT.contains(&d.name.as_str()));
                finished(&report, one_shot)
            }
            Ok(SubagentStart::Launched { agent_id }) => ToolOutput::text(launched(&agent_id)),
            Err(message) => crate::refused(message),
        }
    }
}

/// Claude Code lists `fork` first among the choices when forking is on.
fn with_fork(message: String) -> String {
    match message.split_once("Available agents: ") {
        Some((head, names)) if head.starts_with("subagent_type is required") => {
            let names = if names == "none" { String::new() } else { format!(", {names}") };
            format!("{head}Available agents: {FORK}{names}")
        }
        _ => message,
    }
}

impl Tool for AgentTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match serde_json::from_str::<Input>(input.get()) {
                Ok(input) => self.run(input, ctx).await,
                Err(e) => crate::invalid_input(&e),
            }
        })
    }
}

/// What is wrong with an agent's `name`: the schema's pattern, and names
/// that already address an agent (`main`, the reserved names, the shape of
/// an agent id), in any spelling.
fn name_problem(name: &str) -> Option<&'static str> {
    let shaped = name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !shaped {
        return Some(
            "name must start with a letter or digit and contain only letters, digits, underscores, or hyphens (max \
             64 chars)",
        );
    }
    let lower = name.to_ascii_lowercase();
    if name == "main" {
        return Some("\"main\" is reserved \u{2014} SendMessage routes it to the main conversation");
    }
    if ["main", "team-lead", "user", "system"].contains(&lower.as_str()) || looks_like_agent_id(&lower) {
        return Some(
            "name must not be a reserved name (\"main\", \"team-lead\", \"user\" or \"system\", in any spelling) or \
             have the shape of an agent id \u{2014} those already address an agent directly",
        );
    }
    None
}

/// `a`, optionally a label and `-`, then 16 hex digits: an agent id.
fn looks_like_agent_id(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('a') else { return false };
    let hex = |s: &str| s.len() == 16 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex(rest) {
        return true;
    }
    rest.len() > 17 && hex(&rest[rest.len() - 16..]) && rest[..rest.len() - 16].ends_with('-') && rest.len() - 17 <= 63
}

/// Every token the subagent's usage counts.
pub(crate) fn total_tokens(usage: &Usage) -> u64 {
    [usage.input_tokens, usage.output_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens]
        .into_iter()
        .flatten()
        .sum()
}

fn finished(report: &SubagentReport, one_shot: bool) -> ToolOutput {
    let text = if report.text.is_empty() { NO_OUTPUT } else { report.text.as_str() };
    let content = ContentBlock::Text { text: text.to_owned(), citations: None };
    if one_shot {
        return ToolOutput::blocks(vec![content]);
    }
    let id = &report.agent_id;
    let worktree = report
        .worktree
        .as_ref()
        .map(|w| format!(" worktreePath: {} worktreeBranch: {}", w.path.display(), w.branch))
        .unwrap_or_default();
    let footer = format!(
        "agentId: {id}{worktree} (use SendMessage with to: '{id}', summary: '<5-10 word recap>' to continue this agent)\n\
         <usage>subagent_tokens: {}\ntool_uses: {}\nduration_ms: {}</usage>",
        total_tokens(&report.usage),
        report.tool_uses,
        report.duration_ms
    );
    ToolOutput::blocks(vec![content, ContentBlock::Text { text: footer, citations: None }])
}

fn launched(id: &str) -> String {
    format!(
        "Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of \
         it, including the agentId below, into a user-facing reply.)\n\
         agentId: {id} (internal ID - do not mention to user. Use SendMessage with to: '{id}', summary: '<5-10 word \
         recap>' to continue this agent.)\n\
         The agent is working in the background. You will be notified automatically when it completes. You know \
         nothing about its results until that notification arrives — do not report, assume, or predict them; continue \
         other work or respond to the user in the meantime.\n\
         In your own words, briefly tell the user what you launched — do not echo this tool result. Agent results will \
         arrive in a subsequent message. If the user asks for progress, say the agent is still running."
    )
}
