//! The Skill tool: the model's way to load a listed skill, through the same
//! expansion as a typed `/name args`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{
    BoxFuture, Reminders, SubagentRequest, SubagentStart, Subagents, Tool, ToolContext, ToolOutput, ToolSpec, Workspace,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::catalog::{InvokeError, SkillCatalog};
use crate::expand::{ExpandContext, Expansion};
use crate::fork::{background_fork_request, fork_request};
use crate::inline::CommandRunner;

pub const SKILL_TOOL: &str = "Skill";
const DESCRIPTION: &str = include_str!("../prompts/Skill.md");
const SCHEMA: &str = include_str!("../Skill.schema.json");
const MAX_PENDING: usize = 16;

/// What a loaded skill asks of the rest of the turn; the engine applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillGrant {
    pub name: String,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub fork: bool,
    pub agent: Option<String>,
}

impl From<&Expansion> for SkillGrant {
    fn from(e: &Expansion) -> Self {
        Self {
            name: e.name.clone(),
            allowed_tools: e.allowed_tools.clone(),
            disallowed_tools: e.disallowed_tools.clone(),
            model: e.model.clone(),
            effort: e.effort.clone(),
            fork: e.fork,
            agent: e.agent.clone(),
        }
    }
}

/// Grants of skills the model loaded, waiting for the engine (kept in the
/// agent's extensions, bounded).
#[derive(Debug, Default)]
pub struct PendingGrants(Mutex<VecDeque<SkillGrant>>);

impl PendingGrants {
    pub fn take(&self) -> Vec<SkillGrant> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect()
    }

    fn push(&self, grant: SkillGrant) {
        let mut pending = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.len() >= MAX_PENDING {
            pending.pop_front();
        }
        pending.push_back(grant);
    }
}

pub struct SkillTool {
    spec: ToolSpec,
    catalog: Arc<SkillCatalog>,
    runner: Arc<dyn CommandRunner>,
    /// Runs `context: fork` skills as subagents.
    subagents: Option<Arc<dyn Subagents>>,
}

impl std::fmt::Debug for SkillTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillTool").finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Input {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

impl SkillTool {
    /// `runner` executes inline `` !`cmd` `` snippets ([`crate::NoCommands`] refuses them).
    pub fn new(catalog: Arc<SkillCatalog>, runner: Arc<dyn CommandRunner>) -> Self {
        let schema = serde_json::from_str(SCHEMA).expect("the bundled schema is JSON");
        Self { spec: ToolSpec::new(SKILL_TOOL, DESCRIPTION, schema), catalog, runner, subagents: None }
    }

    /// Runs `context: fork` skills as subagents, as Claude Code does
    /// (without, they load into the caller's turn).
    #[must_use]
    pub fn with_subagents(mut self, subagents: Arc<dyn Subagents>) -> Self {
        self.subagents = Some(subagents);
        self
    }

    /// Claude Code's forked execution: the expanded skill is the prompt of a
    /// subagent of the skill's `agent` type (general-purpose by default);
    /// its reply is the tool result. Where Claude Code's interactive
    /// terminal would, it launches in the background instead (see
    /// [`Subagents::background_fork`]), named as a typed fork is.
    async fn fork(&self, subagents: &dyn Subagents, expansion: Expansion, ctx: ToolContext<'_>) -> ToolOutput {
        let name = expansion.name.clone();
        let request = match subagents.background_fork(ctx.agent_id, &name, expansion.background) {
            Some(as_name) => SubagentRequest {
                tool_use_id: ctx.tool_use_id.to_owned(),
                ..background_fork_request(expansion, ctx.agent_id, &as_name)
            },
            None => fork_request(expansion, ctx.agent_id, ctx.tool_use_id),
        };
        let as_name = request.name.clone();
        match subagents.start(request).await {
            Ok(SubagentStart::Finished(report)) => {
                ToolOutput::text(format!("Skill \"{name}\" completed (forked execution).\n\nResult:\n{}", report.text))
            }
            Ok(SubagentStart::Launched { agent_id }) => {
                let shown = as_name.map_or(agent_id, |n| format!("@{n}"));
                ToolOutput::text(format!(
                    "Skill \"{name}\" launched (forked execution, running in the background).\n\nRunning in the background as {shown}"
                ))
            }
            Err(e) => ToolOutput::error(e),
        }
    }

    async fn run(&self, input: &RawValue, ctx: ToolContext<'_>) -> ToolOutput {
        let input: Input = match serde_json::from_str(input.get()) {
            Ok(input) => input,
            Err(e) => return invalid(&format!("InputValidationError: {e}")),
        };
        let trimmed = input.skill.trim();
        if trimmed.is_empty() {
            return invalid(&format!("Invalid skill format: {}", input.skill));
        }
        let name = trimmed.strip_prefix('/').unwrap_or(trimmed);
        let workspace = ctx.extensions.get::<Workspace>();
        let mut expand = ExpandContext::new(&*self.runner);
        expand.project_dir = workspace.as_ref().and_then(|w| w.roots().first()).map(|p| p.as_path());
        expand.session_id = Some(ctx.agent_id);
        expand.call = Some(ctx);

        match self.catalog.invoke(name, input.args.as_deref().unwrap_or("").trim(), &expand).await {
            Ok(expansion) if expansion.fork && self.subagents.is_some() => {
                let subagents = self.subagents.as_deref().expect("checked");
                self.fork(subagents, expansion, ctx).await
            }
            Ok(expansion) => {
                ctx.extensions.get_or_default::<Reminders>().push(expansion.body.clone());
                ctx.extensions.get_or_default::<PendingGrants>().push(SkillGrant::from(&expansion));
                ToolOutput::text(format!("Launching skill: {}", expansion.name))
            }
            Err(e @ (InvokeError::Unknown { .. } | InvokeError::UserOnly(_))) => invalid(&e.to_string()),
            Err(InvokeError::Expand(e)) => ToolOutput::error(e.to_string()),
        }
    }
}

impl Tool for SkillTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(self.run(input, ctx))
    }
}

fn invalid(message: &str) -> ToolOutput {
    ToolOutput::error(format!("<tool_use_error>{message}</tool_use_error>"))
}
