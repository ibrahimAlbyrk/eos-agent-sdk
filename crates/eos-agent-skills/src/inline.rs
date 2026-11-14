//! Inline shell commands in a skill body (`` !`cmd` `` and ```` ```! ````
//! blocks), run as the agent's shell tool calls before the body is sent.

use eos_agent_core::{BoxFuture, ToolContext};

use crate::expand::ExpandError;

/// Why an inline command produced no output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellFailure {
    /// The permission check did not allow it (the reason, if any).
    Denied(String),
    /// It ran and failed, or could not run (its output or the error).
    Failed(String),
}

/// Runs the `` !`cmd` `` snippets of a skill body. The shell tool lives in
/// another crate; the host passes an adapter over it (with its permission
/// checks). `Ok` is the text that replaces the snippet.
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// `call` is the agent the skill is loaded for, when there is one.
    fn run<'a>(
        &'a self,
        command: &'a str,
        call: Option<ToolContext<'a>>,
    ) -> BoxFuture<'a, Result<String, ShellFailure>>;
}

/// Refuses every inline command.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCommands;

impl CommandRunner for NoCommands {
    fn run<'a>(&'a self, _: &'a str, _: Option<ToolContext<'a>>) -> BoxFuture<'a, Result<String, ShellFailure>> {
        Box::pin(async { Err(ShellFailure::Failed("inline shell commands are not available in this session".into())) })
    }
}

/// One `` !`cmd` `` or ```` ```! ```` snippet: its byte range and command.
#[derive(Debug, PartialEq)]
struct Snippet {
    start: usize,
    end: usize,
    command: String,
}

pub(crate) async fn run_inline(
    text: String,
    runner: &dyn CommandRunner,
    call: Option<ToolContext<'_>>,
) -> Result<String, ExpandError> {
    let snippets = find_snippets(&text);
    if snippets.is_empty() {
        return Ok(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for s in snippets {
        let pattern = || text[s.start..s.end].to_owned();
        let output = runner.run(&s.command, call).await.map_err(|failure| match failure {
            ShellFailure::Denied(message) => ExpandError::ShellDenied { pattern: pattern(), message },
            ShellFailure::Failed(message) => ExpandError::Shell { pattern: pattern(), message },
        })?;
        out.push_str(&text[last..s.start]);
        out.push_str(&output);
        last = s.end;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

fn find_snippets(text: &str) -> Vec<Snippet> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("```!").map(|i| i + from) {
        let body_start = at + 4 + (text[at + 4..].len() - text[at + 4..].trim_start().len());
        let Some(close) = text[body_start..].find("```").map(|i| i + body_start) else { break };
        let command = text[body_start..close].strip_suffix('\n').unwrap_or(&text[body_start..close]);
        push(&mut found, at, close + 3, command);
        from = close + 3;
    }
    if text.contains("!`") {
        let masked = mask_code_spans(text);
        let bytes = masked.as_bytes();
        let mut i = 0;
        while let Some(at) = masked[i..].find("!`").map(|p| p + i) {
            i = at + 2;
            let after_space = at == 0 || masked[..at].chars().next_back().is_some_and(char::is_whitespace);
            let inside_block = found.iter().any(|s| at >= s.start && at < s.end);
            let Some(close) = masked[at + 2..].find('`').map(|p| p + at + 2) else { break };
            if !after_space || inside_block || close == at + 2 || bytes[at + 2] == b'`' {
                continue;
            }
            push(&mut found, at, close + 1, &text[at + 2..close]);
            i = close + 1;
        }
    }
    found.sort_by_key(|s| s.start);
    found
}

fn push(found: &mut Vec<Snippet>, start: usize, end: usize, command: &str) {
    let command = command.trim().replace("\r\n", "\n");
    if !command.is_empty() {
        found.push(Snippet { start, end, command });
    }
}

/// Inline code spans (`` `x` ``) not opened by `!` are blanked out, so an
/// example like `` `!`date`` `` in prose is not mistaken for a command.
fn mask_code_spans(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let close = text[i + 1..].find(['`', '\n']).map(|p| p + i + 1);
            if let Some(close) = close.filter(|&c| bytes[c] == b'`' && c > i + 1) {
                let opener = i.checked_sub(1).map(|p| bytes[p]);
                if opener != Some(b'!') && opener != Some(b'`') {
                    out.push_str(&text[last..=i]);
                    out.extend(text[i + 1..close].chars().flat_map(|c| std::iter::repeat_n(' ', c.len_utf8())));
                    last = close;
                }
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_inline_and_block_commands() {
        let text = "Status: !`git status`\nIn prose `!`date`` is an example.\n```!\nls -la\n```\nend";
        let found = find_snippets(text);
        let commands: Vec<&str> = found.iter().map(|s| s.command.as_str()).collect();
        assert_eq!(commands, ["git status", "ls -la"]);
        assert_eq!(&text[found[0].start..found[0].end], "!`git status`");
    }

    #[derive(Debug)]
    struct Echo;
    impl CommandRunner for Echo {
        fn run<'a>(
            &'a self,
            command: &'a str,
            _: Option<ToolContext<'a>>,
        ) -> BoxFuture<'a, Result<String, ShellFailure>> {
            Box::pin(async move { Ok(format!("<{command}>")) })
        }
    }

    #[tokio::test]
    async fn replaces_snippets_with_output() {
        let out = run_inline("a !`one` b\n```!\ntwo\n```".into(), &Echo, None).await.expect("runs");
        assert_eq!(out, "a <one> b\n<two>");
        let err = run_inline("x !`date`".into(), &NoCommands, None).await.expect_err("refused");
        assert!(err.to_string().starts_with("Shell command failed for pattern \"!`date`\""));
    }
}
