//! Claude Code's built-in agent types, kept as agent files in `prompts/agents/`.

use crate::definition::{AgentDefinition, Source};

pub const GENERAL_PURPOSE: &str = "general-purpose";
pub const EXPLORE: &str = "Explore";
pub const PLAN: &str = "Plan";
/// The pseudo-type that forks the calling agent.
pub const FORK: &str = "fork";
/// The type a workflow's `agent()` calls run as; never listed.
pub const WORKFLOW_SUBAGENT: &str = "workflow-subagent";

/// How the host offers code search, which Explore's and Plan's prompts name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTools {
    /// Dedicated `Glob` and `Grep` tools.
    Dedicated,
    /// `find` and `grep` through `Bash` (Claude Code's default when its
    /// binary embeds them, which drops Glob and Grep).
    Shell,
}

/// general-purpose, Explore and Plan.
pub fn builtin_definitions(search: SearchTools) -> Vec<AgentDefinition> {
    let (file_search, read_only, search_tools) = match search {
        SearchTools::Dedicated => (
            "- Use Glob for broad file pattern matching\n- Use Grep for searching file contents with regex",
            "ls, git status, git log, git diff, find, cat, head, tail",
            "Glob, Grep, and Read",
        ),
        SearchTools::Shell => (
            "- Use `find` via Bash for broad file pattern matching\n- Use `grep` via Bash for searching file contents with regex",
            "ls, git status, git log, git diff, find, grep, cat, head, tail",
            "`find`, `grep`, and Read",
        ),
    };
    [
        include_str!("../prompts/agents/general-purpose.md"),
        include_str!("../prompts/agents/Explore.md"),
        include_str!("../prompts/agents/Plan.md"),
    ]
    .into_iter()
    .map(|text| {
        let mut def =
            AgentDefinition::parse(text, Source::BuiltIn, None).ok().flatten().expect("bundled agent files are valid");
        def.system_prompt = def
            .system_prompt
            .replace("{{FILE_SEARCH_GUIDELINES}}", file_search)
            .replace("{{READ_ONLY_COMMANDS}}", read_only)
            .replace("{{SEARCH_TOOLS}}", search_tools);
        def
    })
    .collect()
}

/// The `fork` pseudo-type: the parent's context, tools, model and system
/// prompt, a turn cap, and permission prompts bubbled up to the parent.
pub fn fork_definition() -> AgentDefinition {
    AgentDefinition {
        name: FORK.to_owned(),
        description: "Fork — inherits full conversation context. Selected explicitly via subagent_type: \"fork\" when \
                      the fork gate is on; never the default."
            .to_owned(),
        tools: Some(vec!["*".to_owned()]),
        disallowed_tools: Vec::new(),
        model: Some("inherit".to_owned()),
        effort: None,
        permission_mode: Some("bubble".to_owned()),
        mcp_servers: Vec::new(),
        max_turns: Some(200),
        skills: Vec::new(),
        initial_prompt: None,
        background: false,
        omit_claude_md: false,
        isolation: None,
        memory: None,
        color: None,
        system_prompt: String::new(),
        source: Source::BuiltIn,
        path: None,
    }
}

/// Claude Code's `workflow-subagent`: every tool but the ones a workflow
/// agent must not use; the workflow supplies its system prompt per call.
pub fn workflow_subagent_definition() -> AgentDefinition {
    AgentDefinition {
        name: WORKFLOW_SUBAGENT.to_owned(),
        description: "Internal subagent for workflow script orchestration.".to_owned(),
        tools: Some(vec!["*".to_owned()]),
        disallowed_tools: vec!["SendUserMessage".to_owned(), "Agent".to_owned(), "Workflow".to_owned()],
        system_prompt: String::new(),
        ..fork_definition()
    }
    .with_model_inherit()
}

impl AgentDefinition {
    fn with_model_inherit(mut self) -> Self {
        self.model = Some("inherit".to_owned());
        self.permission_mode = None;
        self.max_turns = None;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_complete() {
        let defs = builtin_definitions(SearchTools::Dedicated);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, [GENERAL_PURPOSE, EXPLORE, PLAN]);
        let gp = &defs[0];
        assert_eq!(gp.tools_summary(), "*");
        assert!(gp.system_prompt.starts_with("You are an agent for Claude Code"));
        assert!(!gp.omit_claude_md && gp.model.is_none());
        for def in &defs[1..] {
            assert!(def.omit_claude_md);
            assert_eq!(def.model.as_deref(), Some("inherit"));
            assert_eq!(
                def.tools_summary(),
                "All tools except Agent, Artifact, ArtifactComments, ArtifactData, ArtifactCheck, ExitPlanMode, Edit, \
                 Write, NotebookEdit"
            );
            assert!(def.allows_tool("Read") && def.allows_tool("Bash"));
            assert!(!def.allows_tool("Edit") && !def.allows_tool("Agent"));
            assert!(def.system_prompt.contains("=== CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ==="));
            assert!(!def.system_prompt.contains("{{"));
        }
        assert!(defs[1].system_prompt.contains("- Use Glob for broad file pattern matching\n- Use Grep for"));
        assert!(defs[1].description.starts_with("Read-only search agent for broad fan-out searches —"));
        assert!(defs[1].description.contains("doesn't review"));
        assert!(defs[2].system_prompt.contains("conventions using Glob, Grep, and Read\n"));
        let shell = builtin_definitions(SearchTools::Shell);
        assert!(shell[1].system_prompt.contains("(ls, git status, git log, git diff, find, grep, cat, head, tail)"));
        assert!(shell[2].system_prompt.contains("using `find`, `grep`, and Read"));
    }
}
