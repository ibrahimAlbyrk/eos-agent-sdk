//! The context Claude Code sends with the first message: `<system-reminder>`
//! blocks in front of the user's text (instruction files, account facts) and
//! one `role: "system"` message (environment, model, deferred tools, skills,
//! date).

use std::path::PathBuf;

use eos_agent_core::{SessionContext, StoredMessage};
use eos_agent_types::ContentBlock;

use crate::env::EnvFacts;
use crate::library::{Fragment, Prompts, system_reminder};

/// Where an instruction file comes from; it decides the label Claude Code
/// puts after the file's path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionScope {
    Managed,
    User,
    Project,
    Local,
}

impl InstructionScope {
    fn label(self) -> &'static str {
        match self {
            Self::Project => " (project instructions, checked into the codebase)",
            Self::Local => " (user's private project instructions, not checked in)",
            Self::Managed => " (organization-managed policy instructions)",
            Self::User => " (user's private global instructions for all projects)",
        }
    }
}

/// One CLAUDE.md-style file, in the order it should be shown.
#[derive(Debug, Clone)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: InstructionScope,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// e.g. "Opus 5.5"; without it only the id is stated.
    pub marketing_name: Option<String>,
    /// e.g. "June 2026".
    pub knowledge_cutoff: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SessionContextBuilder {
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    instructions: Vec<InstructionFile>,
    user_email: Option<String>,
    git_status: Option<String>,
    scratchpad: Option<PathBuf>,
    model: Option<ModelInfo>,
    deferred_tools: Vec<String>,
    deferred_notes: Vec<String>,
    leading_sections: Vec<String>,
    extra_sections: Vec<String>,
    skills: Vec<SkillEntry>,
    date: Option<String>,
    env: Option<EnvFacts>,
    system_turns: bool,
}

impl SessionContextBuilder {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            additional_dirs: Vec::new(),
            instructions: Vec::new(),
            user_email: None,
            git_status: None,
            scratchpad: None,
            model: None,
            deferred_tools: Vec::new(),
            deferred_notes: Vec::new(),
            leading_sections: Vec::new(),
            extra_sections: Vec::new(),
            skills: Vec::new(),
            date: None,
            env: None,
            system_turns: true,
        }
    }

    /// `false` for a model that takes no mid-conversation `role: "system"`
    /// message: each section of the system message goes in front of the
    /// user's text as its own `<system-reminder>` block, as Claude Code
    /// sends them to such models (then the instructions and account
    /// blocks, then the date).
    pub fn system_turns(mut self, on: bool) -> Self {
        self.system_turns = on;
        self
    }

    pub fn additional_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.additional_dirs = dirs;
        self
    }

    pub fn instructions(mut self, files: Vec<InstructionFile>) -> Self {
        self.instructions = files;
        self
    }

    pub fn user_email(mut self, email: Option<String>) -> Self {
        self.user_email = email.filter(|e| !e.is_empty());
        self
    }

    /// Shown as given under `# gitStatus` (Claude Code's snapshot text).
    pub fn git_status(mut self, status: Option<String>) -> Self {
        self.git_status = status.filter(|s| !s.is_empty());
        self
    }

    pub fn scratchpad(mut self, dir: Option<PathBuf>) -> Self {
        self.scratchpad = dir;
        self
    }

    pub fn model(mut self, model: ModelInfo) -> Self {
        self.model = Some(model);
        self
    }

    /// Names of the tools sent with `defer_loading`, announced for ToolSearch.
    pub fn deferred_tools(mut self, names: Vec<String>) -> Self {
        self.deferred_tools = names;
        self
    }

    /// Further paragraphs of the same announcement (Claude Code's notes on
    /// MCP servers that need a sign-in or failed): one section with the
    /// deferred tools, or on their own when no tool is deferred.
    pub fn deferred_notes(mut self, notes: Vec<String>) -> Self {
        self.deferred_notes = notes;
        self
    }

    /// Sections that open the system message, before the environment (the
    /// Bash sandbox section, as Claude Code places it).
    pub fn leading_sections(mut self, sections: Vec<String>) -> Self {
        self.leading_sections = sections;
        self
    }

    /// Further sections of the system message (agent listing, MCP notes, …),
    /// placed after the deferred tools and before the skills, as Claude Code
    /// orders them.
    pub fn extra_sections(mut self, sections: Vec<String>) -> Self {
        self.extra_sections = sections;
        self
    }

    pub fn skills(mut self, skills: Vec<SkillEntry>) -> Self {
        self.skills = skills;
        self
    }

    /// `YYYY-MM-DD`; the local date when not set.
    pub fn date(mut self, date: impl Into<String>) -> Self {
        self.date = Some(date.into());
        self
    }

    /// Replaces the detected platform, shell, OS and repository facts.
    pub fn env(mut self, env: EnvFacts) -> Self {
        self.env = Some(env);
        self
    }

    pub fn build(&self, prompts: &Prompts) -> SessionContext {
        if !self.system_turns {
            return self.build_without_system_turns(prompts);
        }
        let mut user_prefix = Vec::new();
        if let Some(text) = self.instructions_text(prompts) {
            user_prefix.push(text_block(system_reminder(&text)));
        }
        if let Some(text) = self.account_text(prompts) {
            // Claude Code ends this block, unlike the others, with a newline.
            user_prefix.push(text_block(system_reminder(&text) + "\n"));
        }
        SessionContext {
            user_prefix: user_prefix.into(),
            system_notes: vec![StoredMessage::system_text(self.system_message(prompts))],
            ..SessionContext::default()
        }
    }

    fn build_without_system_turns(&self, prompts: &Prompts) -> SessionContext {
        let (mut sections, date) = self.sections(prompts);
        if let Some(text) = self.instructions_text(prompts) {
            sections.push(text);
        }
        sections.extend(self.account_text(prompts));
        sections.push(date);
        let mut blocks: Vec<ContentBlock> = sections.iter().map(|s| text_block(system_reminder(s))).collect();
        // The last context block ends with a newline before the user's text.
        if let Some(ContentBlock::Text { text, .. }) = blocks.last_mut() {
            text.push('\n');
        }
        SessionContext { user_prefix: blocks.into(), ..SessionContext::default() }
    }

    fn instructions_text(&self, prompts: &Prompts) -> Option<String> {
        let files: Vec<String> = self
            .instructions
            .iter()
            .filter(|f| !f.content.trim().is_empty())
            .map(|f| format!("Contents of {}{}:\n\n{}", f.path.display(), f.scope.label(), f.content.trim()))
            .collect();
        (!files.is_empty())
            .then(|| format!("{}\n\n{}", prompts.get(Fragment::InstructionsPreamble), files.join("\n\n")))
    }

    fn account_text(&self, prompts: &Prompts) -> Option<String> {
        let mut sections = Vec::new();
        if let Some(email) = &self.user_email {
            sections.push(format!("# userEmail\n{}", prompts.render(Fragment::UserEmail, &[("EMAIL", email)])));
        }
        if let Some(status) = &self.git_status {
            sections.push(format!("# gitStatus\n{status}"));
        }
        (!sections.is_empty()).then(|| prompts.render(Fragment::SessionContext, &[("SECTIONS", &sections.join("\n"))]))
    }

    fn system_message(&self, prompts: &Prompts) -> String {
        let (mut parts, date) = self.sections(prompts);
        parts.push(date);
        parts.join("\n\n")
    }

    /// The system message's sections and, apart, its closing date line.
    fn sections(&self, prompts: &Prompts) -> (Vec<String>, String) {
        let env = self.env.clone().unwrap_or_else(|| EnvFacts::detect(&self.cwd));
        let mut parts: Vec<String> = self.leading_sections.iter().filter(|s| !s.is_empty()).cloned().collect();
        parts.push(self.environment(prompts, &env));
        if let Some(model) = &self.model {
            parts.push(model_line(prompts, model));
        }
        let mut announcement = Vec::new();
        if !self.deferred_tools.is_empty() {
            let mut names = self.deferred_tools.clone();
            names.sort_unstable();
            announcement.push(prompts.render(Fragment::DeferredTools, &[("NAMES", &names.join("\n"))]));
        }
        announcement.extend(self.deferred_notes.iter().filter(|s| !s.is_empty()).cloned());
        if !announcement.is_empty() {
            parts.push(announcement.join("\n\n"));
        }
        parts.extend(self.extra_sections.iter().filter(|s| !s.is_empty()).cloned());
        if !self.skills.is_empty() {
            let lines: Vec<String> = self
                .skills
                .iter()
                .map(|s| {
                    if s.description.is_empty() {
                        format!("- {}", s.name)
                    } else {
                        format!("- {}: {}", s.name, s.description)
                    }
                })
                .collect();
            parts.push(prompts.render(Fragment::Skills, &[("SKILLS", &lines.join("\n"))]));
        }
        let date = self.date.clone().unwrap_or_else(crate::reminders::local_date);
        (parts, prompts.render(Fragment::Date, &[("DATE", &date)]))
    }

    fn environment(&self, prompts: &Prompts, env: &EnvFacts) -> String {
        let mut lines = vec![format!(" - Primary working directory: {}", self.cwd.display())];
        if env.is_worktree {
            lines.push(format!(" - {}", prompts.get(Fragment::Worktree)));
            lines.push(format!(" - {}", prompts.get(Fragment::WorktreeStash)));
        }
        lines.push(format!(" - Is a git repository: {}", env.is_git_repo));
        if !self.additional_dirs.is_empty() {
            lines.push(" - Additional working directories:".into());
            lines.extend(self.additional_dirs.iter().map(|d| format!("  - {}", d.display())));
        }
        lines.push(format!(" - Platform: {}", env.platform));
        lines.push(format!(" - Shell: {}", env.shell));
        if let Some(os) = &env.os_version {
            lines.push(format!(" - OS Version: {os}"));
        }
        if let Some(dir) = &self.scratchpad {
            let path = dir.display().to_string();
            lines.push(format!(" - {}", prompts.render(Fragment::Scratchpad, &[("PATH", &path)])));
        }
        lines.push(format!(" - {}", prompts.get(Fragment::UntrustedDownloads)));
        prompts.render(Fragment::Environment, &[("LINES", &lines.join("\n"))])
    }
}

fn model_line(prompts: &Prompts, model: &ModelInfo) -> String {
    let mut line = match &model.marketing_name {
        Some(name) => prompts.render(Fragment::Model, &[("NAME", name), ("ID", &model.id)]),
        None => prompts.render(Fragment::ModelUnnamed, &[("ID", &model.id)]),
    };
    if let Some(cutoff) = &model.knowledge_cutoff {
        line.push(' ');
        line.push_str(&prompts.render(Fragment::KnowledgeCutoff, &[("CUTOFF", cutoff)]));
    }
    line
}

fn text_block(text: String) -> ContentBlock {
    ContentBlock::Text { text, citations: None }
}
