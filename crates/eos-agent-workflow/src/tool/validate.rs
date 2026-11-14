//! Claude Code's `validateInput`: which script a call names, and the
//! refusals before anything runs.

use std::path::PathBuf;

use eos_agent_core::{ToolContext, Workspace};

use super::{Input, PLAIN_JS, Resolved, WorkflowTool};
use crate::journal::Journal;
use crate::meta::{self, MAX_SCRIPT};
use crate::store::{Readable, printable};
use crate::{lint, named};

impl WorkflowTool {
    /// Claude Code's `validateInput`, then the script it resolves to.
    pub(super) fn validate(&self, input: &Input, ctx: &ToolContext<'_>) -> Result<Resolved, String> {
        if input.script.is_none() && input.name.is_none() && input.script_path.is_none() {
            return Err("Must provide script, name, scriptPath, or runId".into());
        }
        if let Some(script) = &input.script {
            if script.chars().count() > MAX_SCRIPT {
                return Err(format!("script: String must contain at most {MAX_SCRIPT} character(s)"));
            }
            if !printable(script) {
                return Err("script contains control characters that would be hidden in the approval dialog".into());
            }
        }
        for (field, value) in [("name", &input.name), ("scriptPath", &input.script_path)] {
            if value.as_deref().is_some_and(|v| v.chars().any(|c| c.is_control() || is_format(c))) {
                return Err(format!("{field}: contains control or invisible format characters"));
            }
        }
        let workspace = ctx.extensions.get::<Workspace>();
        let session = self.session(ctx.agent_id);
        let readable = Readable { workspace: workspace.as_deref(), session: &session };
        if let Some(path) = &input.script_path
            && let Some(e) = readable.check(path)
        {
            return Err(e);
        }
        if let Some(run_id) = &input.resume {
            if !valid_run_id(run_id) {
                return Err("resumeFromRunId: Invalid".into());
            }
            if let Some(e) = self.still_running(ctx, run_id) {
                return Err(e);
            }
            let transcripts = session.transcripts(run_id);
            if !Journal::new(&transcripts).exists() {
                return Err(format!(
                    "The journal for workflow run {run_id} is not on disk, and this session cannot fetch a remote copy, \
                     so there is nothing to resume. Fallback: Read the agent-<id>.jsonl files in {} and hand-author a \
                     continuation script.",
                    transcripts.display()
                ));
            }
        }
        let cwd = workspace.as_ref().map(|w| w.cwd());
        let (script, path, from_file) = if let Some(path) = &input.script_path {
            match &input.script {
                Some(script) => (script.clone(), Some(readable_path(workspace.as_deref(), path)), false),
                None => {
                    let (text, real) = readable.load(path)?;
                    (text, Some(real), true)
                }
            }
        } else if let Some(name) = &input.name {
            match named::find(name, cwd.as_deref(), self.config.user_dir.as_deref()) {
                Some(saved) => (input.script.clone().unwrap_or(saved.script), None, input.script.is_none()),
                None => return Err(named::not_found(name, cwd.as_deref(), self.config.user_dir.as_deref())),
            }
        } else {
            (input.script.clone().unwrap_or_default(), None, false)
        };
        if from_file && !printable(&script) {
            let what = match (&input.script_path, &input.name) {
                (Some(p), _) => format!("Workflow script file {p}"),
                (None, Some(n)) => format!("The file of workflow \"{n}\""),
                _ => "The script".into(),
            };
            return Err(format!(
                "{what} contains a control character that an approval prompt cannot show, such as an escape byte or a \
                 carriage return that is not part of a CRLF line ending. Remove it from the file and try again."
            ));
        }
        let hint = self.skill_hint(input);
        let parsed = meta::parse(&script).map_err(|e| format!("Invalid workflow script: {e}{hint}"))?;
        if let Err(e) = self.config.engine.check_syntax(&parsed.body) {
            let body_line = e.line.map(|l| l + script[..script.len() - parsed.body.len()].matches('\n').count());
            return Err(format!(
                "Invalid workflow script: {}{hint}",
                parse_error(&e.message, &script, body_line, e.column)
            ));
        }
        if input.script.is_some() && lint::nondeterministic(&parsed.body) {
            return Err(format!("{}{hint}", lint::NONDETERMINISTIC));
        }
        Ok(Resolved { script, path, parsed })
    }
}

fn parse_error(message: &str, script: &str, line: Option<usize>, column: Option<usize>) -> String {
    let message = match (line, column) {
        (Some(line), Some(column)) => format!("{message} ({line}:{})", column.saturating_sub(1)),
        _ => message.to_owned(),
    };
    let text = line.and_then(|l| script.split('\n').nth(l.saturating_sub(1)));
    let (Some(text), Some(column)) = (text, column) else {
        return format!("Script parse error: {message}. {PLAIN_JS}");
    };
    const SHOWN: usize = 80;
    let chars: Vec<char> = text.chars().collect();
    let col = column.saturating_sub(1).min(chars.len());
    let start = col.saturating_sub(SHOWN / 2).min(chars.len().saturating_sub(SHOWN));
    let excerpt: String = chars[start..chars.len().min(start + SHOWN)].iter().collect();
    format!("Script parse error: {message}\n\n{excerpt}\n{}^\n\n{PLAIN_JS}", " ".repeat(col - start))
}

fn readable_path(workspace: Option<&Workspace>, path: &str) -> PathBuf {
    workspace.map_or_else(|| PathBuf::from(path), |w| w.resolve(std::path::Path::new(path)))
}

fn is_format(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{feff}' | '\u{2028}' | '\u{2029}')
}

/// `^wf_[a-z0-9-]{6,}$`.
fn valid_run_id(id: &str) -> bool {
    id.strip_prefix("wf_").is_some_and(|rest| {
        rest.len() >= 6 && rest.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids() {
        assert!(valid_run_id("wf_1a2b3c4d-5e6") && !valid_run_id("wf_ABCDEF") && !valid_run_id("wf_12"));
    }

    #[test]
    fn parse_errors_point_at_the_line() {
        let script = "export const meta = {name: 'a', description: 'b'}\nconst x: string = 1";
        let out = parse_error("Unexpected token", script, Some(2), Some(8));
        assert_eq!(
            out,
            format!("Script parse error: Unexpected token (2:7)\n\nconst x: string = 1\n       ^\n\n{PLAIN_JS}")
        );
        assert_eq!(parse_error("bad", script, None, None), format!("Script parse error: bad. {PLAIN_JS}"));
    }
}
