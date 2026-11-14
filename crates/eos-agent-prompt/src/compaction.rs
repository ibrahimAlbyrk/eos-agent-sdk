//! Claude Code's summarizing words: the instruction with the user's focus
//! placed before its closing reminder, and the summary turned into the
//! message that continues the conversation.

use std::sync::Arc;

use eos_agent_core::CompactionText;

use crate::library::{Fragment, Prompts};

/// Where the instruction's closing reminder starts; the focus goes before it.
const REMINDER_START: &str = "\n\nREMINDER:";

#[derive(Debug, Clone)]
pub struct ClaudeCodeCompaction {
    /// The instruction up to its closing reminder.
    head: Arc<str>,
    reminder: Arc<str>,
    seed_intro: Arc<str>,
    resume: Arc<str>,
}

impl ClaudeCodeCompaction {
    pub fn new(prompts: &Prompts) -> Self {
        let prompt = prompts.get(Fragment::Compaction);
        let (head, reminder) = prompt.rfind(REMINDER_START).map_or((prompt, ""), |at| prompt.split_at(at));
        Self {
            head: head.into(),
            reminder: reminder.into(),
            seed_intro: prompts.get(Fragment::CompactionSeed).into(),
            resume: prompts.get(Fragment::CompactionResume).into(),
        }
    }
}

impl CompactionText for ClaudeCodeCompaction {
    fn instruction(&self, focus: Option<&str>) -> String {
        let mut out = String::with_capacity(self.head.len() + self.reminder.len() + 64);
        out.push_str(&self.head);
        if let Some(focus) = focus.filter(|f| !f.trim().is_empty()) {
            out.push_str("\n\nAdditional Instructions:\n");
            out.push_str(focus);
        }
        out.push_str(&self.reminder);
        out
    }

    fn seed(&self, summary: &str) -> String {
        format!("{}\n\n{}\n{}", self.seed_intro, formatted_summary(summary), self.resume)
    }
}

/// The reply without its `<analysis>` and with `<summary>…</summary>` written
/// as `Summary:`, blank-line runs collapsed, trimmed.
fn formatted_summary(reply: &str) -> String {
    let mut text = reply.to_owned();
    if let Some((start, end)) = span(&text, "<analysis>", "</analysis>") {
        text.replace_range(start..end, "");
    }
    if let Some((start, end)) = span(&text, "<summary>", "</summary>") {
        let inner = text[start + "<summary>".len()..end - "</summary>".len()].trim().to_owned();
        text.replace_range(start..end, &format!("Summary:\n{inner}"));
    }
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0;
    for c in text.chars() {
        if c == '\n' {
            newlines += 1;
            continue;
        }
        out.push_str(if newlines >= 2 {
            "\n\n"
        } else if newlines == 1 {
            "\n"
        } else {
            ""
        });
        newlines = 0;
        out.push(c);
    }
    out.trim().to_owned()
}

/// The byte range of the first `open … close`, tags included.
fn span(text: &str, open: &str, close: &str) -> Option<(usize, usize)> {
    let start = text.find(open)?;
    let end = text[start + open.len()..].find(close)? + start + open.len() + close.len();
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_reads_like_claude_code() {
        let reply = "<analysis>\nthinking\n</analysis>\n\n\n<summary>\n1. Primary Request:\n   Fix it\n\n\n\n2. Next\n</summary>";
        assert_eq!(formatted_summary(reply), "Summary:\n1. Primary Request:\n   Fix it\n\n2. Next");
        assert_eq!(formatted_summary("  plain words  "), "plain words");
    }
}
