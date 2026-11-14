//! Model aliases, the facts the session context states about a model, and
//! the request settings Claude Code uses for it: output limit, context
//! window, effort and thinking mode, each only where the model takes it.

use eos_agent_core::Thinking;
use eos_agent_prompt::ModelInfo;

use crate::profile::Provider;

/// Capabilities from Claude Code's model table that change a request.
const EFFORT: u16 = 1;
const MAX_EFFORT: u16 = 1 << 1;
const XHIGH_EFFORT: u16 = 1 << 2;
const ADAPTIVE_THINKING: u16 = 1 << 3;
/// Claude Code's guidance on finishing a turn's work (and its classic tool
/// texts) instead of the games paragraph.
const EARLY_STOPPING: u16 = 1 << 4;
/// Claude Code's lean system prompt (`lean_prompt`); the others get its
/// full one.
const LEAN: u16 = 1 << 5;
/// Takes tool changes inside the conversation (`mid_conv_tool_change`).
const TOOL_CHANGES: u16 = 1 << 6;
/// Refuses `thinking: {type: "disabled"}`: the model always thinks
/// (`rejects_disabled_thinking`).
const ALWAYS_THINKS: u16 = 1 << 7;
/// Takes `disabled` only at effort `high` or below
/// (`thinking_disabled_effort_cap`).
const NO_THINKING_UP_TO_HIGH: u16 = 1 << 8;

const FULL_EFFORT: u16 = EFFORT | MAX_EFFORT | XHIGH_EFFORT | ADAPTIVE_THINKING;
const NO_XHIGH: u16 = EFFORT | MAX_EFFORT | ADAPTIVE_THINKING;

/// One model as Claude Code 2.1.319's model table describes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Model {
    /// Claude Code's alias, on the latest model of each family only.
    alias: Option<&'static str>,
    pub(crate) id: &'static str,
    /// The first-party API name, when it is not `id` (dated older models).
    api_id: Option<&'static str>,
    name: &'static str,
    cutoff: Option<&'static str>,
    pub(crate) context_window: u64,
    /// The `max_tokens` Claude Code sends.
    pub(crate) max_output_tokens: u32,
    /// The most the model may be asked for (bounds a thinking budget).
    upper_output_tokens: u32,
    pub(crate) default_effort: Option<&'static str>,
    caps: u16,
}

impl Model {
    fn has(&self, cap: u16) -> bool {
        self.caps & cap == cap
    }

    pub(crate) fn early_stopping_guidance(&self) -> bool {
        self.has(EARLY_STOPPING)
    }

    pub(crate) fn lean_prompt(&self) -> bool {
        self.has(LEAN)
    }
}

/// A table row: `(id, API name)`, `(name, cutoff)`, `(context window,
/// default output, upper output)`, default effort, capabilities.
const fn m(
    (id, api_id): (&'static str, Option<&'static str>),
    (name, cutoff): (&'static str, Option<&'static str>),
    (context_window, max_output_tokens, upper_output_tokens): (u64, u32, u32),
    default_effort: Option<&'static str>,
    caps: u16,
) -> Model {
    Model {
        alias: None,
        id,
        api_id,
        name,
        cutoff,
        context_window,
        max_output_tokens,
        upper_output_tokens,
        default_effort,
        caps,
    }
}

const fn alias(alias: &'static str, model: Model) -> Model {
    Model { alias: Some(alias), ..model }
}

const K200: u64 = 200_000;
const M1: u64 = 1_000_000;
const JUN26: Option<&str> = Some("June 2026");
const MAY26: Option<&str> = Some("May 2026");
const JAN26: Option<&str> = Some("January 2026");
const MAY25: Option<&str> = Some("May 2025");
const JAN25: Option<&str> = Some("January 2025");

/// Claude Code's model table (first-party facts only).
const KNOWN: &[Model] = &[
    m(("claude-3-5-haiku", Some("claude-3-5-haiku-20241022")), ("Haiku 3.5", None), (K200, 8_192, 8_192), None, 0),
    m(
        ("claude-haiku-4-5", Some("claude-haiku-4-5-20251001")),
        ("Haiku 4.5", Some("February 2025")),
        (K200, 32_000, 64_000),
        None,
        0,
    ),
    alias(
        "haiku",
        m(
            ("claude-haiku-5-5", None),
            ("Haiku 5.5", JUN26),
            (M1, 128_000, 128_000),
            Some("medium"),
            FULL_EFFORT | LEAN | EARLY_STOPPING | TOOL_CHANGES | ALWAYS_THINKS,
        ),
    ),
    m(("claude-3-5-sonnet", Some("claude-3-5-sonnet-20241022")), ("Sonnet 3.5", None), (K200, 8_192, 8_192), None, 0),
    m(("claude-3-7-sonnet", Some("claude-3-7-sonnet-20250219")), ("Sonnet 3.7", None), (K200, 32_000, 64_000), None, 0),
    m(("claude-sonnet-4-0", Some("claude-sonnet-4-20250514")), ("Sonnet 4", JAN25), (K200, 32_000, 64_000), None, 0),
    m(
        ("claude-sonnet-4-5", Some("claude-sonnet-4-5-20250929")),
        ("Sonnet 4.5", JAN25),
        (K200, 32_000, 64_000),
        None,
        0,
    ),
    m(("claude-sonnet-4-6", None), ("Sonnet 4.6", Some("August 2025")), (K200, 32_000, 128_000), None, NO_XHIGH),
    m(("claude-sonnet-5", None), ("Sonnet 5", JAN26), (M1, 64_000, 128_000), Some("high"), FULL_EFFORT),
    alias(
        "sonnet",
        m(
            ("claude-sonnet-5-5", None),
            ("Sonnet 5.5", JUN26),
            (M1, 128_000, 128_000),
            Some("medium"),
            FULL_EFFORT | LEAN | TOOL_CHANGES | ALWAYS_THINKS,
        ),
    ),
    m(("claude-opus-4-0", Some("claude-opus-4-20250514")), ("Opus 4", JAN25), (K200, 32_000, 32_000), None, 0),
    m(("claude-opus-4-1", Some("claude-opus-4-1-20250805")), ("Opus 4.1", JAN25), (K200, 32_000, 32_000), None, 0),
    m(("claude-opus-4-5", Some("claude-opus-4-5-20251101")), ("Opus 4.5", MAY25), (K200, 32_000, 64_000), None, 0),
    m(("claude-opus-4-6", None), ("Opus 4.6", MAY25), (K200, 64_000, 128_000), None, NO_XHIGH),
    m(("claude-opus-4-7", None), ("Opus 4.7", JAN26), (M1, 64_000, 128_000), Some("xhigh"), FULL_EFFORT),
    m(
        ("claude-opus-4-8", None),
        ("Opus 4.8", JAN26),
        (M1, 64_000, 128_000),
        Some("high"),
        FULL_EFFORT | LEAN | TOOL_CHANGES,
    ),
    m(
        ("claude-opus-5", None),
        ("Opus 5", MAY26),
        (M1, 64_000, 128_000),
        Some("high"),
        FULL_EFFORT | LEAN | TOOL_CHANGES | NO_THINKING_UP_TO_HIGH,
    ),
    alias(
        "opus",
        m(
            ("claude-opus-5-5", None),
            ("Opus 5.5", JUN26),
            (M1, 128_000, 128_000),
            Some("medium"),
            FULL_EFFORT | LEAN | TOOL_CHANGES | ALWAYS_THINKS,
        ),
    ),
    m(
        ("claude-fable-5", None),
        ("Fable 5", JAN26),
        (M1, 64_000, 128_000),
        Some("high"),
        FULL_EFFORT | LEAN | TOOL_CHANGES | ALWAYS_THINKS,
    ),
    alias(
        "fable",
        m(
            ("claude-fable-5-1", None),
            ("Fable 5.1", JUN26),
            (M1, 64_000, 128_000),
            Some("high"),
            FULL_EFFORT | LEAN | TOOL_CHANGES | ALWAYS_THINKS,
        ),
    ),
    // Claude Code lets Mythos 5 take effort and adaptive thinking by name,
    // and takes it to always think as it takes every model its list of
    // models that turn thinking off leaves out.
    m(("claude-mythos-5", None), ("Mythos 5", JAN26), (M1, 64_000, 128_000), None, FULL_EFFORT | ALWAYS_THINKS),
    m(
        ("claude-mythos-5-1", None),
        ("Mythos 5.1", JUN26),
        (M1, 64_000, 128_000),
        Some("high"),
        FULL_EFFORT | LEAN | TOOL_CHANGES | ALWAYS_THINKS,
    ),
];

/// For a model Claude Code does not know.
pub(crate) const FALLBACK_MAX_TOKENS: u32 = 32_000;
pub(crate) const FALLBACK_CONTEXT_WINDOW: u64 = 200_000;
/// Claude Code's smallest thinking budget.
const MIN_THINKING_BUDGET: u32 = 1_024;
/// Effort levels, lowest first.
const LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// What `gpt` means before the account's catalog has been read.
pub(crate) const GPT_FALLBACK: &str = "gpt-5.5";
/// Never sent (a ChatGPT plan takes no output limit); it sizes the reply
/// reserve when deciding to summarize.
const GPT_MAX_TOKENS: u32 = 128_000;

/// `max_tokens` and the context window of an OpenAI model, from the catalog.
pub(crate) fn openai_limits(catalog: &eos_agent_openai::Catalog, id: &str) -> (u32, u64) {
    let window = catalog.get(eos_agent_openai::models::slug(id)).and_then(|m| m.context_window);
    (GPT_MAX_TOKENS, window.unwrap_or(eos_agent_openai::models::DEFAULT_CONTEXT_WINDOW))
}

pub(crate) fn openai_info(catalog: &eos_agent_openai::Catalog, id: &str) -> ModelInfo {
    let model = catalog.get(eos_agent_openai::models::slug(id));
    ModelInfo { id: id.to_owned(), marketing_name: model.and_then(|m| m.display_name.clone()), knowledge_cutoff: None }
}

/// The model id for an alias (`opus`, `sonnet`, `haiku`, `fable`); other names pass through.
pub fn resolve_model(name: &str) -> String {
    KNOWN.iter().find(|m| m.alias == Some(name)).map_or_else(|| name.to_owned(), |m| m.id.to_owned())
}

/// The table entry for a model id: its own id, its API name, or either with
/// a date (`-20251001`) or a `[1m]` suffix.
pub(crate) fn known(id: &str) -> Option<&'static Model> {
    let id = id.split('[').next().unwrap_or(id);
    let names = || KNOWN.iter().flat_map(|m| [Some(m.id), m.api_id].into_iter().flatten().map(move |n| (n, m)));
    names()
        .find(|(n, _)| *n == id)
        .or_else(|| {
            names()
                .filter(|(n, _)| id.strip_prefix(n).and_then(|r| r.strip_prefix('-')).is_some_and(is_date))
                .max_by_key(|(n, _)| n.len())
        })
        .map(|(_, m)| m)
}

/// Claude Code's `u7`: whether `id` takes mid-conversation `role: "system"`
/// messages. Claude 3 models and those before Opus 4.8 in its release order
/// do not (they get the notes as `<system-reminder>` text);
/// `CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM` sends them anyway.
pub(crate) fn takes_system_turns(id: &str) -> bool {
    const BEFORE_OPUS_4_8: &[&str] = &[
        "claude-opus-4-0",
        "claude-sonnet-4-0",
        "claude-opus-4-1",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-5",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-opus-4-7",
    ];
    if std::env::var_os("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    let id = known(id).map_or(id, |m| m.id);
    !(id.contains("claude-3-") || BEFORE_OPUS_4_8.contains(&id))
}

/// Claude Code's `NAe`: whether `id` takes tool changes inside the
/// conversation: a model with system turns whose table entry says so, Mythos
/// 5 (by name), or one the table does not know;
/// `CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM` turns it on for any.
pub(crate) fn takes_tool_changes(id: &str) -> bool {
    if std::env::var_os("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    takes_system_turns(id) && known(id).is_none_or(|m| m.has(TOOL_CHANGES) || m.id == "claude-mythos-5")
}

/// Claude Code's `kPe`: whether the system prompt tells `id` about
/// mid-conversation system turns (else it names `<system-reminder>` tags):
/// models that take them, except Sonnet 5 and Opus 4.8.
pub(crate) fn explains_system_turns(id: &str) -> bool {
    let model = known(id).map_or(id, |m| m.id);
    takes_system_turns(id) && model != "claude-sonnet-5" && model != "claude-opus-4-8"
}

fn is_date(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit())
}

pub(crate) fn model_info(id: &str) -> ModelInfo {
    let known = known(id);
    ModelInfo {
        id: id.to_owned(),
        marketing_name: known.map(|m| m.name.to_owned()),
        knowledge_cutoff: known.and_then(|m| m.cutoff.map(str::to_owned)),
    }
}

/// `max_tokens` and the context window Claude Code uses for `id`.
pub(crate) fn limits(id: &str) -> (u32, u64) {
    known(id).map_or((FALLBACK_MAX_TOKENS, FALLBACK_CONTEXT_WINDOW), |m| (m.max_output_tokens, m.context_window))
}

/// The `output_config.effort` to send `id`: `wanted` (else the model's
/// default, else `high`) as Claude Code adjusts it: nothing for a model without effort,
/// `max` and `xhigh` lowered to `high` where the model lacks them, a numeric
/// level read as `high`. A model not in the table takes what it is given.
pub(crate) fn effort_for(id: &str, wanted: Option<&str>) -> Option<String> {
    let Some(model) = known(id) else { return wanted.map(str::to_owned) };
    if !model.has(EFFORT) {
        return None;
    }
    let level = wanted.or(model.default_effort).unwrap_or("high");
    let level = if LEVELS.contains(&level) { level } else { "high" };
    let level = match level {
        "max" if !model.has(MAX_EFFORT) => "high",
        "xhigh" if !model.has(XHIGH_EFFORT) => "high",
        other => other,
    };
    Some(level.to_owned())
}

/// The thinking mode to send `id` for an agent that thinks (`summarized` =
/// stream readable summaries): adaptive where the model has it, a fixed
/// budget on older models (Claude Code's: the model's upper limit less one,
/// within `max_tokens`), none on Claude 3 models.
pub(crate) fn thinking_for(id: &str, summarized: bool, max_tokens: u32) -> Option<Thinking> {
    let Some(model) = known(id) else { return Some(Thinking::Adaptive { summarized }) };
    if model.has(ADAPTIVE_THINKING) {
        return Some(Thinking::Adaptive { summarized });
    }
    if model.id.starts_with("claude-3-") {
        return None;
    }
    let budget = (model.upper_output_tokens - 1).min(max_tokens.saturating_sub(1)).max(MIN_THINKING_BUDGET);
    Some(Thinking::Enabled { budget_tokens: budget, summarized })
}

/// How a request meant to run without thinking (a one-shot answer, a side
/// call) goes to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThinkingOff {
    pub(crate) thinking: Option<Thinking>,
    pub(crate) effort: Option<String>,
    /// The model thinks anyway, so no tool call may be forced (Claude Code
    /// turns a forced one into `auto`).
    pub(crate) thinks: bool,
}

/// Claude Code's rule for a request with thinking off: `disabled` where the
/// model takes it (an effort above `high` lowered to `high` where it takes
/// `disabled` only up to there); no thinking setting where the model always
/// thinks (it then thinks as it decides) or never does (Claude 3). A Claude
/// model the table does not know is taken to always think, as Claude Code
/// takes it on the first-party API. OpenAI requests keep `disabled`, which
/// their translation reads as the lowest reasoning.
pub(crate) fn thinking_off(id: &str, effort: Option<String>) -> ThinkingOff {
    let model = match known(id) {
        _ if Provider::of(id) == Provider::OpenAi => {
            return ThinkingOff { thinking: Some(Thinking::Disabled), effort, thinks: false };
        }
        Some(model) if !model.has(ALWAYS_THINKS) => model,
        _ => return ThinkingOff { thinking: None, effort, thinks: true },
    };
    if model.id.starts_with("claude-3-") {
        return ThinkingOff { thinking: None, effort, thinks: false };
    }
    let capped = model.has(NO_THINKING_UP_TO_HIGH) && effort.as_deref().is_some_and(|e| matches!(e, "xhigh" | "max"));
    let effort = if capped { Some("high".to_owned()) } else { effort };
    ThinkingOff { thinking: Some(Thinking::Disabled), effort, thinks: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude Code's `NAe` over its model table (no forcing variable here).
    #[test]
    fn tool_changes_by_model() {
        for yes in ["claude-opus-5-5", "claude-haiku-5-5", "claude-opus-4-8", "claude-mythos-5", "claude-next-1"] {
            assert!(takes_tool_changes(yes), "{yes}");
        }
        for no in ["claude-sonnet-5", "claude-opus-4-7", "claude-haiku-4-5", "claude-3-7-sonnet"] {
            assert!(!takes_tool_changes(no), "{no}");
        }
    }

    #[test]
    fn system_turns_by_model() {
        for old in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-opus-4-7",
            "claude-3-5-haiku",
        ] {
            assert!(!takes_system_turns(old), "{old}");
        }
        for new in ["claude-opus-4-8", "claude-sonnet-5", "claude-haiku-5-5", "claude-opus-5-5", "claude-next-9"] {
            assert!(takes_system_turns(new), "{new}");
        }
    }

    #[test]
    fn which_models_are_told_about_system_turns() {
        for told in ["claude-mythos-5", "claude-opus-5-5", "claude-haiku-5-5", "claude-mythos-5-1"] {
            assert!(explains_system_turns(told), "{told}");
        }
        for not in ["claude-sonnet-5", "claude-sonnet-5-20260101", "claude-opus-4-8", "claude-haiku-4-5"] {
            assert!(!explains_system_turns(not), "{not}");
        }
    }

    #[test]
    fn aliases_and_facts() {
        assert_eq!(resolve_model("haiku"), "claude-haiku-5-5");
        assert_eq!(resolve_model("claude-x"), "claude-x");
        let info = model_info("claude-opus-5-5");
        assert_eq!(info.marketing_name.as_deref(), Some("Opus 5.5"));
        assert!(model_info("other").marketing_name.is_none());
        assert_eq!(known("claude-fable-5-1").and_then(|m| m.default_effort), Some("high"));
    }

    #[test]
    fn dated_and_suffixed_names() {
        assert_eq!(known("claude-haiku-4-5-20251001").map(|m| m.id), Some("claude-haiku-4-5"));
        assert_eq!(known("claude-sonnet-4-20250514").map(|m| m.id), Some("claude-sonnet-4-0"));
        assert_eq!(known("claude-opus-5-5[1m]").map(|m| m.id), Some("claude-opus-5-5"));
        assert_eq!(known("claude-opus-5-20260101").map(|m| m.id), Some("claude-opus-5"));
        assert!(known("claude-opus-5-5-x").is_none());
    }

    #[test]
    fn limits_per_model() {
        assert_eq!(limits("claude-haiku-5-5"), (128_000, 1_000_000));
        assert_eq!(limits("claude-haiku-4-5"), (32_000, 200_000));
        assert_eq!(limits("claude-fable-5-1"), (64_000, 1_000_000));
        assert_eq!(limits("someone-elses-model"), (FALLBACK_MAX_TOKENS, FALLBACK_CONTEXT_WINDOW));
    }

    #[test]
    fn effort_only_where_supported() {
        assert_eq!(effort_for("claude-haiku-5-5", None).as_deref(), Some("medium"));
        assert_eq!(effort_for("claude-haiku-5-5", Some("max")).as_deref(), Some("max"));
        assert_eq!(effort_for("claude-haiku-4-5", Some("high")), None);
        assert_eq!(effort_for("claude-3-5-haiku", None), None);
        assert_eq!(effort_for("claude-opus-4-6", Some("xhigh")).as_deref(), Some("high"));
        assert_eq!(effort_for("claude-opus-4-6", Some("max")).as_deref(), Some("max"));
        assert_eq!(effort_for("claude-opus-4-6", None).as_deref(), Some("high"), "no default level of its own");
        assert_eq!(effort_for("claude-opus-5-5", Some("12000")).as_deref(), Some("high"));
        assert_eq!(effort_for("custom", Some("low")).as_deref(), Some("low"));
        assert_eq!(effort_for("custom", None), None);
    }

    #[test]
    fn thinking_modes() {
        let adaptive = Some(Thinking::Adaptive { summarized: false });
        assert_eq!(thinking_for("claude-haiku-5-5", false, 128_000), adaptive);
        assert_eq!(
            thinking_for("claude-haiku-4-5", true, 32_000),
            Some(Thinking::Enabled { budget_tokens: 31_999, summarized: true })
        );
        assert_eq!(
            thinking_for("claude-opus-4-1", true, 500),
            Some(Thinking::Enabled { budget_tokens: 1_024, summarized: true })
        );
        assert_eq!(thinking_for("claude-3-5-haiku", true, 8_192), None);
        assert_eq!(thinking_for("custom", false, 1), adaptive);
    }

    #[test]
    fn thinking_off_as_each_model_takes_it() {
        let off = |id: &str, effort: Option<&str>| thinking_off(id, effort.map(str::to_owned));
        let disabled = |effort: Option<&str>| ThinkingOff {
            thinking: Some(Thinking::Disabled),
            effort: effort.map(str::to_owned),
            thinks: false,
        };
        let thinks =
            |effort: Option<&str>| ThinkingOff { thinking: None, effort: effort.map(str::to_owned), thinks: true };
        for always in [
            "claude-haiku-5-5",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-mythos-5",
            "claude-mythos-5-1",
            "claude-fable-5-2",
            "claude-next-1",
        ] {
            assert_eq!(off(always, Some("low")), thinks(Some("low")), "{always}");
        }
        for takes_it in ["claude-sonnet-5", "claude-opus-4-8", "claude-sonnet-4-6", "claude-haiku-4-5-20251001"] {
            assert_eq!(off(takes_it, Some("max")), disabled(Some("max")), "{takes_it}");
        }
        assert_eq!(off("claude-opus-5", Some("max")), disabled(Some("high")), "up to high only");
        assert_eq!(off("claude-opus-5", Some("medium")), disabled(Some("medium")));
        assert_eq!(off("claude-opus-5", None), disabled(None));
        let never = ThinkingOff { thinking: None, effort: None, thinks: false };
        assert_eq!(off("claude-3-5-haiku", None), never);
        assert_eq!(off("gpt-5.5", Some("low")), disabled(Some("low")));
    }
}
