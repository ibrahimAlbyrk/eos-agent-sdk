//! Building a fork's first turn. A fork's conversation is the parent's, then
//! the parent's last assistant message (the one holding the Agent call), then
//! one user message: a `tool_result` for every `tool_use` in that message and
//! the directive. Same system prompt and prefix as the parent, so the whole
//! prompt cache is reused.

use eos_agent_types::{ContentBlock, ToolResultContent};

/// Marks an agent that is itself a fork (the host inserts it into the fork's
/// `Extensions`); a fork may not fork again.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForkedAgent;

const FORK_IS_YOU: &str = "Fork started — that fork is you.";
const CALL_STARTED_FORK: &str =
    "This call started a fork, and that fork is you. The call's other work and its result belong to the parent.";
const PARENTS_CALL: &str =
    "This is the parent's call, not yours. It may not have run yet, and its result goes to the parent.";

/// The directive with the rules a fork works under.
pub fn fork_directive(prompt: &str) -> String {
    format!(
        "<fork-boilerplate>\nYou are a worker fork. The transcript above is the parent's history — inherited \
         reference, not your situation. You are NOT a continuation of that agent. Execute ONE directive, then \
         stop.\n\nHard rules:\n- Do NOT spawn subagents with the Agent tool. The \"default to forking\" guidance is \
         for the parent; you ARE the fork, execute directly.\n- One shot: report once and stop. No follow-up \
         questions, no proposed next steps, no waiting for the user.\n\nGuidelines (your directive may override any \
         of these):\n- Stay in scope. Other forks may be handling adjacent work; if you spot something outside your \
         directive, note it in a sentence and move on.\n- Open with one line restating your task, so the parent can \
         spot scope drift at a glance.\n- Be concise — as short as the answer allows, no shorter. Plain text, no \
         preamble, no meta-commentary.\n- If you committed changes, list the paths and commit hashes in your \
         report.\n</fork-boilerplate>\n\nYour directive: {prompt}"
    )
}

/// The user message that opens a fork, given the parent's last assistant
/// content and the id of the Agent call that started this fork.
pub fn fork_seed(parent_assistant: &[ContentBlock], fork_tool_use_id: &str, prompt: &str) -> Vec<ContentBlock> {
    let calls: Vec<(&str, &str)> = parent_assistant
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    let own = if calls.len() == 1 { calls[0].0 } else { fork_tool_use_id };
    let mut out: Vec<ContentBlock> = calls
        .iter()
        .map(|&(id, name)| {
            let text = if id != own {
                PARENTS_CALL
            } else if name == "Agent" || name == "Task" {
                FORK_IS_YOU
            } else {
                CALL_STARTED_FORK
            };
            ContentBlock::ToolResult {
                tool_use_id: id.to_owned(),
                content: Some(ToolResultContent::Blocks(vec![text_block(text)])),
                is_error: false,
            }
        })
        .collect();
    out.push(text_block(&fork_directive(prompt)));
    out
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text { text: text.to_owned(), citations: None }
}

#[cfg(test)]
mod tests {
    use serde_json::value::RawValue;

    use super::*;

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        let input = RawValue::from_string("{}".into()).expect("json");
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input }
    }

    fn result_text(block: &ContentBlock) -> (&str, &str) {
        match block {
            ContentBlock::ToolResult { tool_use_id, content: Some(ToolResultContent::Blocks(b)), .. } => match &b[0] {
                ContentBlock::Text { text, .. } => (tool_use_id.as_str(), text.as_str()),
                _ => ("", ""),
            },
            _ => ("", ""),
        }
    }

    #[test]
    fn seed_answers_every_call() {
        let parent = [text_block("Forking."), tool_use("t1", "Agent"), tool_use("t2", "Agent"), tool_use("t3", "Read")];
        let seed = fork_seed(&parent, "t2", "Audit the branch.");
        assert_eq!(seed.len(), 4);
        assert_eq!(result_text(&seed[0]), ("t1", PARENTS_CALL));
        assert_eq!(result_text(&seed[1]), ("t2", FORK_IS_YOU));
        assert_eq!(result_text(&seed[2]), ("t3", PARENTS_CALL));
        let ContentBlock::Text { text, .. } = &seed[3] else { panic!("directive last") };
        assert!(text.starts_with("<fork-boilerplate>\nYou are a worker fork."));
        assert!(text.ends_with("</fork-boilerplate>\n\nYour directive: Audit the branch."));
    }
}
