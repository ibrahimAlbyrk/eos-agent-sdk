//! Claude Code's Workflow tool in the SDK: whether it is on (its settings
//! and environment), the size guideline, the turn's token budget ("+500k"),
//! the `workflow-authoring` skill and the "ultracode" keyword.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::BoxFuture;
use eos_agent_skills::{BundledSkill, SkillRenderer};
use eos_agent_workflow::{AUTHORING, SKILL, SKILL_DESCRIPTION, SizeGuideline, TokenBudgets};
use serde_json::Value;

/// What the model is told when the user typed "ultracode".
pub(crate) const KEYWORD_REMINDER: &str = "The user included the keyword \"ultracode\", opting this turn into \
multi-agent orchestration \u{2014} use the Workflow tool to fulfill the request.";

/// The workflow settings Claude Code reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowSettings {
    pub(crate) enabled: bool,
    pub(crate) size: SizeGuideline,
    pub(crate) size_is_default: bool,
    pub(crate) keyword_trigger: bool,
}

impl WorkflowSettings {
    /// Claude Code's gate: `CLAUDE_CODE_DISABLE_WORKFLOWS`, then
    /// `CLAUDE_CODE_WORKFLOWS`, then the `disableWorkflows` /
    /// `enableWorkflows` settings (later layers win), on by default.
    pub(crate) fn read(layers: &[Value], env: impl Fn(&str) -> Option<String>) -> Self {
        let truthy = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        let falsy = |v: &str| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off");
        let setting = |key: &str| layers.iter().rev().find_map(|doc| doc.get(key).cloned());
        let mut enabled = setting("enableWorkflows").and_then(|v| v.as_bool()).unwrap_or(true);
        if setting("disableWorkflows").and_then(|v| v.as_bool()) == Some(true) {
            enabled = false;
        }
        if env("CLAUDE_CODE_WORKFLOWS").is_some_and(|v| falsy(&v)) {
            enabled = false;
        }
        if env("CLAUDE_CODE_DISABLE_WORKFLOWS").is_some_and(|v| truthy(&v)) {
            enabled = false;
        }
        let configured = setting("workflowSizeGuideline").and_then(|v| v.as_str().and_then(SizeGuideline::parse));
        Self {
            enabled,
            size: configured.unwrap_or(SizeGuideline::Medium),
            size_is_default: configured.is_none(),
            keyword_trigger: setting("workflowKeywordTriggerEnabled").and_then(|v| v.as_bool()).unwrap_or(true),
        }
    }
}

/// The `workflow-authoring` bundled skill.
pub(crate) fn authoring_skill() -> BundledSkill {
    BundledSkill {
        name: SKILL.to_owned(),
        description: SKILL_DESCRIPTION.to_owned(),
        user_invocable: true,
        renderer: Arc::new(Authoring),
    }
}

#[derive(Debug)]
struct Authoring;

impl SkillRenderer for Authoring {
    fn render<'a>(&'a self, _args: &'a str, _call: Option<eos_agent_core::ToolContext<'a>>) -> BoxFuture<'a, String> {
        Box::pin(async { AUTHORING.to_owned() })
    }
}

/// The turn's token target from a "+500k"-style directive in what the user
/// typed: a word `+<number>k` or `+<number>m` (the last one wins).
pub(crate) fn token_target(text: &str) -> Option<u64> {
    text.split_whitespace().rev().find_map(|word| {
        let word = word.trim_end_matches(['.', ',', ';', '!', '?']);
        let rest = word.strip_prefix('+')?;
        let (number, scale) = match rest.char_indices().last()? {
            (i, 'k' | 'K') => (&rest[..i], 1_000.0),
            (i, 'm' | 'M') => (&rest[..i], 1_000_000.0),
            _ => return None,
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b'.' || b == b'_' || b == b',') {
            return None;
        }
        let n: f64 = number.replace(['_', ','], "").parse().ok()?;
        let tokens = n * scale;
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "a positive token count")]
        (tokens >= 1.0 && tokens.is_finite()).then_some(tokens as u64)
    })
}

/// Claude Code's keyword match (`Fi`): a whole word, case-insensitive, not
/// in a quote, code span, bracket or tag, not part of a path or flag; never
/// in a slash command.
pub(crate) fn has_keyword(text: &str, word: &str) -> bool {
    if !text.to_lowercase().contains(word) || text.starts_with('/') {
        return false;
    }
    let chars: Vec<char> = text.chars().collect();
    let wordish = |c: Option<&char>| c.is_some_and(|c| c.is_alphanumeric() || *c == '_');
    let closer = |c: char| match c {
        '`' => Some('`'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '[' => Some(']'),
        '(' => Some(')'),
        '{' => Some('}'),
        '<' => Some('>'),
        _ => None,
    };
    // Spans to ignore.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<(char, usize)> = None;
    for (i, &c) in chars.iter().enumerate() {
        match open {
            Some((o, start)) => {
                if Some(c) == closer(o) && !(o == '\'' && wordish(chars.get(i + 1))) {
                    spans.push((start, i + 1));
                    open = None;
                }
            }
            None => {
                let opens = match c {
                    '<' => chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == '/'),
                    '\'' => !wordish(if i == 0 { None } else { chars.get(i - 1) }),
                    c => closer(c).is_some() && c != '<',
                };
                if opens {
                    open = Some((c, i));
                }
            }
        }
    }
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let target: Vec<char> = word.chars().collect();
    (0..lower.len().saturating_sub(target.len() - 1)).any(|i| {
        let end = i + target.len();
        if lower.len() != chars.len() || lower[i..end] != target[..] {
            return false;
        }
        let before = if i == 0 { None } else { chars.get(i - 1) };
        let after = chars.get(end);
        if wordish(before) || wordish(after) || spans.iter().any(|&(s, e)| i >= s && i < e) {
            return false;
        }
        if before.is_some_and(|c| matches!(c, '/' | '\\' | '-'))
            || after.is_some_and(|c| matches!(c, '/' | '\\' | '-' | '?'))
        {
            return false;
        }
        !(after == Some(&'.') && wordish(chars.get(end + 1)))
    })
}

/// Each top-level agent's turn: its target and the output tokens its
/// subagents spent toward it.
#[derive(Debug, Default)]
pub(crate) struct Budgets {
    turns: Mutex<HashMap<String, (Option<u64>, u64)>>,
}

impl Budgets {
    pub(crate) fn start_turn(&self, session: &str, target: Option<u64>) {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner).insert(session.to_owned(), (target, 0));
    }

    pub(crate) fn spend(&self, session: &str, tokens: u64) {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        turns.entry(session.to_owned()).or_default().1 += tokens;
    }

    pub(crate) fn forget(&self, session: &str) {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner).remove(session);
    }
}

impl TokenBudgets for Budgets {
    fn total(&self, session: &str) -> Option<u64> {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner).get(session).and_then(|t| t.0)
    }

    fn spent(&self, session: &str) -> u64 {
        self.turns.lock().unwrap_or_else(PoisonError::into_inner).get(session).map_or(0, |t| t.1)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn gate_and_guideline() {
        let none = |_: &str| None;
        assert!(WorkflowSettings::read(&[], none).enabled);
        assert!(!WorkflowSettings::read(&[json!({"disableWorkflows": true})], none).enabled);
        assert!(!WorkflowSettings::read(&[json!({"enableWorkflows": false})], none).enabled);
        assert!(!WorkflowSettings::read(&[], |k| (k == "CLAUDE_CODE_DISABLE_WORKFLOWS").then(|| "1".into())).enabled);
        assert!(!WorkflowSettings::read(&[], |k| (k == "CLAUDE_CODE_WORKFLOWS").then(|| "false".into())).enabled);
        let s = WorkflowSettings::read(&[json!({"workflowSizeGuideline": "large"})], none);
        assert_eq!((s.size, s.size_is_default), (SizeGuideline::Large, false));
    }

    #[test]
    fn token_targets() {
        assert_eq!(token_target("audit everything +500k"), Some(500_000));
        assert_eq!(token_target("+1.5m go"), Some(1_500_000));
        assert_eq!(token_target("+2k then +3k."), Some(3_000));
        assert_eq!(token_target("1+500k or +k or +5"), None);
    }

    #[test]
    fn keyword_rules() {
        assert!(has_keyword("ultracode: review this repo", "ultracode"));
        assert!(has_keyword("Please use ULTRACODE for it", "ultracode"));
        for text in [
            "/ultracode now",
            "no-ultracode",
            "ultracodes",
            "`ultracode`",
            "\"ultracode\"",
            "see ultracode/x",
            "ultracode.md",
        ] {
            assert!(!has_keyword(text, "ultracode"), "{text}");
        }
    }
}
