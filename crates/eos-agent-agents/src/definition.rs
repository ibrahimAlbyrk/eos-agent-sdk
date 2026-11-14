//! Agent definitions: what a subagent type is, parsed from Markdown files
//! with YAML frontmatter (`~/.claude/agents/*.md`, `.claude/agents/*.md`).

use std::collections::HashMap;
use std::path::PathBuf;

use crate::frontmatter::{self, Value};

/// Where a definition came from. Later variants win over earlier ones when
/// two definitions share a name, as in Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    BuiltIn,
    Plugin,
    /// `~/.claude/agents`.
    User,
    /// `.claude/agents` in the project.
    Project,
    /// Passed by the host (Claude Code's `--agents` / SDK `agents`).
    Flag,
    /// Managed by the organization.
    Policy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDefinition {
    /// The agent type (`subagent_type`).
    pub name: String,
    /// When to use it; shown to the model in the agent listing.
    pub description: String,
    /// Tools it may use; `None` is every tool. Built-ins keep a literal `"*"`.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    /// A model alias or id, or `"inherit"`; `None` uses the default.
    pub model: Option<String>,
    /// `low`…`max` or an integer budget, as written.
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    /// MCP servers (by name) connected only while this agent runs.
    pub mcp_servers: Vec<String>,
    pub max_turns: Option<u32>,
    /// Skills preloaded into the agent.
    pub skills: Vec<String>,
    /// First user turn when the agent runs as the main session.
    pub initial_prompt: Option<String>,
    /// Always runs in the background.
    pub background: bool,
    /// Run without the user/project CLAUDE.md files.
    pub omit_claude_md: bool,
    pub isolation: Option<String>,
    pub memory: Option<String>,
    pub color: Option<String>,
    pub system_prompt: String,
    pub source: Source,
    pub path: Option<PathBuf>,
}

/// Longest name Claude Code accepts.
const MAX_NAME_LEN: usize = 256;
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const PERMISSION_MODES: [&str; 6] = ["acceptEdits", "auto", "bypassPermissions", "default", "dontAsk", "plan"];

impl AgentDefinition {
    /// Parses one agent file. `Ok(None)` for a Markdown file that is not an
    /// agent (no `name`); `Err` carries the reason a file named as an agent
    /// was rejected. Invalid optional fields are dropped, as Claude Code does.
    pub fn parse(text: &str, source: Source, path: Option<PathBuf>) -> Result<Option<Self>, String> {
        let (fields, body) = frontmatter::split(text);
        let Some(Value::Scalar(name)) = fields.get("name") else { return Ok(None) };
        if name.is_empty() {
            return Ok(None);
        }
        let place = path.as_ref().map_or_else(|| "Agent".to_owned(), |p| format!("Agent file {}", p.display()));
        if name.starts_with('-') {
            return Err(format!("{place} has invalid name '{name}': names must not start with '-'"));
        }
        if name.chars().count() > MAX_NAME_LEN {
            return Err(format!("{place} has invalid name '{name}': names must be at most {MAX_NAME_LEN} characters"));
        }
        if name.contains(':') {
            return Err(format!(
                "{place} has invalid name '{name}': names must not contain ':' (reserved for plugin namespacing)"
            ));
        }
        let description = match fields.get("description") {
            Some(Value::Scalar(d)) if !d.is_empty() => d.replace("\\n", "\n"),
            _ => return Err(format!("{place} is missing required 'description' in frontmatter")),
        };
        let scalar = |key: &str| match fields.get(key) {
            Some(Value::Scalar(v)) if !v.trim().is_empty() => Some(v.trim().to_owned()),
            _ => None,
        };
        let flag = |key: &str| scalar(key).is_some_and(|v| v == "true");
        let one_of = |key: &str, allowed: &[&str]| scalar(key).filter(|v| allowed.contains(&v.as_str()));
        Ok(Some(Self {
            name: name.clone(),
            description,
            tools: tool_list(&fields, "tools", source),
            disallowed_tools: tool_list(&fields, "disallowedTools", source).unwrap_or_default(),
            model: scalar("model").map(|m| if m.eq_ignore_ascii_case("inherit") { "inherit".to_owned() } else { m }),
            effort: scalar("effort").map(|e| e.to_lowercase()).filter(|e| EFFORTS.contains(&e.as_str()) || is_count(e)),
            permission_mode: one_of("permissionMode", &PERMISSION_MODES),
            mcp_servers: strings(&fields, "mcpServers"),
            max_turns: scalar("maxTurns").and_then(|n| n.parse().ok()).filter(|&n| n > 0),
            skills: strings(&fields, "skills").iter().flat_map(|s| split_list(s)).collect(),
            initial_prompt: scalar("initialPrompt"),
            background: flag("background"),
            omit_claude_md: flag("omitClaudeMd"),
            isolation: one_of("isolation", &["worktree", "remote"]),
            memory: one_of("memory", &["user", "project", "local"]),
            color: scalar("color"),
            system_prompt: body.trim().to_owned(),
            source,
            path,
        }))
    }

    /// The tool rule as Claude Code prints it in the agent listing.
    pub fn tools_summary(&self) -> String {
        match (&self.tools, self.disallowed_tools.is_empty()) {
            (Some(tools), false) if !tools.is_empty() => {
                let kept: Vec<&str> =
                    tools.iter().filter(|t| !self.disallowed_tools.contains(t)).map(String::as_str).collect();
                if kept.is_empty() { "None".to_owned() } else { kept.join(", ") }
            }
            (Some(tools), true) if !tools.is_empty() => tools.join(", "),
            (_, false) => format!("All tools except {}", self.disallowed_tools.join(", ")),
            _ => "All tools".to_owned(),
        }
    }

    /// Whether this agent may use the tool called `name`.
    pub fn allows_tool(&self, name: &str) -> bool {
        let named = |rules: &[String]| rules.iter().any(|r| r == "*" || rule_tool(r) == name);
        let allowed = self.tools.as_deref().is_none_or(named);
        allowed && !self.disallowed_tools.iter().any(|r| rule_tool(r) == name)
    }
}

/// `Bash(git log:*)` names the tool `Bash`.
fn rule_tool(rule: &str) -> &str {
    rule.split_once('(').map_or(rule, |(tool, _)| tool).trim()
}

fn is_count(s: &str) -> bool {
    s.parse::<u32>().is_ok_and(|n| n > 0)
}

fn strings(fields: &HashMap<String, Value>, key: &str) -> Vec<String> {
    match fields.get(key) {
        Some(Value::List(items)) => items.clone(),
        Some(Value::Scalar(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// `tools: Read, Grep` or a YAML list. `*` anywhere means every tool (`None`),
/// except in built-ins, which keep it literally. A present but empty field is
/// an empty list: no tools.
fn tool_list(fields: &HashMap<String, Value>, key: &str, source: Source) -> Option<Vec<String>> {
    if !fields.contains_key(key) {
        return None;
    }
    let list: Vec<String> = strings(fields, key).iter().flat_map(|s| split_list(s)).collect();
    if list.iter().any(|t| t == "*") {
        return (source == Source::BuiltIn).then(|| vec!["*".to_owned()]);
    }
    Some(list)
}

/// Splits on commas and spaces outside parentheses, as Claude Code does.
fn split_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' | ' ' if depth == 0 => {
                if !current.trim().is_empty() {
                    out.push(current.trim().to_owned());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> AgentDefinition {
        AgentDefinition::parse(text, Source::User, None).expect("valid").expect("an agent")
    }

    #[test]
    fn frontmatter_fields() {
        let def = parse(
            "---\nname: reviewer\ndescription: Reviews code.\\nCarefully.\ntools: Read, Bash(git log:*), Grep\n\
             disallowedTools: [Grep]\nmodel: Inherit\neffort: HIGH\npermissionMode: plan\nmaxTurns: 12\n\
             skills: a, b\nmcpServers:\n  - github\ninitialPrompt: go\nbackground: true\nomitClaudeMd: \"true\"\n\
             isolation: worktree\nmemory: project\ncolor: cyan\n---\n\n  You review.\n",
        );
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews code.\nCarefully.");
        assert_eq!(def.tools, Some(vec!["Read".into(), "Bash(git log:*)".into(), "Grep".into()]));
        assert_eq!(def.disallowed_tools, vec!["Grep".to_owned()]);
        assert_eq!(def.model.as_deref(), Some("inherit"));
        assert_eq!(def.effort.as_deref(), Some("high"));
        assert_eq!(def.permission_mode.as_deref(), Some("plan"));
        assert_eq!(def.max_turns, Some(12));
        assert_eq!(def.skills, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(def.mcp_servers, vec!["github".to_owned()]);
        assert_eq!(def.initial_prompt.as_deref(), Some("go"));
        assert!(def.background && def.omit_claude_md);
        assert_eq!((def.isolation.as_deref(), def.memory.as_deref()), (Some("worktree"), Some("project")));
        assert_eq!(def.system_prompt, "You review.");
        assert_eq!(def.tools_summary(), "Read, Bash(git log:*)");
        assert!(def.allows_tool("Bash") && def.allows_tool("Read"));
        assert!(!def.allows_tool("Grep") && !def.allows_tool("Write"));
    }

    #[test]
    fn invalid_values_are_dropped() {
        let def = parse(
            "---\nname: x\ndescription: d\nmodel: \"  \"\neffort: huge\npermissionMode: yolo\nmaxTurns: -1\n\
             isolation: vm\nbackground: yes\n---\nbody",
        );
        assert_eq!((&def.model, &def.effort, &def.permission_mode, def.max_turns), (&None, &None, &None, None));
        assert_eq!((&def.isolation, def.background), (&None, false));
        assert_eq!(def.tools, None);
        assert_eq!(def.tools_summary(), "All tools");
        assert!(def.allows_tool("Anything"));
    }

    #[test]
    fn star_and_empty_tool_lists() {
        let all = parse("---\nname: a\ndescription: d\ntools: Read, *\ndisallowedTools: Write\n---\n");
        assert_eq!(all.tools, None);
        assert_eq!(all.tools_summary(), "All tools except Write");
        assert!(all.allows_tool("Read") && !all.allows_tool("Write"));
        let none = parse("---\nname: b\ndescription: d\ntools:\ndisallowedTools: Read\n---\n");
        assert_eq!(none.tools, Some(Vec::new()));
        assert!(!none.allows_tool("Read"));
        let built_in =
            AgentDefinition::parse("---\nname: c\ndescription: d\ntools: \"*\"\n---\n", Source::BuiltIn, None);
        assert_eq!(built_in.expect("valid").expect("agent").tools_summary(), "*");
    }

    #[test]
    fn rejected_files() {
        let parse = |t: &str| AgentDefinition::parse(t, Source::Project, Some("/p/a.md".into()));
        assert_eq!(parse("# notes\n"), Ok(None));
        assert_eq!(parse("---\ndescription: d\n---\n"), Ok(None));
        assert_eq!(
            parse("---\nname: a\n---\n"),
            Err("Agent file /p/a.md is missing required 'description' in frontmatter".into())
        );
        assert!(parse("---\nname: -a\ndescription: d\n---\n").is_err());
        assert!(parse("---\nname: plug:a\ndescription: d\n---\n").is_err());
    }
}
