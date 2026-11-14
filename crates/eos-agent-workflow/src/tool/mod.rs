//! The Workflow tool itself: input, Claude Code's validation and error
//! wording, what the permission gate reviews, the launch and its result.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{
    BackgroundTask, BackgroundTasks, BoxFuture, Subagents, TaskStatus, Tool, ToolContext, ToolOutput, ToolReview,
    ToolSpec, Workspace,
};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::config::WorkflowConfig;
use crate::journal::{Journal, Replay};
use crate::meta::Parsed;
use crate::run::{self, RunSpec};

mod validate;
use crate::store::{Readable, SessionDir, write_private};
use crate::{lint, named};

pub const NAME: &str = "Workflow";
const DESCRIPTION: &str = include_str!("../../prompts/Workflow.md");
const SCHEMA: &str = include_str!("../../prompts/Workflow.schema.json");
/// The authoring reference: the bundled skill's body, and the description's
/// tail when that skill is not available.
pub const AUTHORING: &str = include_str!("../../prompts/workflow-authoring.md");
pub const SKILL: &str = "workflow-authoring";
pub const SKILL_DESCRIPTION: &str = "Reference for writing a Workflow tool script (script API and gotchas, resume, \
quality patterns, worked examples). Load before authoring a script for a workflow the user already opted into; it does \
not itself authorize running one.";
const POINTER: &str = "Before writing a script, load the `workflow-authoring` skill \u{2014} the workflow authoring \
reference: script API and gotchas, resume, the **Ultracode** section, quality patterns, worked examples.";
const REVIEW: &str = "Review dynamic workflow before running";
const PLAIN_JS: &str = "Workflow scripts must be plain JavaScript \u{2014} common causes are TypeScript syntax (type \
annotations, interfaces, generics) and broken string quoting or escaping.";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    script: Option<String>,
    name: Option<String>,
    #[serde(rename = "description")]
    _description: Option<String>,
    #[serde(rename = "title")]
    _title: Option<String>,
    args: Option<Value>,
    #[serde(rename = "scriptPath")]
    script_path: Option<String>,
    #[serde(rename = "resumeFromRunId")]
    resume: Option<String>,
}

/// The workflow runs an agent launched, by run id (for "still running").
#[derive(Debug, Default)]
struct Runs(Mutex<HashMap<String, Arc<BackgroundTask>>>);

/// A resolved, valid script.
struct Resolved {
    script: String,
    /// The file it came from (`scriptPath`).
    path: Option<PathBuf>,
    parsed: Parsed,
}

pub struct WorkflowTool {
    spec: ToolSpec,
    config: Arc<WorkflowConfig>,
    subagents: Arc<dyn Subagents>,
}

impl std::fmt::Debug for WorkflowTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowTool").field("root", &self.config.root).finish_non_exhaustive()
    }
}

/// Claude Code's description: the tool text, then the skill pointer (or the
/// whole reference), then the size guideline.
pub fn description(config: &WorkflowConfig) -> String {
    let mut text = format!("{DESCRIPTION}\n\n{}", if config.authoring_skill { POINTER } else { AUTHORING });
    if let Some(guideline) = config.size.description(config.size_is_default) {
        text.push_str("\n\n");
        text.push_str(&guideline);
    }
    text
}

impl WorkflowTool {
    pub fn new(config: WorkflowConfig, subagents: Arc<dyn Subagents>) -> Self {
        let schema = serde_json::from_str(SCHEMA).expect("the bundled schema is valid JSON");
        let spec = ToolSpec::new(NAME, &description(&config), schema)
            .with_search_hint("orchestrate subagents with deterministic JavaScript workflow");
        Self { spec, config: Arc::new(config), subagents }
    }

    fn session(&self, agent: &str) -> SessionDir {
        SessionDir::new(&self.config.root, agent)
    }

    /// The hint Claude Code adds to script errors (its `Ir`).
    fn skill_hint(&self, input: &Input) -> &'static str {
        if input.name.is_none()
            && (input.script.is_some() || input.script_path.is_some())
            && self.config.authoring_skill
        {
            "\nLoad the `workflow-authoring` skill for the script reference if you have not, fix the script, and retry."
        } else {
            ""
        }
    }

    fn still_running(&self, ctx: &ToolContext<'_>, run_id: &str) -> Option<String> {
        let runs = ctx.extensions.get::<Runs>()?;
        let task = runs.0.lock().unwrap_or_else(PoisonError::into_inner).get(run_id).cloned()?;
        (task.status() == TaskStatus::Running).then(|| {
            format!(
                "Workflow {run_id} is still running (task {}). Stop it first with TaskStop({{task_id: \"{}\"}}) before \
                 resuming.",
                task.id, task.id
            )
        })
    }

    async fn launch(&self, input: Input, ctx: ToolContext<'_>) -> ToolOutput {
        let resolved = match self.validate(&input, &ctx) {
            Ok(r) => r,
            Err(e) => return refused(e),
        };
        let session = self.session(ctx.agent_id);
        let run_id = input.resume.clone().unwrap_or_else(|| {
            let id = uuid::Uuid::new_v4().to_string();
            format!("wf_{}", &id[..12])
        });
        let task_id = new_task_id();
        let meta = resolved.parsed.meta.clone();
        // Claude Code's compile step: the regex-or-division check, then the
        // constructs it refuses.
        let refused_construct = self
            .config
            .engine
            .ambiguous_slash(&resolved.parsed.body)
            .or_else(|| lint::forbidden(&resolved.parsed.body).map(Cow::into_owned));
        if let Some(refused_construct) = refused_construct {
            let hint = self.skill_hint(&input);
            return ToolOutput::error(format!(
                "Workflow script has a syntax error and was not launched:\nSyntaxError: {refused_construct}{hint}"
            ));
        }
        let transcripts = session.transcripts(&run_id);
        let script_path = match &resolved.path {
            Some(path) => path.clone(),
            None => {
                let path = session.script(&meta.name, &run_id);
                if let Err(e) = write_private(&path, &resolved.script) {
                    return refused(format!("Could not save the workflow script to {}: {e}", path.display()));
                }
                path
            }
        };
        let replay = match &input.resume {
            Some(run_id) => {
                if let Some(e) = self.still_running(&ctx, run_id) {
                    return refused(e);
                }
                match Journal::new(&transcripts).load() {
                    Ok(entries) => Some(Replay::from_entries(entries)),
                    Err(e) => return refused(format!("Could not read the journal for workflow run {run_id}: {e}")),
                }
            }
            None => None,
        };
        let task = Arc::new(BackgroundTask::new(task_id.clone(), "local_workflow", meta.description.clone()));
        ctx.extensions.get_or_default::<BackgroundTasks>().insert(Arc::clone(&task));
        {
            let runs = ctx.extensions.get_or_default::<Runs>();
            let mut runs = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
            // Finished runs only matter for "still running"; keep a few.
            if runs.len() >= 64 {
                runs.retain(|_, t| t.status() == TaskStatus::Running);
            }
            runs.insert(run_id.clone(), Arc::clone(&task));
        }
        let workspace = ctx.extensions.get::<Workspace>();
        let spec = RunSpec {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
            session: ctx.agent_id.to_owned(),
            tool_use_id: ctx.tool_use_id.to_owned(),
            script: resolved.script,
            script_path: script_path.clone(),
            args: input.args,
            meta: meta.clone(),
            body: resolved.parsed.body,
            replay,
            dir: session,
            cwd: workspace.as_ref().map(|w| w.cwd()),
            workspace,
            default_model: self.subagents.model_of(ctx.agent_id),
        };
        run::start(spec, Arc::clone(&self.config), Arc::clone(&self.subagents), task);
        ToolOutput::text(launched(&task_id, &meta.description, &transcripts, &script_path, &run_id))
    }
}

/// Claude Code's result text for a launched run.
fn launched(
    task_id: &str,
    summary: &str,
    transcripts: &std::path::Path,
    script: &std::path::Path,
    run_id: &str,
) -> String {
    let script = script.display();
    format!(
        "Workflow launched in background. Task ID: {task_id}\nSummary: {summary}\nTranscript dir: {}\nScript file: \
         {script}\n(Edit this file with Write/Edit and re-invoke Workflow with {{scriptPath: \"{script}\"}} to iterate \
         without resending the script.)\nRun ID: {run_id}\nTo resume after editing the script: Workflow({{scriptPath: \
         \"{script}\", resumeFromRunId: \"{run_id}\"}}) \u{2014} completed agents return cached results (cached results \
         may themselves be empty \u{2014} inspect journal.jsonl before assuming there is something to recover).\n\n\
         You will be notified when it completes. Use /workflows to watch live progress.",
        transcripts.display()
    )
}

/// Claude Code's `C`: the parse error with the line it points at. The
/// message ends in acorn's `(line:column)`, the column counted from 0.
/// Claude Code's `local_workflow` task id: `w` and 8 random bytes, each
/// taken modulo 36 as a digit or lowercase letter.
fn new_task_id() -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = String::with_capacity(9);
    out.push('w');
    out.extend((0..8).map(|_| char::from(DIGITS[usize::from(fastrand::u8(..)) % DIGITS.len()])));
    out
}

fn refused(message: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}

impl Tool for WorkflowTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match serde_json::from_str::<Input>(input.get()) {
                Ok(input) => self.launch(input, ctx).await,
                Err(e) => refused(format_args!("InputValidationError: {e}")),
            }
        })
    }

    /// The gate sees the script that would run, also when the call names it
    /// by path or by name.
    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        Box::pin(async move {
            let mut value: Value = serde_json::from_str(input.get()).ok()?;
            let parsed: Input = serde_json::from_str(input.get()).ok()?;
            if parsed.script.is_none() {
                let workspace = ctx.extensions.get::<Workspace>();
                let session = self.session(ctx.agent_id);
                let script = if let Some(path) = &parsed.script_path {
                    Readable { workspace: workspace.as_deref(), session: &session }.load(path).ok().map(|(s, _)| s)
                } else if let Some(name) = &parsed.name {
                    let cwd = workspace.as_ref().map(|w| w.cwd());
                    named::find(name, cwd.as_deref(), self.config.user_dir.as_deref()).map(|s| s.script)
                } else {
                    None
                };
                if let (Some(script), Some(obj)) = (script, value.as_object_mut()) {
                    obj.insert("script".into(), Value::String(script));
                }
            }
            let input = RawValue::from_string(value.to_string()).ok()?.into();
            Some(ToolReview { message: REVIEW.to_owned(), input })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids() {
        let id = new_task_id();
        assert!(id.len() == 9 && id.starts_with('w'));
        assert!(id[1..].bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()));
    }
}
