//! The active agent types of a session: definitions merged by precedence,
//! looked up the way the Agent tool resolves `subagent_type`.

use std::collections::HashMap;
use std::sync::Arc;

use crate::builtin::GENERAL_PURPOSE;
use crate::definition::AgentDefinition;

#[derive(Debug, Clone, Default)]
pub struct AgentDefinitions {
    /// Sorted by name, case-insensitively.
    active: Vec<Arc<AgentDefinition>>,
}

impl AgentDefinitions {
    /// Keeps one definition per name: the higher [`Source`](crate::Source)
    /// wins, and within a source the later one (callers list nearer project
    /// directories last).
    pub fn new(definitions: impl IntoIterator<Item = AgentDefinition>) -> Self {
        let mut definitions: Vec<AgentDefinition> = definitions.into_iter().collect();
        definitions.sort_by_key(|d| d.source);
        let mut by_name: HashMap<String, Arc<AgentDefinition>> = HashMap::with_capacity(definitions.len());
        for def in definitions {
            by_name.insert(def.name.clone(), Arc::new(def));
        }
        let mut active: Vec<Arc<AgentDefinition>> = by_name.into_values().collect();
        active.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)));
        Self { active }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<AgentDefinition>> {
        self.active.iter()
    }

    pub fn get(&self, name: &str) -> Option<&Arc<AgentDefinition>> {
        self.active.iter().find(|d| d.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.active.iter().map(|d| d.name.as_str()).collect()
    }

    /// The definition for a requested `subagent_type`: an exact name, else
    /// the one name equal to it ignoring case, spaces, dashes and underscores.
    /// `None` asks for general-purpose. Errors are worded as Claude Code's.
    pub fn resolve(&self, requested: Option<&str>) -> Result<&Arc<AgentDefinition>, String> {
        let Some(requested) = requested else {
            return self.get(GENERAL_PURPOSE).ok_or_else(|| {
                format!(
                    "subagent_type is required: the general-purpose agent is not available in this session. \
                     Available agents: {}",
                    self.available()
                )
            });
        };
        if let Some(def) = self.get(requested) {
            return Ok(def);
        }
        let wanted = normalize(requested);
        let matches: Vec<&Arc<AgentDefinition>> =
            self.active.iter().filter(|d| !wanted.is_empty() && normalize(&d.name) == wanted).collect();
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("Agent type '{requested}' not found. Available agents: {}", self.available())),
            many => {
                let names: Vec<&str> = many.iter().map(|d| d.name.as_str()).collect();
                Err(format!(
                    "Agent type '{requested}' is ambiguous — matches {}. Use the exact name: {}",
                    names.join(", "),
                    names.join(" or ")
                ))
            }
        }
    }

    fn available(&self) -> String {
        let names = self.names();
        if names.is_empty() { "none".to_owned() } else { names.join(", ") }
    }

    /// The agent listing Claude Code sends as a system note (the Agent tool's
    /// description points the model at it).
    pub fn listing(&self) -> String {
        let mut out = String::from("Available agent types for the Agent tool:");
        for def in &self.active {
            out += &format!("\n- {}: {} (Tools: {})", def.name, def.description, def.tools_summary());
        }
        out += "\n\nWhen you launch multiple agents for independent work, send them in a single message with \
                multiple tool uses so they run concurrently.";
        out
    }
}

/// Claude Code also folds Unicode compatibility forms (NFKC); names here are
/// compared after lowercasing and dropping whitespace, dashes and underscores.
pub(crate) fn normalize(name: &str) -> String {
    name.chars().filter(|c| !c.is_whitespace() && *c != '_' && !is_dash(*c)).flat_map(char::to_lowercase).collect()
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SearchTools, Source, builtin_definitions};

    fn agent(name: &str, source: Source, description: &str) -> AgentDefinition {
        let text = format!("---\nname: {name}\ndescription: {description}\n---\nprompt");
        AgentDefinition::parse(&text, source, None).expect("valid").expect("agent")
    }

    #[test]
    fn precedence_and_order() {
        let defs = AgentDefinitions::new(
            [
                agent("reviewer", Source::Project, "project"),
                agent("reviewer", Source::User, "user"),
                agent("Explore", Source::User, "mine"),
                agent("zeta", Source::Policy, "policy"),
                agent("reviewer", Source::Project, "nearer project"),
            ]
            .into_iter()
            .chain(builtin_definitions(SearchTools::Dedicated)),
        );
        assert_eq!(defs.names(), ["Explore", "general-purpose", "Plan", "reviewer", "zeta"]);
        assert_eq!(defs.get("reviewer").map(|d| d.description.as_str()), Some("nearer project"));
        assert_eq!(defs.get("Explore").map(|d| d.source), Some(Source::User));
    }

    #[test]
    fn resolution_and_errors() {
        let defs = AgentDefinitions::new(builtin_definitions(SearchTools::Dedicated));
        assert_eq!(defs.resolve(None).map(|d| d.name.as_str()), Ok("general-purpose"));
        assert_eq!(defs.resolve(Some("explore")).map(|d| d.name.as_str()), Ok("Explore"));
        assert_eq!(defs.resolve(Some("General Purpose")).map(|d| d.name.as_str()), Ok("general-purpose"));
        assert_eq!(
            defs.resolve(Some("coder")).map(|d| d.name.as_str()),
            Err("Agent type 'coder' not found. Available agents: Explore, general-purpose, Plan".into())
        );
        let twins = AgentDefinitions::new([agent("my-agent", Source::User, "a"), agent("my_agent", Source::User, "b")]);
        assert_eq!(
            twins.resolve(Some("MyAgent")).map(|d| d.name.as_str()),
            Err("Agent type 'MyAgent' is ambiguous — matches my-agent, my_agent. Use the exact name: my-agent or my_agent"
                .into())
        );
        assert_eq!(
            twins.resolve(None).map(|d| d.name.as_str()),
            Err("subagent_type is required: the general-purpose agent is not available in this session. Available \
                 agents: my-agent, my_agent"
                .into())
        );
    }

    #[test]
    fn listing_matches_claude_code() {
        let defs = AgentDefinitions::new(builtin_definitions(SearchTools::Dedicated));
        let listing = defs.listing();
        assert!(listing.starts_with(
            "Available agent types for the Agent tool:\n- Explore: Read-only search agent for broad fan-out searches"
        ));
        assert!(listing.contains(
            "\n- Plan: Software architect agent for designing implementation plans. Use this when you need to plan the \
             implementation strategy for a task. Returns step-by-step plans, identifies critical files, and considers \
             architectural trade-offs. (Tools: All tools except Agent, Artifact, ArtifactComments, ArtifactData, \
             ArtifactCheck, ExitPlanMode, Edit, Write, NotebookEdit)\n"
        ));
        assert!(listing.contains("for you. (Tools: *)\n"));
        assert!(listing.ends_with(
            "\n\nWhen you launch multiple agents for independent work, send them in a single \
                                   message with multiple tool uses so they run concurrently."
        ));
    }
}
