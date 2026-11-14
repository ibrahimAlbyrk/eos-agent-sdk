//! The `<task-notification>` a finished run sends the agent that launched it
//! (Claude Code's `Dqr`).

use std::path::Path;

use serde_json::Value;

/// Longer results are cut here; the whole one is in the output file.
const RESULT_SHOWN: usize = 8000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Completed,
    Failed,
    Killed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification<'a> {
    pub task_id: &'a str,
    pub tool_use_id: &'a str,
    pub output_file: &'a Path,
    pub status: Status,
    /// `meta.description`.
    pub summary: &'a str,
    pub error: Option<&'a str>,
    /// The run's result as JSON text (key order as the script built it).
    pub result: Option<&'a str>,
    pub failures: &'a [String],
    pub agent_count: u32,
    /// `(done, error, skipped, empty result)`.
    pub counts: (u32, u32, u32, u32),
    pub total_tokens: u64,
    pub tool_uses: u64,
    pub duration_ms: u64,
    pub transcript_dir: &'a Path,
    pub script_path: &'a Path,
    pub run_id: &'a str,
    pub args: Option<&'a Value>,
}

impl Notification<'_> {
    pub fn render(&self) -> String {
        let name = escape(self.summary);
        let summary = match self.status {
            Status::Completed => format!("Dynamic workflow \"{name}\" completed"),
            Status::Failed => {
                format!("Dynamic workflow \"{name}\" failed: {}", self.error.map_or("Unknown error".into(), escape))
            }
            Status::Killed => format!("Dynamic workflow \"{name}\" was stopped"),
        };
        let script = self.script_path.display();
        let args = self.args.map(|a| format!(", args: {a}")).unwrap_or_default();
        let transcripts = self.transcript_dir.display();
        let mut body = String::new();
        if self.status != Status::Completed {
            let lines = [
                format!(
                    "To resume after editing the script, call: Workflow({{scriptPath: '{script}', resumeFromRunId: '{}'{args}}})",
                    self.run_id
                ),
                format!("Agent transcripts: {transcripts}"),
            ];
            body += &format!("\n<recovery>{}</recovery>", escape(&lines.join("\n")));
        }
        if let Some(result) = self.result.filter(|_| self.status == Status::Completed) {
            let text = escape(result);
            let total = text.chars().count();
            if total > RESULT_SHOWN {
                let cut: String = text.chars().take(RESULT_SHOWN).collect();
                body += &format!(
                    "\n<result>{cut}\n... (truncated {} chars, full result in {})</result>",
                    total - RESULT_SHOWN,
                    self.output_file.display()
                );
            } else {
                body += &format!("\n<result>{text}</result>");
            }
        }
        if self.status == Status::Completed {
            let lines = [
                format!(
                    "Per-agent results: {transcripts}/journal.jsonl \u{2014} one {{\"type\":\"result\",...}} line per \
                     completed agent with its full return value."
                ),
                "If the result above is empty or unexpected, Read this file BEFORE diagnosing \u{2014} do not assume \
                 agents returned non-empty results."
                    .to_owned(),
                format!(
                    "To re-run with edited post-processing: Workflow({{scriptPath: '{script}', resumeFromRunId: '{}'{args}}}) \
                     \u{2014} agents whose (prompt, opts) are unchanged replay from cache.",
                    self.run_id
                ),
            ];
            body += &format!("\n<diagnostics>{}</diagnostics>", escape(&lines.join("\n")));
        }
        if !self.failures.is_empty() {
            body += &format!("\n<failures>{}</failures>", escape(&self.failures.join("\n")));
        }
        let (done, error, skipped, empty) = self.counts;
        body += &format!(
            "\n<usage><agent_count>{}</agent_count><agents_done>{done}</agents_done><agents_error>{error}</agents_error>\
             <agents_skipped>{skipped}</agents_skipped><agents_empty_result>{empty}</agents_empty_result>\
             <subagent_tokens>{}</subagent_tokens><tool_uses>{}</tool_uses><duration_ms>{}</duration_ms></usage>",
            self.agent_count, self.total_tokens, self.tool_uses, self.duration_ms
        );
        format!(
            "<task-notification>\n<task-id>{}</task-id>\n<tool-use-id>{}</tool-use-id>\n<output-file>{}</output-file>\n\
             <status>{}</status>\n<summary>{summary}</summary>{body}\n</task-notification>",
            self.task_id,
            self.tool_use_id,
            self.output_file.display(),
            self.status.as_str()
        )
    }
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn base<'a>(result: Option<&'a str>, failures: &'a [String]) -> Notification<'a> {
        Notification {
            task_id: "w1",
            tool_use_id: "toolu_1",
            output_file: Path::new("/s/tasks/w1.output"),
            status: Status::Completed,
            summary: "Review <changes>",
            error: None,
            result,
            failures,
            agent_count: 2,
            counts: (2, 0, 0, 1),
            total_tokens: 300,
            tool_uses: 4,
            duration_ms: 1500,
            transcript_dir: Path::new("/s/subagents/workflows/wf_1"),
            script_path: Path::new("/s/workflows/scripts/review-wf_1.js"),
            run_id: "wf_1",
            args: None,
        }
    }

    #[test]
    fn completed() {
        let out = base(Some(r#"{"confirmed":[]}"#), &[]).render();
        assert_eq!(
            out,
            "<task-notification>\n<task-id>w1</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n\
             <output-file>/s/tasks/w1.output</output-file>\n<status>completed</status>\n\
             <summary>Dynamic workflow \"Review &lt;changes&gt;\" completed</summary>\n\
             <result>{\"confirmed\":[]}</result>\n\
             <diagnostics>Per-agent results: /s/subagents/workflows/wf_1/journal.jsonl \u{2014} one {\"type\":\"result\",...} \
             line per completed agent with its full return value.\nIf the result above is empty or unexpected, Read this \
             file BEFORE diagnosing \u{2014} do not assume agents returned non-empty results.\nTo re-run with edited \
             post-processing: Workflow({scriptPath: '/s/workflows/scripts/review-wf_1.js', resumeFromRunId: 'wf_1'}) \u{2014} \
             agents whose (prompt, opts) are unchanged replay from cache.</diagnostics>\n\
             <usage><agent_count>2</agent_count><agents_done>2</agents_done><agents_error>0</agents_error>\
             <agents_skipped>0</agents_skipped><agents_empty_result>1</agents_empty_result><subagent_tokens>300\
             </subagent_tokens><tool_uses>4</tool_uses><duration_ms>1500</duration_ms></usage>\n</task-notification>"
        );
    }

    #[test]
    fn failed_with_recovery_and_failures() {
        let failures = vec!["parallel[1] failed: boom".to_owned()];
        let args = json!(["a.ts"]);
        let mut n = base(None, &failures);
        n.status = Status::Failed;
        n.error = Some("Error: x");
        n.args = Some(&args);
        let out = n.render();
        assert!(out.contains("<summary>Dynamic workflow \"Review &lt;changes&gt;\" failed: Error: x</summary>"));
        assert!(out.contains(
            "<recovery>To resume after editing the script, call: Workflow({scriptPath: \
             '/s/workflows/scripts/review-wf_1.js', resumeFromRunId: 'wf_1', args: [\"a.ts\"]})\nAgent transcripts: \
             /s/subagents/workflows/wf_1</recovery>"
        ));
        assert!(out.contains("<failures>parallel[1] failed: boom</failures>"));
        assert!(!out.contains("<diagnostics>") && !out.contains("<result>"));
    }

    #[test]
    fn long_results_are_cut() {
        let result = format!("\"{}\"", "x".repeat(8998));
        let out = base(Some(&result), &[]).render();
        assert!(out.contains("... (truncated 1000 chars, full result in /s/tasks/w1.output)</result>"), "{out}");
    }
}
