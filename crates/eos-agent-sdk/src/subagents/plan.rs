//! What a subagent starts from: a fork of its parent, or an agent type over
//! a narrowed copy of the parent's tools, prompt and settings.

use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_agents::{
    AgentDefinition, FORK, ForkedAgent, STRUCTURED_OUTPUT, SpawnPlan, StructuredOutput, WORKFLOW_SUBAGENT,
    fork_definition, fork_seed, workflow_subagent_definition,
};
use eos_agent_core::{
    Extensions, ServerNotes, SessionContext, SubagentRequest, SubagentSystem, Tool, ToolSource, Toolbox,
};
use eos_agent_skills::ListingBudget;
use eos_agent_types::{ContentBlock, Role};

use crate::context::{ContextFacts, session_context};
use crate::registry::Blueprint;
use crate::services::Services;
use crate::{spawn, toolbox};

/// Tools a subagent never gets: nobody answers its questions, and only a
/// top-level agent runs workflows (Claude Code's external build).
const NEVER_FOR_SUBAGENTS: &[&str] = &["AskUserQuestion", "Workflow"];

type Plan = (Blueprint, Extensions, Vec<ContentBlock>, String);

/// A fork: the parent's prompt, tools and model, its conversation, then the
/// seed answering the parent's tool calls with the directive.
pub(super) fn fork(parent: &Blueprint, request: &SubagentRequest) -> Result<Plan, String> {
    let last = request.history.last().filter(|m| m.role() == Role::Assistant);
    let blocks =
        last.and_then(|m| m.blocks().ok()).ok_or("A fork needs the conversation of the agent that calls it.")?;
    let input = fork_seed(&blocks, &request.tool_use_id, &request.prompt);
    let extensions = Extensions::default();
    extensions.insert(ForkedAgent);
    let mut blueprint = parent.clone();
    blueprint.toolbox = live_toolbox(parent);
    blueprint.config.max_turns = fork_definition().max_turns;
    Ok((blueprint, extensions, input, FORK.to_owned()))
}

/// A named agent type over a narrowed copy of the parent's tools.
pub(super) fn fresh(services: &Services, parent: &Blueprint, request: &SubagentRequest) -> Result<Plan, String> {
    let workflow_definition;
    let definition: &AgentDefinition = if request.agent_type.as_deref() == Some(WORKFLOW_SUBAGENT) {
        workflow_definition = workflow_subagent_definition();
        &workflow_definition
    } else {
        parent.project.definitions.resolve(request.agent_type.as_deref())?
    };
    let plan = SpawnPlan::new(definition, request.model.as_deref(), &parent.config.model, false);
    // A model whose provider has no credential here runs on the parent's
    // (a GPT parent asking for `haiku` without a Claude sign-in).
    let wanted_model = services.resolve_model(plan.model);
    let model = if services.profile(&wanted_model).is_some() { wanted_model } else { parent.config.model.clone() };
    let parent_profile = services.profile(&parent.config.model).ok_or("The calling agent's provider is gone.")?;
    let own = services.profile(&model).ok_or("No provider for the subagent's model.")?;
    // Its tools from the parent's: narrowed to what the agent type allows,
    // in its model's wording, with StructuredOutput for a structured answer.
    let allowed = definition.clone();
    let deny = request.deny_tools.clone();
    let asked = Arc::clone(&parent.asked);
    let same_profile = std::ptr::eq(parent_profile, own);
    let defer = own.defer_tools && parent.asked.tool_search;
    let (full_wording, wording) = (own.full_wording, Arc::clone(&own.base_tools));
    let schema = request.output_schema.clone();
    let build = move |parent: &Toolbox| {
        let keep = |name: &str| {
            allowed.allows_tool(name)
                && !NEVER_FOR_SUBAGENTS.contains(&name)
                && !asked.is_main_only(name)
                && !denied(&deny, name)
        };
        let toolbox = toolbox::narrow(parent, keep, defer);
        // Haiku 5.5 and GPT models word some tools differently, under any parent.
        let toolbox = if same_profile {
            toolbox
        } else if full_wording {
            let worded = toolbox::reword(&toolbox, &wording);
            toolbox::compose(crate::decorate::full_tools(worded.tools()), defer)
        } else {
            toolbox::reword(&toolbox, &wording)
        };
        match &schema {
            Some(schema) => {
                let answer: Arc<dyn Tool> = Arc::new(StructuredOutput::new(schema.clone()));
                let tools = toolbox.tools().iter().filter(|t| t.spec().name() != STRUCTURED_OUTPUT).cloned();
                toolbox::compose(tools.chain(std::iter::once(answer)), defer)
            }
            None => toolbox,
        }
    };
    let live = live_toolbox(parent);
    let toolbox = Arc::new(build(&live));
    let mut config = parent.config.clone();
    // As Claude Code's subagents (not its forks), one whose agent type takes
    // every tool or names MCP tools follows the MCP servers that connect
    // after it started; any other keeps the tools it started with.
    let follows = definition.tools.as_ref().is_none_or(|t| t.iter().any(|r| r == "*" || r.starts_with("mcp__")));
    config.tool_source = parent.config.tool_source.clone().filter(|_| follows).map(|parent| {
        let built = Mutex::new((live, Arc::clone(&toolbox)));
        Arc::new(SubagentTools { parent, build: Box::new(build), built }) as Arc<dyn ToolSource>
    });
    // Claude Code's `maxTurns` of the agent type (none: no limit).
    config.max_turns = plan.max_turns;
    let context_window = if model == parent.config.model {
        // The same model keeps what the host set for the tree.
        parent.context_window
    } else {
        // Claude Code's settings for the subagent's own model, not the parent's.
        let (max_tokens, context_window) = services.limits(&model);
        config.max_tokens = max_tokens;
        config.compaction = Some(parent.asked.compaction(context_window, max_tokens, None));
        context_window
    };
    if context_window != parent.context_window {
        config.reminder_sources =
            spawn::reminders(services, &parent.project, ListingBudget::for_context_window(context_window));
    }
    if own.provider != parent_profile.provider {
        // Another provider: its own request shape (the tree keeps its cache key).
        config.cache_ttl = own.cache_ttl;
        config.extra = spawn::request_extra(own, &parent.project, config.cache_key.as_deref().unwrap_or_default());
    }
    // The Agent call's effort, else the agent type's, else the session's;
    // each only as far as the model takes it.
    let wanted = request.effort.as_deref().or(definition.effort.as_deref()).or(parent.session_effort.as_deref());
    config.effort = services.effort_for(&model, wanted);
    config.thinking = services.thinking_for(&model, parent.thinking_summaries, config.max_tokens);
    config.system_turns = services.system_turns(&model);
    config.tool_changes = services.tool_changes(&model);
    config.model = model;
    let own_prompt = match &request.system {
        Some(SubagentSystem::Replace(text)) => text.clone(),
        Some(SubagentSystem::Append(text)) => format!("{}{text}", plan.system_prompt),
        None => plan.system_prompt.to_owned(),
    };
    let mut texts: Vec<&str> = vec![&own.identity, &own_prompt];
    texts.extend(own.subagent_suffix.as_deref());
    let system = own.prompt(&texts);
    let blueprint = Blueprint {
        project: Arc::clone(&parent.project),
        toolbox,
        system,
        config,
        roots: parent.roots.clone(),
        task_list: Arc::clone(&parent.task_list),
        search_budget: parent.search_budget.clone(),
        mcp: parent.mcp.clone(),
        context_window,
        session_effort: parent.session_effort.clone(),
        thinking_summaries: parent.thinking_summaries,
        include_claude_md: plan.include_claude_md && parent.asked.claude_md,
        subagent_events: parent.subagent_events.clone(),
        task_events: parent.task_events.clone(),
        sandbox: parent.sandbox.clone(),
        internal: parent.internal.clone(),
        confined: parent.confined,
        interactive: parent.interactive,
        connecting: None,
        mcp_tools: None,
        asked: Arc::clone(&parent.asked),
        seed: parent.seed.clone(),
    };
    let input = vec![ContentBlock::Text { text: request.prompt.clone(), citations: None }];
    Ok((blueprint, Extensions::default(), input, definition.name.clone()))
}

pub(super) async fn session(services: &Services, blueprint: &Blueprint, git: bool) -> SessionContext {
    let git_status = if git { services.git_status(&blueprint.project.cwd).await } else { None };
    let facts = ContextFacts {
        project: &blueprint.project,
        toolbox: &blueprint.toolbox,
        model: &blueprint.config.model,
        email: blueprint.project.global.email.as_deref(),
        git_status: git_status.as_deref(),
        include_claude_md: blueprint.include_claude_md,
        servers: crate::context::server_notes(
            blueprint.mcp.as_ref().and_then(std::sync::Weak::upgrade).as_deref(),
            &blueprint.toolbox,
        ),
        listing_budget: ListingBudget::for_context_window(blueprint.context_window),
        roots: &blueprint.roots,
        sandbox: blueprint.sandbox.as_ref(),
    };
    session_context(&facts, services)
}

/// The parent's tools as of now: a top-level agent's change as its MCP
/// servers connect.
pub(super) fn live_toolbox(parent: &Blueprint) -> Arc<eos_agent_core::Toolbox> {
    parent.config.tool_source.as_ref().and_then(|s| s.current()).unwrap_or_else(|| Arc::clone(&parent.toolbox))
}

/// A subagent's tools as its parent's change (Claude Code's `refreshTools`
/// of a subagent): rebuilt from the parent's whenever those change.
struct SubagentTools {
    parent: Arc<dyn ToolSource>,
    build: Box<dyn Fn(&Toolbox) -> Toolbox + Send + Sync>,
    /// The parent's toolbox and the one built from it.
    built: Mutex<(Arc<Toolbox>, Arc<Toolbox>)>,
}

impl std::fmt::Debug for SubagentTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentTools").field("parent", &self.parent).finish_non_exhaustive()
    }
}

impl ToolSource for SubagentTools {
    fn update(&self, current: &Arc<Toolbox>) -> Option<Arc<Toolbox>> {
        let toolbox = self.current()?;
        (!Arc::ptr_eq(&toolbox, current)).then_some(toolbox)
    }

    fn current(&self) -> Option<Arc<Toolbox>> {
        let parent = self.parent.current()?;
        let mut built = self.built.lock().unwrap_or_else(PoisonError::into_inner);
        if !Arc::ptr_eq(&built.0, &parent) {
            let toolbox = Arc::new((self.build)(&parent));
            *built = (parent, toolbox);
        }
        Some(Arc::clone(&built.1))
    }

    fn servers(&self, toolbox: &Toolbox) -> ServerNotes {
        self.parent.servers(toolbox)
    }
}

/// Whether `deny` takes tool `name` away: an exact name, a `prefix*`, or
/// `mcp__<server>` for all of that server's tools.
fn denied(deny: &[String], name: &str) -> bool {
    deny.iter().any(|d| match d.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => d == name || (d.starts_with("mcp__") && !d[5..].contains("__") && name.starts_with(&format!("{d}__"))),
    })
}
