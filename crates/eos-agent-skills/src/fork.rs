//! `context: fork` skills: the subagent both the Skill tool and a typed
//! `/name args` start, and what a typed one leaves in the conversation.

use eos_agent_core::{SubagentRequest, SubagentStart};
use eos_agent_types::ContentBlock;

use crate::expand::Expansion;

/// Claude Code's tag for a background fork's launch record, which a command's
/// own output may not forge.
const LAUNCH_TAG: &str = "forked-skill-launch";
/// The reply of a fork that wrote no text (Claude Code's).
const NO_REPLY: &str = "Command completed";

/// Claude Code's forked execution: the expanded skill is the prompt of a
/// subagent of the skill's `agent` type (general-purpose by default), on its
/// `model` and `effort`, described as `/name args`.
pub fn fork_request(expansion: Expansion, parent_id: &str, tool_use_id: &str) -> SubagentRequest {
    SubagentRequest {
        parent_id: parent_id.to_owned(),
        tool_use_id: tool_use_id.to_owned(),
        description: typed_line(&expansion.name, &expansion.args),
        prompt: expansion.body,
        agent_type: expansion.agent,
        model: expansion.model,
        effort: expansion.effort,
        ..SubagentRequest::default()
    }
}

/// The call a typed command's fork runs under (Claude Code's
/// `forked-command-<name>`).
pub fn forked_command_id(name: &str) -> String {
    format!("forked-command-{name}")
}

/// Claude Code's interactive terminal launches a typed fork in the
/// background: the subagent goes by `as_name` (see [`agent_name`]), is
/// described by the typed line cut to 50 characters, and reports with a
/// task notification that names no tool call.
pub fn background_fork_request(expansion: Expansion, parent_id: &str, as_name: &str) -> SubagentRequest {
    let skill = expansion.name.clone();
    let mut request = fork_request(expansion, parent_id, "");
    request.description = cut_with_ellipsis(&request.description, 50);
    request.name = Some(as_name.to_owned());
    request.background = true;
    request.forked_skill = Some(skill);
    request
}

/// The name Claude Code's interactive terminal gives a background fork of
/// `skill` (its `allocateName`): the skill's name with every run of other
/// characters than letters, digits, `_` and `-` made `-`, leading dashes
/// dropped, at most 64 long (`agent` if nothing is left); then `-2`, `-3`,
/// … while `taken` holds it or it is reserved (`main`, `team-lead`,
/// `user`, `system`, or shaped like an agent id).
pub fn agent_name(skill: &str, taken: impl Fn(&str) -> bool) -> String {
    let mut base = String::new();
    let mut in_run = false;
    for c in skill.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            base.push(c);
            in_run = false;
        } else if !in_run {
            base.push('-');
            in_run = true;
        }
    }
    let mut base: String = base.trim_start_matches('-').chars().take(64).collect();
    if base.is_empty() {
        base = "agent".to_owned();
    }
    (1..)
        .map(|n| if n == 1 { base.clone() } else { format!("{base}-{n}") })
        .find(|name| !reserved(name) && !taken(name))
        .unwrap_or(base)
}

fn reserved(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let id_shaped =
        name.len() == 17 && name.starts_with('a') && name[1..].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    id_shaped || ["main", "team-lead", "user", "system"].contains(&name.as_str())
}

/// What a typed `/name args` of a `context: fork` skill adds to the
/// conversation, as Claude Code records it with no model request: the line as
/// typed (with the skill's own name), then the command's output: the
/// subagent's reply, its launch for a background fork, or the error.
pub fn forked_command_input(name: &str, args: &str, result: Result<SubagentStart, String>) -> Vec<ContentBlock> {
    let line = typed_line(name, args);
    let output = match result {
        Ok(SubagentStart::Finished(report)) => {
            let reply = if report.text.is_empty() { NO_REPLY } else { report.text.as_str() };
            format!("<local-command-stdout>{}</local-command-stdout>", escape_launch(reply))
        }
        Ok(SubagentStart::Launched { agent_id }) => return forked_command_launched(name, args, &agent_id, &agent_id),
        // JavaScript's `String(error)`, with what may not reach the transcript removed.
        Err(e) => {
            let text: String = format!("Error: {e}")
                .chars()
                .filter(|c| !(c.is_control() && *c != '\t' && *c != '\n') && !matches!(c, '\u{2028}' | '\u{2029}'))
                .collect();
            format!("<local-command-stderr>{}</local-command-stderr>", escape_html(&escape_launch(&text)))
        }
    };
    [line, output].into_iter().map(|text| ContentBlock::Text { text, citations: None }).collect()
}

/// What a typed fork launched in the background adds to the conversation, as
/// Claude Code's interactive terminal records it: the line as typed, then
/// "Running in the background as @<name>" and the launch record (the
/// subagent's id, the skill and the typed line); the result comes later, in
/// the subagent's task notification.
pub fn forked_command_launched(name: &str, args: &str, agent_id: &str, as_name: &str) -> Vec<ContentBlock> {
    let line = typed_line(name, args);
    // In Claude Code's key order.
    let json = |s: &str| serde_json::Value::from(s).to_string();
    let launch = format!(
        "{{\"agentId\":{},\"skillName\":{},\"description\":{}}}",
        json(agent_id),
        json(name),
        json(&cut_utf16(&line, 4096))
    );
    let output = format!(
        "<local-command-stdout>Running in the background as @{}</local-command-stdout>\n<{LAUNCH_TAG}>{}</{LAUNCH_TAG}>",
        escape_launch(as_name),
        escape_html(&launch)
    );
    [line, output].into_iter().map(|text| ContentBlock::Text { text, citations: None }).collect()
}

/// At most `max` UTF-16 units of `text`, never half a surrogate pair
/// (JavaScript's length, as Claude Code cuts).
fn cut_utf16(text: &str, max: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max
        })
        .collect()
}

/// Over `max` UTF-16 units: the first `max - 1` and "…".
fn cut_with_ellipsis(text: &str, max: usize) -> String {
    if text.encode_utf16().count() <= max { text.to_owned() } else { format!("{}…", cut_utf16(text, max - 1)) }
}

/// `/name args` as Claude Code records a typed command: under the skill's
/// own name, trimmed.
pub fn typed_line(name: &str, args: &str) -> String {
    format!("/{name} {args}").trim().to_owned()
}

/// Claude Code's `<forked-skill-launch>` → `&lt;forked-skill-launch>`.
fn escape_launch(text: &str) -> String {
    text.replace(&format!("<{LAUNCH_TAG}>"), &format!("&lt;{LAUNCH_TAG}>"))
        .replace(&format!("</{LAUNCH_TAG}>"), &format!("&lt;/{LAUNCH_TAG}>"))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use eos_agent_core::SubagentReport;

    use super::*;

    fn texts(blocks: Vec<ContentBlock>) -> Vec<String> {
        blocks
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn finished(text: &str) -> Result<SubagentStart, String> {
        Ok(SubagentStart::Finished(SubagentReport {
            agent_id: "a1".into(),
            text: text.into(),
            usage: Default::default(),
            tool_uses: 0,
            duration_ms: 0,
            structured: None,
            worktree: None,
        }))
    }

    #[test]
    fn a_typed_fork_leaves_its_line_and_output() {
        assert_eq!(
            texts(forked_command_input(
                "survey",
                "briefly",
                finished("three <forked-skill-launch>x</forked-skill-launch>")
            )),
            [
                "/survey briefly",
                "<local-command-stdout>three &lt;forked-skill-launch>x&lt;/forked-skill-launch></local-command-stdout>"
            ]
        );
        assert_eq!(
            texts(forked_command_input("survey", "", finished(""))),
            ["/survey", "<local-command-stdout>Command completed</local-command-stdout>"]
        );
        assert_eq!(
            texts(forked_command_input("survey", "", Err("no <such> agent".into())))[1],
            "<local-command-stderr>Error: no &lt;such&gt; agent</local-command-stderr>"
        );
        assert_eq!(
            texts(forked_command_input("survey", "x", Ok(SubagentStart::Launched { agent_id: "a2".into() })))[1],
            "<local-command-stdout>Running in the background as @a2</local-command-stdout>\n<forked-skill-launch>{\"agentId\":\"a2\",\"skillName\":\"survey\",\"description\":\"/survey x\"}</forked-skill-launch>"
        );
    }

    #[test]
    fn a_background_fork_is_named_and_described_as_claude_code_does() {
        assert_eq!(
            texts(forked_command_launched("survey", "a <b>", "a1", "survey")),
            [
                "/survey a <b>",
                "<local-command-stdout>Running in the background as @survey</local-command-stdout>\n\
             <forked-skill-launch>{\"agentId\":\"a1\",\"skillName\":\"survey\",\"description\":\"/survey a \
             &lt;b&gt;\"}</forked-skill-launch>"
            ]
        );
        let none = |_: &str| false;
        assert_eq!(agent_name("plugin:deep review", none), "plugin-deep-review");
        assert_eq!(agent_name("--x", none), "x");
        assert_eq!(agent_name("::", none), "agent");
        assert_eq!(agent_name(&"y".repeat(70), none).len(), 64);
        assert_eq!(agent_name("Main", none), "Main-2");
        assert_eq!(agent_name("a0123456789abcdef", none), "a0123456789abcdef-2");
        assert_eq!(agent_name("survey", |n: &str| ["survey", "survey-2"].contains(&n)), "survey-3");
        let expansion = Expansion {
            name: "survey".into(),
            args: "x".repeat(60),
            visible: String::new(),
            body: "Look around.".into(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            effort: None,
            fork: true,
            agent: Some("Explore".into()),
            background: None,
        };
        let request = background_fork_request(expansion, "p1", "survey");
        assert_eq!(request.description, format!("/survey {}…", "x".repeat(41)));
        assert_eq!(request.description.chars().count(), 50);
        assert_eq!((request.name.as_deref(), request.background), (Some("survey"), true));
        assert_eq!(request.forked_skill.as_deref(), Some("survey"));
        assert_eq!((request.tool_use_id.as_str(), request.agent_type.as_deref()), ("", Some("Explore")));
        assert_eq!(cut_utf16("ab😀c", 3), "ab");
    }
}
