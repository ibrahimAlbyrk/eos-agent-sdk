//! `agent({disallowedTools})` as Claude Code reads it. Each entry is a
//! permission-rule spelling (`Tool`, `Tool(content)`, `mcp__server`,
//! `mcp__server__*`, `mcp__server__tool`, `mcp__*`); before the spawn every
//! entry that takes nothing from the session's tool pool is checked, so a
//! misspelt deny is refused instead of silently doing nothing.

use std::collections::HashSet;

const SKILL: &str = "Skill";
const STRUCTURED_OUTPUT: &str = "StructuredOutput";
/// Denying Bash also denies what runs code the way Bash does.
const BASH: &str = "Bash";
const MONITOR: &str = "Monitor";
/// Claude Code's old tool names, still read in rules.
const LEGACY: [(&str, &str); 8] = [
    ("Task", "Agent"),
    ("KillShell", "TaskStop"),
    ("KillBash", "TaskStop"),
    ("ListPeers", "ListAgents"),
    ("Brief", "SendUserMessage"),
    ("ListMcpResources", "ListMcpResourcesTool"),
    ("ReadMcpResource", "ReadMcpResourceTool"),
    ("ReadMcpResourceDir", "ReadMcpResourceDirTool"),
];

fn current_name(name: &str) -> &str {
    LEGACY.iter().find(|(old, _)| *old == name).map_or(name, |(_, new)| new)
}

/// Index of `c` not escaped by an odd run of backslashes, searching from
/// the start or the end.
fn unescaped(s: &str, c: u8, from_end: bool) -> Option<usize> {
    let bytes = s.as_bytes();
    let escaped = |i: usize| bytes[..i].iter().rev().take_while(|b| **b == b'\\').count() % 2 == 1;
    let hit = |&i: &usize| bytes[i] == c && !escaped(i);
    if from_end { (0..bytes.len()).rev().find(hit) } else { (0..bytes.len()).find(hit) }
}

/// Claude Code's `_r`: the tool a rule names (`Bash(git log:*)` → `Bash`);
/// a rule that does not parse as a call names itself.
fn rule_tool(rule: &str) -> &str {
    let (open, close) = (unescaped(rule, b'(', false), unescaped(rule, b')', true));
    let call = match (open, close) {
        (Some(o), Some(c)) if c > o && c == rule.len() - 1 && !rule[..o].contains(['(', ')']) => Some(&rule[..o]),
        _ => None,
    };
    current_name(call.filter(|t| !t.is_empty()).unwrap_or(rule))
}

/// Claude Code's `yr`: `mcp__<server>[__<tool>]`.
struct McpName<'a> {
    server: &'a str,
    tool: Option<String>,
}

fn mcp_name(name: &str) -> Option<McpName<'_>> {
    let mut parts = name.split("__");
    if parts.next() != Some("mcp") {
        return None;
    }
    let server = parts.next().filter(|s| !s.is_empty())?;
    let rest: Vec<&str> = parts.collect();
    Some(McpName { server, tool: (!rest.is_empty()).then(|| rest.join("__")) })
}

/// Claude Code's server-name normalization.
fn normalize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

fn starts_mcp(s: &str) -> bool {
    s.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("mcp__"))
}

/// Claude Code's `jit(entries).isToolDisallowed`, for tools known by name.
struct Matcher {
    names: HashSet<String>,
    servers: HashSet<String>,
    every_server: bool,
}

impl Matcher {
    fn new<'a>(entries: impl IntoIterator<Item = &'a str>) -> Self {
        let mut m = Self { names: HashSet::new(), servers: HashSet::new(), every_server: false };
        for entry in entries {
            let tool = rule_tool(entry);
            m.names.insert(tool.to_owned());
            if tool == BASH {
                m.names.insert(MONITOR.to_owned());
            }
            if let Some(mcp) = mcp_name(tool)
                && mcp.tool.as_deref().is_none_or(|t| t == "*")
            {
                if mcp.server == "*" {
                    m.every_server = true;
                } else {
                    m.servers.insert(mcp.server.to_owned());
                }
            }
        }
        m
    }

    fn denies(&self, tool: &str) -> bool {
        self.names.contains(tool)
            || mcp_name(tool).is_some_and(|m| self.every_server || self.servers.contains(m.server))
    }

    /// Takes every tool of `server` (two made-up tools stand for them all).
    fn denies_server(&self, server: &str) -> bool {
        self.denies(&format!("mcp__{server}__probe_tool_one")) && self.denies(&format!("mcp__{server}__probe_tool_two"))
    }
}

/// Claude Code's `mqo`: how an `mcp__` entry relates to the pool's servers.
#[derive(Debug, PartialEq, Eq)]
enum McpFit {
    /// It takes a server of the pool.
    PoolServer,
    /// It would, spelt this way.
    Slip(String),
    Unmatched,
}

fn mcp_fit(entry: &str, pool: &[String]) -> Option<McpFit> {
    let spelt = if !entry.starts_with("mcp__") && starts_mcp(entry) {
        format!("mcp__{}", &entry[5..])
    } else {
        entry.to_owned()
    };
    let parsed = mcp_name(&spelt)?;
    let as_written = Matcher::new([entry]);
    let mut servers: Vec<&str> = Vec::new();
    for name in pool {
        if let Some(m) = mcp_name(name)
            && !servers.contains(&m.server)
        {
            servers.push(m.server);
        }
    }
    let tool = parsed.tool.as_deref();
    if parsed.server == "*" {
        if !tool.is_none_or(|t| t == "*" || t.is_empty()) || servers.is_empty() {
            return Some(McpFit::Unmatched);
        }
        if as_written.denies_server(servers[0]) {
            return Some(McpFit::PoolServer);
        }
        return Some(McpFit::Slip(if tool == Some("*") { "mcp__*__*" } else { "mcp__*" }.to_owned()));
    }
    let server = normalize(parsed.server);
    let exact = servers.iter().find(|s| **s == server);
    if exact.is_some_and(|s| as_written.denies_server(s)) {
        return Some(McpFit::PoolServer);
    }
    let lower = server.to_lowercase();
    let Some(near) = exact.or_else(|| servers.iter().find(|s| s.to_lowercase() == lower)) else {
        return Some(McpFit::Unmatched);
    };
    let spelling = match tool {
        Some(t) if t != "*" && !t.is_empty() => format!("mcp__{}__{}", normalize(near), normalize(t)),
        Some("*") => format!("mcp__{near}__*"),
        _ => format!("mcp__{near}"),
    };
    let corrected = Matcher::new([spelling.as_str()]);
    if corrected.denies_server(near) || pool.iter().any(|t| corrected.denies(t)) {
        return Some(McpFit::Slip(spelling));
    }
    Some(McpFit::Unmatched)
}

/// Claude Code's `he`: the spelling that names `server` in the pool.
fn server_hint(server: &str, pool: &[String]) -> Option<String> {
    if server.is_empty() {
        return None;
    }
    let entry = format!("mcp__{server}");
    match mcp_fit(&entry, pool)? {
        McpFit::PoolServer => Some(entry),
        McpFit::Slip(spelling) => Some(spelling),
        McpFit::Unmatched => None,
    }
}

const UN_NARROWED: &str = "Refusing the spawn rather than running it un-narrowed.";
const DROPPED: &str = "Refusing the spawn rather than running it with this deny silently dropped.";

/// Claude Code's spawn-time check of `disallowedTools` against `pool`, the
/// session's tool names. Entries that take nothing and look like no tool
/// either pass (the deny is a no-op) or are refused with Claude Code's words.
pub(crate) fn check(entries: &[String], pool: &[String], schema_mode: bool) -> Result<(), String> {
    for entry in entries {
        if pool.iter().any(|t| Matcher::new([entry.as_str()]).denies(t)) {
            continue;
        }
        let tool = rule_tool(entry);
        let trimmed = tool.trim();
        if tool != trimmed || trimmed.contains(|c: char| c.is_whitespace() || c == '(' || c == ')') {
            return Err(format!(
                "agent() opts.disallowedTools entry '{entry}' parses to tool name '{tool}', which can never match a \
                 tool \u{2014} check for a space before the rule parens or an unbalanced paren. {UN_NARROWED}"
            ));
        }
        let cannot = format!("agent() opts.disallowedTools entry '{entry}' cannot match any tool \u{2014} ");
        if starts_mcp(tool) {
            let spelt = format!("mcp__{}", &tool[5..]);
            let Some(mcp) = mcp_name(&spelt) else {
                return Err(format!(
                    "{cannot}its server segment is empty, so it names no server. Use 'mcp__<server>' to deny one \
                     server's tools ('mcp__<server>__<tool>' for one tool), or 'mcp__*' to deny every MCP server's \
                     tools. {UN_NARROWED}"
                ));
            };
            if mcp.server.contains('*') && mcp.server != "*" {
                let hint = server_hint(&mcp.server.replace('*', ""), pool)
                    .map(|h| format!("'{h}' for that server's tools, or "))
                    .unwrap_or_default();
                return Err(format!(
                    "{cannot}server names take no wildcard. Use {hint}'mcp__*' for every MCP server's tools. \
                     {UN_NARROWED}"
                ));
            }
            if let Some(t) = &mcp.tool
                && t != "*"
                && t.contains('*')
            {
                let hint = server_hint(mcp.server, pool).map_or_else(
                    || "Enumerate exact tool names, or use 'mcp__*' to deny every MCP server's tools. ".to_owned(),
                    |h| format!("Use '{h}' to deny all of that server's tools, or enumerate exact tool names. "),
                );
                return Err(format!("{cannot}tool names take no wildcard. {hint}{UN_NARROWED}"));
            }
        }
        if tool.contains('*') && mcp_name(tool).is_none() && !starts_mcp(tool) {
            let hint = if tool.to_lowercase().starts_with("skill__") {
                format!("Use '{SKILL}' to deny every skill. ")
            } else {
                "Enumerate the tools to deny (e.g. ['Bash', 'Write']), or use 'mcp__*' to deny every MCP server's \
                 tools. "
                    .to_owned()
            };
            return Err(format!("{cannot}'*' is not a deny wildcard outside mcp__ server specs. {hint}{UN_NARROWED}"));
        }
        if tool == SKILL || (schema_mode && tool == STRUCTURED_OUTPUT) {
            continue;
        }
        let parsed = if tool == entry { String::new() } else { format!(" (parsed tool name '{tool}')") };
        let lower = tool.to_lowercase();
        let near = pool.iter().map(String::as_str).find(|t| t.to_lowercase() == lower).or_else(|| {
            if lower == SKILL.to_lowercase() {
                Some(SKILL)
            } else if schema_mode && lower == STRUCTURED_OUTPUT.to_lowercase() {
                Some(STRUCTURED_OUTPUT)
            } else {
                None
            }
        });
        if let Some(near) = near {
            return Err(format!(
                "agent() opts.disallowedTools entry '{entry}'{parsed} does not match any tool, but '{near}' exists \
                 \u{2014} tool names are case-sensitive. {DROPPED}"
            ));
        }
        if let Some(McpFit::Slip(spelling)) = mcp_fit(tool, pool) {
            return Err(format!(
                "agent() opts.disallowedTools entry '{entry}'{parsed} cannot match as written, but '{spelling}' \
                 would \u{2014} deny entries must match the normalized server spelling exactly, mcp__ prefix and \
                 case included. {DROPPED}"
            ));
        }
    }
    Ok(())
}

/// Schema mode needs StructuredOutput; Claude Code refuses a spawn whose
/// merged denies take it.
pub(crate) fn takes_structured_output<'a>(denies: impl IntoIterator<Item = &'a str>) -> bool {
    Matcher::new(denies).denies(STRUCTURED_OUTPUT)
}

/// The entries as the host's deny list reads them: a rule's tool, a whole
/// server as `mcp__<server>` (every server as `mcp__*`), Bash with Monitor.
pub(crate) fn host_denies(entries: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for entry in entries {
        let tool = rule_tool(entry);
        match mcp_name(tool) {
            Some(m) if m.tool.as_deref().is_none_or(|t| t == "*") => {
                out.push(if m.server == "*" { "mcp__*".to_owned() } else { format!("mcp__{}", m.server) });
            }
            _ => out.push(tool.to_owned()),
        }
        if tool == BASH {
            out.push(MONITOR.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> Vec<String> {
        ["Bash", "Monitor", "Read", "Write", "WebFetch", "mcp__github__create_issue", "mcp__github__list_prs"]
            .map(String::from)
            .to_vec()
    }

    fn refused(entry: &str) -> String {
        check(&[entry.to_owned()], &pool(), false).expect_err(entry)
    }

    #[test]
    fn rules_name_their_tool() {
        assert_eq!(rule_tool("Bash(git log:*)"), "Bash");
        assert_eq!(rule_tool("Bash()"), "Bash");
        assert_eq!(rule_tool("Task"), "Agent");
        assert_eq!(rule_tool("Bash (x)"), "Bash ");
        assert_eq!(rule_tool("Ba(sh(x)"), "Ba");
        assert_eq!(rule_tool("Bash(x"), "Bash(x");
        assert_eq!(rule_tool(r"Bash\(x\)"), r"Bash\(x\)");
    }

    #[test]
    fn entries_that_take_a_tool_pass() {
        let ok = ["Bash", "Bash(rm:*)", "Write", "mcp__github", "mcp__github__*", "mcp__*", "mcp__github__list_prs"];
        assert!(check(&ok.map(String::from), &pool(), false).is_ok());
        // No-ops: nothing like them in the pool.
        assert!(check(&["NotATool".into(), "mcp__slack".into(), "Skill".into()], &pool(), false).is_ok());
        assert!(check(&["StructuredOutput".into()], &pool(), true).is_ok());
    }

    #[test]
    fn misspelt_entries_are_refused_in_claude_codes_words() {
        assert_eq!(
            refused("bash"),
            "agent() opts.disallowedTools entry 'bash' does not match any tool, but 'Bash' exists \u{2014} tool names \
             are case-sensitive. Refusing the spawn rather than running it with this deny silently dropped."
        );
        assert!(refused("write(x)").contains("entry 'write(x)' (parsed tool name 'write') does not match"));
        assert!(refused("skill").contains("but 'Skill' exists"));
        assert_eq!(
            refused("MCP__github"),
            "agent() opts.disallowedTools entry 'MCP__github' cannot match as written, but 'mcp__github' would \
             \u{2014} deny entries must match the normalized server spelling exactly, mcp__ prefix and case \
             included. Refusing the spawn rather than running it with this deny silently dropped."
        );
        assert!(refused("mcp__GitHub__*").contains("but 'mcp__github__*' would"));
        assert!(refused("MCP__*").contains("but 'mcp__*' would"));
        assert!(refused("Bash (rm)").contains("parses to tool name 'Bash ', which can never match a tool"));
        assert!(refused("mcp__").contains("its server segment is empty"));
        assert!(refused("mcp__github*").contains("server names take no wildcard. Use 'mcp__github' for that server's"));
        assert!(refused("mcp__git*").contains("server names take no wildcard. Use 'mcp__*' for every MCP server's"));
        assert!(refused("mcp__github__list_*").contains("Use 'mcp__github' to deny all of that server's tools"));
        assert!(refused("Web*").contains("'*' is not a deny wildcard outside mcp__ server specs. Enumerate"));
        assert!(refused("skill__x*").contains("Use 'Skill' to deny every skill."));
    }

    #[test]
    fn hosts_get_plain_names() {
        let entries = ["Bash(rm:*)", "mcp__github__*", "mcp__*__*", "Task", "Read"].map(String::from);
        assert_eq!(host_denies(&entries), ["Bash", "Monitor", "mcp__github", "mcp__*", "Agent", "Read"]);
        assert!(takes_structured_output(["Agent", "StructuredOutput(x)"]));
        assert!(!takes_structured_output(["Agent", "mcp__*"]));
    }
}
