//! One skill or prompt command, as read from its file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bundled::{Bundled, SkillRenderer};
use crate::frontmatter::{self, Frontmatter};

/// Where a skill came from. Claude Code's `loadedFrom`, roughly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Bundled,
    /// `~/.claude/skills/<name>/SKILL.md`
    UserSkill,
    /// `<project>/.claude/skills/<name>/SKILL.md`
    ProjectSkill,
    /// Legacy `~/.claude/commands/*.md`
    UserCommand,
    /// Legacy `<project>/.claude/commands/*.md`
    ProjectCommand,
    PluginSkill {
        plugin: String,
    },
    PluginCommand {
        plugin: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum Body {
    Text(String),
    Bundled(Bundled),
    /// Composed when loaded.
    Rendered(Arc<dyn SkillRenderer>),
}

#[derive(Debug, Clone)]
pub struct Skill {
    /// The invocation name (`deploy`, `plugin:deploy`, `frontend:component`).
    pub name: String,
    /// The frontmatter's `name`, when given.
    pub display_name: Option<String>,
    pub description: String,
    /// The description came from the frontmatter (not the body's first line).
    pub has_user_description: bool,
    pub when_to_use: Option<String>,
    pub argument_hint: Option<String>,
    /// Names for `$name` placeholders, in argument order.
    pub argument_names: Vec<String>,
    /// Permission patterns granted while the skill runs.
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    /// A model id for the turn; `None` = inherit.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Only the user may invoke it (`/name`); hidden from the model.
    pub disable_model_invocation: bool,
    pub user_invocable: bool,
    /// `context: fork`: meant to run as a subagent.
    pub fork: bool,
    pub agent: Option<String>,
    /// `background`: may a typed `/name` of a fork skill launch in the
    /// background (Claude Code's interactive terminal)? `None`: yes.
    pub background: Option<bool>,
    /// Path globs that make this a path-scoped skill.
    pub paths: Vec<String>,
    pub origin: Origin,
    /// The skill's own folder; its body is introduced with it.
    pub base_dir: Option<PathBuf>,
    pub plugin_root: Option<PathBuf>,
    pub(crate) body: Body,
    /// Identity of the listing entry, to send only new ones later.
    pub(crate) digest: u64,
}

const MAX_SKILL_FILE: u64 = 1_000_000;

/// Claude Code's reading of a yes/no frontmatter value that may also be
/// unset (`O_e`): `1`/`true`/`yes`/`on`, `0`/`false`/`no`/`off`, else unset.
fn flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Skill {
    /// A skill from a markdown file's text (frontmatter + body).
    pub fn from_markdown(name: &str, text: &str, origin: Origin, base_dir: Option<PathBuf>) -> Self {
        let (fm, body) = frontmatter::split(text);
        let fallback =
            if matches!(origin, Origin::UserCommand | Origin::ProjectCommand) { "Custom command" } else { "Skill" };
        let user_description = fm.str("description").map(|d| d.trim().to_owned());
        let description = user_description.clone().unwrap_or_else(|| first_line(body, fallback));
        let mut skill = Self {
            name: name.to_owned(),
            display_name: fm.str("name").map(|s| s.trim().to_owned()),
            description,
            has_user_description: user_description.is_some(),
            when_to_use: fm.str("when-to-use").map(|s| s.trim().to_owned()),
            argument_hint: fm.str("argument-hint").map(str::to_owned),
            argument_names: fm
                .list("arguments", words)
                .into_iter()
                .filter(|n| !n.bytes().all(|b| b.is_ascii_digit()))
                .collect(),
            allowed_tools: tools(&fm, "allowed-tools"),
            disallowed_tools: tools(&fm, "disallowed-tools"),
            model: fm.str("model").map(str::trim).filter(|m| !m.eq_ignore_ascii_case("inherit")).map(str::to_owned),
            effort: fm.str("effort").map(str::trim).filter(|e| valid_effort(e)).map(str::to_owned),
            disable_model_invocation: fm.bool("disable-model-invocation").unwrap_or(false),
            user_invocable: fm.bool("user-invocable").unwrap_or(true),
            fork: fm.str("context").is_some_and(|c| c.trim() == "fork"),
            agent: fm.str("agent").map(str::to_owned),
            background: fm.str("background").and_then(flag),
            paths: fm
                .list("paths", |s| s.split(',').map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect())
                .into_iter()
                .map(|p| p.strip_suffix("/**").map(str::to_owned).unwrap_or(p))
                .filter(|p| !p.is_empty() && p != "**")
                .collect(),
            origin,
            base_dir,
            plugin_root: None,
            body: Body::Text(body.to_owned()),
            digest: 0,
        };
        skill.digest = skill.compute_digest();
        skill
    }

    pub(crate) fn bundled(name: &str, description: &str, argument_hint: &str, kind: Bundled) -> Self {
        let mut skill = Self::from_markdown(name, "", Origin::Bundled, None);
        skill.description = description.to_owned();
        skill.has_user_description = true;
        skill.argument_hint = Some(argument_hint.to_owned());
        skill.disable_model_invocation = true;
        skill.body = Body::Bundled(kind);
        skill.digest = skill.compute_digest();
        skill
    }

    /// Reads `path` (at most 1 MB); `None` when unreadable or too large.
    pub(crate) fn from_file(name: &str, path: &Path, origin: Origin, base_dir: Option<PathBuf>) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_SKILL_FILE {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        Some(Self::from_markdown(name, &text, origin, base_dir))
    }

    /// The instructions as written (`None` for bundled commands built in code).
    pub fn body(&self) -> Option<&str> {
        match &self.body {
            Body::Text(text) => Some(text),
            Body::Bundled(_) | Body::Rendered(_) => None,
        }
    }

    /// The listing's description: `description[ - when_to_use]`.
    pub fn listing_description(&self) -> String {
        match &self.when_to_use {
            Some(when) => format!("{} - {when}", self.description),
            None => self.description.clone(),
        }
    }

    /// May the model see and call it? (Claude Code lists plugin items only
    /// when they describe themselves.)
    pub fn is_listed(&self) -> bool {
        if self.disable_model_invocation || !self.paths.is_empty() {
            return false;
        }
        match self.origin {
            Origin::PluginSkill { .. } | Origin::PluginCommand { .. } => {
                self.has_user_description || self.when_to_use.is_some()
            }
            _ => true,
        }
    }

    pub(crate) fn compute_digest(&self) -> u64 {
        let key = format!("{}\0{}", self.name, self.listing_description());
        eos_agent_core::content_hash(key.as_bytes())
    }
}

fn first_line(body: &str, fallback: &str) -> String {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let heading = line.trim_start_matches('#');
        let text =
            if heading.len() < line.len() && heading.starts_with(char::is_whitespace) { heading.trim() } else { line };
        return if text.chars().count() > 100 {
            format!("{}...", text.chars().take(97).collect::<String>())
        } else {
            text.to_owned()
        };
    }
    fallback.to_owned()
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// `Bash(git add:*), Read` → two patterns; commas and spaces inside
/// parentheses stay part of the pattern.
fn tools(fm: &Frontmatter, key: &str) -> Vec<String> {
    fm.list(key, |s| {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut depth = 0usize;
        for c in s.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
            if depth == 0 && (c == ',' || c.is_whitespace()) {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            } else {
                current.push(c);
            }
        }
        if !current.is_empty() {
            out.push(current);
        }
        out
    })
}

fn valid_effort(effort: &str) -> bool {
    matches!(effort, "low" | "medium" | "high" | "xhigh" | "max") || effort.parse::<u32>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_frontmatter_fields() {
        let s = Skill::from_markdown(
            "deploy",
            "---\ndescription: Deploy the app\nwhen_to_use: on request\nargument-hint: <env>\narguments: env region\n\
             allowed-tools: Bash(git add:*), Read\nmodel: inherit\neffort: high\ndisable-model-invocation: true\n\
             context: fork\nagent: Explore\npaths: src/**, docs\n---\nDo it.",
            Origin::UserSkill,
            None,
        );
        assert_eq!(s.description, "Deploy the app");
        assert_eq!(s.listing_description(), "Deploy the app - on request");
        assert_eq!(s.argument_names, ["env", "region"]);
        assert_eq!(s.allowed_tools, ["Bash(git add:*)", "Read"]);
        assert_eq!(s.model, None);
        assert_eq!(s.effort.as_deref(), Some("high"));
        assert!(s.disable_model_invocation && s.fork && s.user_invocable);
        assert_eq!(s.agent.as_deref(), Some("Explore"));
        assert_eq!(s.background, None);
        assert_eq!(s.paths, ["src", "docs"]);
        assert_eq!(s.body(), Some("Do it."));
    }

    #[test]
    fn background_reads_as_claude_code_reads_it() {
        let background = |value: &str| {
            Skill::from_markdown("x", &format!("---\nbackground: {value}\n---\nDo it."), Origin::UserSkill, None)
                .background
        };
        assert_eq!(background("false"), Some(false));
        assert_eq!(background("Off"), Some(false));
        assert_eq!(background("0"), Some(false));
        assert_eq!(background("yes"), Some(true));
        assert_eq!(background("maybe"), None);
    }

    #[test]
    fn description_falls_back_to_first_line() {
        let s = Skill::from_markdown("x", "\n# Review the diff\n\nMore.", Origin::ProjectSkill, None);
        assert_eq!(s.description, "Review the diff");
        assert!(!s.has_user_description);
        let c = Skill::from_markdown("y", "", Origin::UserCommand, None);
        assert_eq!(c.description, "Custom command");
    }
}
