//! What the host needs from a definition to start a subagent.

use crate::definition::AgentDefinition;

/// A definition resolved against one spawn request and its parent.
#[derive(Debug, Clone, Copy)]
pub struct SpawnPlan<'a> {
    pub definition: &'a AgentDefinition,
    /// The model to run: the call's `model`, else the definition's, else the
    /// parent's (`inherit`). Aliases (`sonnet`, `opus`, …) are left for the
    /// host to map to model ids.
    pub model: &'a str,
    /// Empty for a fork, which keeps the parent's system prompt.
    pub system_prompt: &'a str,
    pub include_claude_md: bool,
    pub max_turns: Option<u32>,
    /// Start from a copy of the parent's conversation with the parent's tools
    /// and system prompt, so the prompt cache carries over.
    pub fork: bool,
}

impl<'a> SpawnPlan<'a> {
    pub fn new(
        definition: &'a AgentDefinition,
        requested_model: Option<&'a str>,
        parent_model: &'a str,
        fork: bool,
    ) -> Self {
        let model = if fork {
            parent_model
        } else {
            requested_model.or(definition.model.as_deref()).filter(|m| *m != "inherit").unwrap_or(parent_model)
        };
        Self {
            definition,
            model,
            system_prompt: &definition.system_prompt,
            include_claude_md: !definition.omit_claude_md,
            max_turns: definition.max_turns,
            fork,
        }
    }

    /// Whether the subagent gets the parent's tool `name`. Apply it to the
    /// parent's own tool pool; a fork keeps every tool.
    pub fn allows_tool(&self, name: &str) -> bool {
        self.fork || self.definition.allows_tool(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EXPLORE, GENERAL_PURPOSE, SearchTools, builtin_definitions, fork_definition};

    #[test]
    fn models_prompts_and_tools() {
        let defs = builtin_definitions(SearchTools::Dedicated);
        let find = |n: &str| defs.iter().find(|d| d.name == n).expect("built-in");
        let explore = SpawnPlan::new(find(EXPLORE), None, "claude-opus-5-5", false);
        assert_eq!(explore.model, "claude-opus-5-5");
        assert!(!explore.include_claude_md);
        assert!(explore.allows_tool("Grep") && !explore.allows_tool("Write"));
        assert!(explore.system_prompt.starts_with("You are a file search specialist"));
        let gp = SpawnPlan::new(find(GENERAL_PURPOSE), Some("haiku"), "claude-opus-5-5", false);
        assert_eq!(gp.model, "haiku");
        assert!(gp.include_claude_md && gp.allows_tool("Write"));

        let mut pinned = find(GENERAL_PURPOSE).clone();
        pinned.model = Some("sonnet".into());
        assert_eq!(SpawnPlan::new(&pinned, None, "p", false).model, "sonnet");

        let fork = fork_definition();
        let plan = SpawnPlan::new(&fork, Some("haiku"), "claude-opus-5-5", true);
        assert_eq!((plan.model, plan.system_prompt, plan.max_turns), ("claude-opus-5-5", "", Some(200)));
        assert!(plan.allows_tool("Anything") && plan.include_claude_md);
    }
}
