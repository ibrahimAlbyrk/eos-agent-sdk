//! The `<task-notification>` Claude Code delivers to the parent (as a user
//! turn) when a background subagent stops.

use std::path::Path;

use eos_agent_core::SubagentReport;

use crate::agent_tool::total_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedBy<'a> {
    /// The parent agent stopped it (TaskStop).
    Parent,
    User,
    Error(&'a str),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome<'a> {
    Completed,
    Failed { error: &'a str },
    Stopped(StoppedBy<'a>),
}

#[derive(Debug, Clone, Copy)]
pub struct AgentNotification<'a> {
    pub agent_id: &'a str,
    /// The Agent call that launched it.
    pub tool_use_id: Option<&'a str>,
    pub output_file: Option<&'a Path>,
    /// The Agent call's `description`.
    pub description: &'a str,
    pub outcome: Outcome<'a>,
    /// The agent's last reply (also sent, when it has one, for a stopped or failed agent).
    pub result: Option<&'a str>,
    /// `(tokens, tool uses, duration ms)`.
    pub usage: Option<(u64, u32, u64)>,
    /// It finished with background work of its own still running (a
    /// command, a monitor), which may wake it again.
    pub waiting: bool,
}

/// Claude Code's note for an agent that finished while its own background
/// work runs on.
const WAITING_NOTE: &str = "This agent stopped with background work of its own still running. It may resume on its \
                            own when that work completes or reports, and the same task-id notifies again if it does; \
                            the result below may be interim.";

const NOTE: &str = "A task-notification fires each time this agent stops with no live background children of its own. \
                    The user can send it another message and resume it, so the same task-id may notify more than once.";

impl<'a> AgentNotification<'a> {
    /// A finished agent's notification, from its report.
    pub fn completed(report: &'a SubagentReport, description: &'a str, tool_use_id: Option<&'a str>) -> Self {
        Self {
            agent_id: &report.agent_id,
            tool_use_id,
            output_file: None,
            description,
            outcome: Outcome::Completed,
            result: Some(&report.text),
            usage: Some((total_tokens(&report.usage), report.tool_uses, report.duration_ms)),
            waiting: false,
        }
    }

    pub fn render(&self) -> String {
        let waiting = self.waiting && self.outcome == Outcome::Completed;
        let (status, summary) = match self.outcome {
            Outcome::Completed if waiting => ("completed", "is waiting on its own background work".to_owned()),
            Outcome::Completed => ("completed", "finished".to_owned()),
            Outcome::Failed { error } => {
                ("failed", format!("failed: {}", if error.is_empty() { "Unknown error" } else { error }))
            }
            Outcome::Stopped(by) => (
                "killed",
                match by {
                    StoppedBy::Parent => "was stopped by Claude".to_owned(),
                    StoppedBy::User => "was stopped by user".to_owned(),
                    StoppedBy::Error(e) => format!("was stopped: {e}"),
                    StoppedBy::Unknown => "was stopped".to_owned(),
                },
            ),
        };
        let mut out = format!("<task-notification>\n<task-id>{}</task-id>", escape(self.agent_id));
        if let Some(id) = self.tool_use_id {
            out += &format!("\n<tool-use-id>{}</tool-use-id>", escape(id));
        }
        if let Some(file) = self.output_file {
            out += &format!("\n<output-file>{}</output-file>", escape(&file.to_string_lossy()));
        }
        out += &format!("\n<status>{status}</status>");
        out += &format!("\n<summary>{}</summary>", escape(&format!("Agent \"{}\" {summary}", self.description)));
        out += &format!("\n<note>{}</note>", if waiting { WAITING_NOTE } else { NOTE });
        if let Some(result) = self.result.filter(|r| !r.is_empty()) {
            out += &format!("\n<result>{}</result>", escape(result));
        }
        if let Some((tokens, tool_uses, duration_ms)) = self.usage {
            out += &format!(
                "\n<usage><subagent_tokens>{tokens}</subagent_tokens><tool_uses>{tool_uses}</tool_uses>\
                 <duration_ms>{duration_ms}</duration_ms></usage>"
            );
        }
        out += "\n</task-notification>";
        out
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use eos_agent_types::Usage;

    use super::*;

    #[test]
    fn completed_matches_claude_code() {
        let report = SubagentReport {
            agent_id: "a97c4462489d0fdb6".into(),
            text: "Found it in `a<b>.rs` & more.".into(),
            usage: Usage { input_tokens: Some(100), output_tokens: Some(26), ..Usage::default() },
            tool_uses: 84,
            duration_ms: 401_930,
            structured: None,
            worktree: None,
        };
        let mut n = AgentNotification::completed(&report, "Map input switch plumbing", Some("toolu_01US"));
        let out_file = Path::new("/tmp/tasks/a97c4462489d0fdb6.output");
        n.output_file = Some(out_file);
        assert_eq!(
            n.render(),
            "<task-notification>\n<task-id>a97c4462489d0fdb6</task-id>\n<tool-use-id>toolu_01US</tool-use-id>\n\
             <output-file>/tmp/tasks/a97c4462489d0fdb6.output</output-file>\n<status>completed</status>\n\
             <summary>Agent \"Map input switch plumbing\" finished</summary>\n<note>A task-notification fires each \
             time this agent stops with no live background children of its own. The user can send it another message \
             and resume it, so the same task-id may notify more than once.</note>\n<result>Found it in `a&lt;b&gt;.rs` \
             &amp; more.</result>\n<usage><subagent_tokens>126</subagent_tokens><tool_uses>84</tool_uses>\
             <duration_ms>401930</duration_ms></usage>\n</task-notification>"
        );
    }

    #[test]
    fn failed_and_stopped() {
        let failed = AgentNotification {
            agent_id: "a1",
            tool_use_id: None,
            output_file: None,
            description: "Review",
            outcome: Outcome::Failed { error: "Agent stalled" },
            result: None,
            usage: None,
            waiting: false,
        };
        assert_eq!(
            failed.render(),
            format!(
                "<task-notification>\n<task-id>a1</task-id>\n<status>failed</status>\n<summary>Agent \"Review\" failed: \
                 Agent stalled</summary>\n<note>{NOTE}</note>\n</task-notification>"
            )
        );
        let stopped = AgentNotification { outcome: Outcome::Stopped(StoppedBy::Parent), ..failed };
        let text = stopped.render();
        assert!(text.contains("<status>killed</status>\n<summary>Agent \"Review\" was stopped by Claude</summary>"));
        // Only a finished agent is said to wait on its own work.
        assert!(AgentNotification { waiting: true, ..stopped }.render().contains(NOTE));
    }

    #[test]
    fn finished_with_work_still_running() {
        let waiting = AgentNotification {
            agent_id: "a1",
            tool_use_id: None,
            output_file: None,
            description: "Checker",
            outcome: Outcome::Completed,
            result: Some("Waiting for the check."),
            usage: None,
            waiting: true,
        };
        assert_eq!(
            waiting.render(),
            "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n<summary>Agent \"Checker\" is \
             waiting on its own background work</summary>\n<note>This agent stopped with background work of its own \
             still running. It may resume on its own when that work completes or reports, and the same task-id \
             notifies again if it does; the result below may be interim.</note>\n<result>Waiting for the \
             check.</result>\n</task-notification>"
        );
    }
}
