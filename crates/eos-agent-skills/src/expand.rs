//! Loading a skill for a turn — one path for `/name args` typed by the user
//! and for the Skill tool: base directory line, arguments, inline shell.

use std::borrow::Cow;
use std::path::Path;

use eos_agent_core::ToolContext;

use crate::args::{self, Vars};
use crate::bundled;
use crate::inline::{CommandRunner, run_inline};
use crate::skill::{Body, Origin, Skill};

/// What expansion needs from the session.
#[derive(Debug, Clone, Copy)]
pub struct ExpandContext<'a> {
    pub runner: &'a dyn CommandRunner,
    /// `${CLAUDE_PROJECT_DIR}`.
    pub project_dir: Option<&'a Path>,
    /// `${CLAUDE_SESSION_ID}`.
    pub session_id: Option<&'a str>,
    /// `${CLAUDE_EFFORT}`.
    pub effort: Option<&'a str>,
    /// The agent the skill loads for, as its inline commands run as its
    /// shell tool calls.
    pub call: Option<ToolContext<'a>>,
}

impl<'a> ExpandContext<'a> {
    pub fn new(runner: &'a dyn CommandRunner) -> Self {
        Self { runner, project_dir: None, session_id: None, effort: None, call: None }
    }
}

/// A skill loaded for a turn. The engine adds `visible` (user typed it) and
/// `body` (hidden, meta) to the conversation and applies the rest to the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub name: String,
    pub args: String,
    /// `<command-message>…</command-message>` and friends.
    pub visible: String,
    /// The skill's instructions with everything filled in.
    pub body: String,
    /// Permission patterns granted for the turn.
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// `context: fork`: run it as a subagent (with `agent`, if named).
    pub fork: bool,
    pub agent: Option<String>,
    /// The skill's `background` (see [`crate::Skill::background`]).
    pub background: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    #[error(
        "This skill, command or hook and the arguments filled into it add up to more than 4,000,000 characters. Use \
shorter arguments or fewer placeholders such as $ARGUMENTS."
    )]
    TooLong,
    #[error("Shell command failed for pattern \"{pattern}\": {message}")]
    Shell { pattern: String, message: String },
    #[error("Shell command permission check failed for pattern \"{pattern}\": {message}")]
    ShellDenied { pattern: String, message: String },
}

pub(crate) async fn render(skill: &Skill, args: &str, ctx: &ExpandContext<'_>) -> Result<Expansion, ExpandError> {
    let body = match &skill.body {
        Body::Bundled(kind) => bundled::render(*kind, args),
        Body::Rendered(renderer) => renderer.render(args, ctx.call).await,
        Body::Text(text) => {
            let text = match &skill.base_dir {
                Some(dir) => Cow::Owned(format!("Base directory for this skill: {}\n\n{text}", dir.display())),
                None => Cow::Borrowed(text.as_str()),
            };
            let skill_dir = skill.base_dir.as_deref().map(Path::to_string_lossy);
            let project_dir = ctx.project_dir.map(Path::to_string_lossy);
            let plugin_root = skill.plugin_root.as_deref().map(Path::to_string_lossy);
            let vars = Vars {
                skill_dir: skill_dir.as_deref(),
                project_dir: project_dir.as_deref(),
                session_id: ctx.session_id,
                effort: ctx.effort,
                plugin_root: plugin_root.as_deref(),
            };
            let text = args::substitute(&text, args, &skill.argument_names, &vars)?;
            run_inline(text, ctx.runner, ctx.call).await?
        }
    };
    Ok(Expansion {
        name: skill.name.clone(),
        args: args.to_owned(),
        visible: visible(skill, args),
        body,
        allowed_tools: skill.allowed_tools.clone(),
        disallowed_tools: skill.disallowed_tools.clone(),
        model: skill.model.clone(),
        effort: skill.effort.clone(),
        fork: skill.fork,
        agent: skill.agent.clone(),
        background: skill.background,
    })
}

/// The visible message of a loaded skill, as Claude Code writes it.
fn visible(skill: &Skill, args: &str) -> String {
    let name = &skill.name;
    let model_only_skill = !skill.user_invocable
        && matches!(skill.origin, Origin::UserSkill | Origin::ProjectSkill | Origin::PluginSkill { .. });
    if model_only_skill {
        return format!(
            "<command-message>{name}</command-message>\n<command-name>{name}</command-name>\n<skill-format>true</skill-format>"
        );
    }
    let mut out = format!("<command-message>{name}</command-message>\n<command-name>/{name}</command-name>");
    if !args.is_empty() {
        out.push_str(&format!("\n<command-args>{args}</command-args>"));
    }
    out
}
