//! Starting a top-level agent: everything per agent is assembled here.

use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{
    AgentConfig, AgentEvent, Compaction, CompactionText, Extensions, FailedServer, InternalPaths, ReminderSource,
    ServerNotes, SubagentPort, SystemPrompt, TaskEvents, ToolSource, Toolbox,
};
use eos_agent_engine::AgentSpec;
use eos_agent_mcp::{McpConfig, McpScope, McpServerConfig, McpToolset};
use eos_agent_prompt::{compaction_threshold, request_extra_with_metadata};
use eos_agent_skills::{ListingBudget, SkillListingReminder};
use eos_agent_tools_session::{ConnectingServers, DeferredTool, McpConnecting, PendingServer, TaskListId};
use eos_agent_tools_shell::{AgentSandbox, SandboxSession};
use eos_agent_tools_web::{SearchBudget, SessionSearchBudget};
use tokio::sync::mpsc;

use crate::agent::SdkAgent;
use crate::auth::SessionAuth;
use crate::context::{ContextFacts, server_notes, session_context};
use crate::host::HostTool;
use crate::options::{AgentOptions, SystemPromptOverride};
use crate::profile::{ModelProfile, Provider};
use crate::project::Project;
use crate::registry::Blueprint;
use crate::sdk::{Sdk, SdkError};
use crate::services::Services;
use crate::toolbox::{ToolFilter, ToolRecipe};

/// What the host asked for at spawn that holds for every agent of the tree,
/// whatever model each runs on.
#[derive(Debug)]
pub(crate) struct Asked {
    pub(crate) host_tools: Vec<HostTool>,
    pub(crate) filter: ToolFilter,
    /// Deferred tools wait behind ToolSearch on models that have it.
    pub(crate) tool_search: bool,
    /// The top-level agent's own system prompt.
    pub(crate) system_prompt: Option<SystemPromptOverride>,
    /// The top-level agent's output limit and window, when the host set them.
    pub(crate) max_tokens: Option<u32>,
    pub(crate) context_window: Option<u64>,
    /// Conversations are summarized on their own.
    pub(crate) auto_compact: bool,
    /// Where the top-level agent's is, when the host set it.
    pub(crate) compact_at: Option<u64>,
    pub(crate) compaction_text: Arc<dyn CompactionText>,
    pub(crate) claude_md: bool,
}

impl Asked {
    /// A host tool only the top-level agent may call.
    pub(crate) fn is_main_only(&self, tool: &str) -> bool {
        self.host_tools.iter().any(|t| t.main_agent_only && t.name() == tool)
    }

    /// Summarizing for an agent with this window and output limit: at `at`,
    /// else where Claude Code starts it, or only when asked.
    pub(crate) fn compaction(&self, context_window: u64, max_tokens: u32, at: Option<u64>) -> Compaction {
        let threshold = at.unwrap_or_else(|| compaction_threshold(context_window, u64::from(max_tokens)));
        Compaction { threshold_tokens: self.auto_compact.then_some(threshold), text: Arc::clone(&self.compaction_text) }
    }
}

/// What follows from a top-level agent's model: its settings (without a
/// tool source), its system prompt, what its toolbox is made of.
pub(crate) struct ModelSetup {
    pub(crate) config: AgentConfig,
    pub(crate) system: SystemPrompt,
    pub(crate) recipe: ToolRecipe,
    pub(crate) context_window: u64,
}

/// Claude Code's settings for `model` (already resolved), within what the
/// host asked for.
pub(crate) fn model_setup(
    services: &Services,
    project: &Project,
    asked: &Asked,
    id: &str,
    model: &str,
    effort: Option<&str>,
    thinking_summaries: bool,
) -> Result<ModelSetup, SdkError> {
    let profile = services
        .profile(model)
        .ok_or_else(|| SdkError::NoProvider { provider: Provider::of(model), model: model.to_owned() })?;
    let recipe = ToolRecipe {
        base: Arc::clone(&profile.base_tools),
        host: asked.host_tools.clone(),
        filter: asked.filter.clone(),
        defer: profile.defer_tools && asked.tool_search,
        full_wording: profile.full_wording,
    };
    // Unset, Claude Code's defaults for the model.
    let (default_max, default_window) = services.limits(model);
    let max_tokens = asked.max_tokens.unwrap_or(default_max);
    let context_window = asked.context_window.unwrap_or(default_window);
    let mut config = AgentConfig::new(model);
    config.max_tokens = max_tokens;
    config.effort = services.effort_for(model, effort);
    config.thinking = services.thinking_for(model, thinking_summaries, max_tokens);
    config.system_turns = services.system_turns(model);
    config.tool_changes = services.tool_changes(model);
    config.tool_change_refusals = Arc::clone(&services.tool_change_refusals);
    config.cache_ttl = profile.cache_ttl;
    config.extra = request_extra(profile, project, id);
    // The agent's prompt-cache key: its id (forks and Claude subagents
    // keep their tree's).
    config.cache_key = Some(Arc::from(id));
    config.compaction = Some(asked.compaction(context_window, max_tokens, asked.compact_at));
    config.reminder_sources = reminders(services, project, ListingBudget::for_context_window(context_window));
    let system = system_prompt(profile, asked.system_prompt.as_ref());
    Ok(ModelSetup { config, system, recipe, context_window })
}

impl Sdk {
    /// Starts an agent. Its events arrive on the returned channel (drop it to
    /// ignore them); dropping the [`SdkAgent`] stops it and its subagents.
    pub async fn spawn(&self, options: AgentOptions) -> Result<(SdkAgent, mpsc::Receiver<AgentEvent>), SdkError> {
        let services = &self.services;
        let cwd = tokio::fs::canonicalize(&options.cwd).await?;
        let project = services.project(&cwd, &options.plugin_dirs).await;
        if let Err(e) = &project.sandbox {
            return Err(SdkError::SandboxUnavailable(e.clone()));
        }
        let asked = Arc::new(Asked {
            host_tools: options.tools,
            filter: ToolFilter { allow: options.allowed_tools, deny: options.disallowed_tools },
            tool_search: options.tool_search,
            system_prompt: options.system_prompt,
            max_tokens: options.max_tokens,
            context_window: options.context_window,
            auto_compact: options.auto_compact,
            compact_at: options.compact_at,
            compaction_text: options.compaction_text.unwrap_or_else(|| Arc::clone(&services.compaction)),
            claude_md: options.claude_md,
        });
        let model = services.resolve_model(options.model.as_deref().unwrap_or(&services.default_model));
        let id = options.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let effort = options.effort.as_deref();
        let ModelSetup { mut config, system, recipe, context_window } =
            model_setup(services, &project, &asked, &id, &model, effort, options.thinking_summaries)?;
        let mcp = if options.mcp || !options.mcp_servers.is_empty() {
            Some(Arc::new(connect_mcp(services, &project, options.mcp, options.mcp_servers).await))
        } else {
            None
        };
        let mcp_tools = mcp.as_ref().map(|m| m.tools()).unwrap_or_default();
        let toolbox = project.toolbox(&recipe, mcp_tools);
        let updates = mcp.as_ref().map(|mcp| {
            Arc::new(McpToolUpdates {
                built: Mutex::new(Built { recipe, version: mcp.version(), toolbox: Arc::clone(&toolbox) }),
                mcp: Arc::clone(mcp),
                project: Arc::clone(&project),
            })
        });
        let connecting = updates.clone().map(|u| McpConnecting(u as Arc<dyn ConnectingServers>));
        config.tool_source = updates.clone().map(|u| u as Arc<dyn ToolSource>);
        let provider = Provider::of(&model);
        let own = match provider {
            Provider::Anthropic => services.claude_auth.clone(),
            Provider::OpenAi => services.openai_auth.clone(),
        };
        let auth = options.auth.map(|a| a.0).or(own).map(|source| {
            let session = SessionAuth::new(&id, source, services.auth_failover.clone());
            config.auth = Some(session.for_requests(provider));
            session
        });

        let blueprint = Arc::new(Blueprint {
            project: Arc::clone(&project),
            toolbox: Arc::clone(&toolbox),
            system,
            config,
            roots: options.roots,
            task_list: id.as_str().into(),
            search_budget: SessionSearchBudget(Arc::new(SearchBudget::new(eos_agent_tools_web::DEFAULT_SEARCH_BUDGET))),
            mcp: mcp.as_ref().map(Arc::downgrade),
            context_window,
            session_effort: options.effort,
            thinking_summaries: options.thinking_summaries,
            include_claude_md: asked.claude_md,
            subagent_events: options.subagent_events,
            task_events: options.task_events,
            sandbox: project.policy().cloned(),
            internal: internal_paths(services, &project.cwd, &id),
            interactive: options.interactive,
            connecting,
            mcp_tools: updates,
            asked: Arc::clone(&asked),
            seed: options.extensions,
        });
        // Claude Code leaves the snapshot out under a custom system prompt.
        let replaced = matches!(asked.system_prompt, Some(SystemPromptOverride::Replace(_)));
        let git_status = if replaced { None } else { services.git_status(&project.cwd).await };
        let facts = ContextFacts {
            project: &project,
            toolbox: &toolbox,
            model: &model,
            email: project.global.email.as_deref(),
            git_status: git_status.as_deref(),
            include_claude_md: asked.claude_md,
            servers: server_notes(mcp.as_deref(), &toolbox),
            listing_budget: ListingBudget::for_context_window(context_window),
            roots: &blueprint.roots,
            sandbox: blueprint.sandbox.as_ref(),
        };
        let mut spec = agent_spec(services, &blueprint, id, None);
        spec.history = options.history;
        spec.session = Some(session_context(&facts, services));
        // Tools rebuild their state (re-arming artifact watches) once the
        // agent is registered as top-level.
        let (registered, ready) = tokio::sync::oneshot::channel();
        spec.ready = Some(ready);
        let (handle, events) = services.engine.spawn(spec);
        services.registry.insert_root(&handle, blueprint);
        let _ = registered.send(());
        Ok((SdkAgent::new(handle, Arc::clone(services), project, mcp, auth), events))
    }
}

/// Rebuilds a top-level agent's toolbox when its MCP servers' tools change
/// (a server done connecting or signed in to, a tool list that changed),
/// before its next request; its forks and new subagents start from the same.
pub(crate) struct McpToolUpdates {
    mcp: Arc<McpToolset>,
    project: Arc<Project>,
    built: Mutex<Built>,
}

/// The toolbox built from `recipe` for the set's `version`, shared by every
/// agent using it.
struct Built {
    recipe: ToolRecipe,
    version: u64,
    toolbox: Arc<Toolbox>,
}

impl McpToolUpdates {
    fn toolbox(&self) -> Arc<Toolbox> {
        let version = self.mcp.version();
        let mut built = self.built.lock().unwrap_or_else(PoisonError::into_inner);
        if built.version != version {
            built.toolbox = self.project.toolbox(&built.recipe, self.mcp.tools());
            built.version = version;
        }
        Arc::clone(&built.toolbox)
    }

    /// From now on the toolbox is made by `recipe` (the agent changed model).
    pub(crate) fn set_recipe(&self, recipe: ToolRecipe) -> Arc<Toolbox> {
        let mut built = self.built.lock().unwrap_or_else(PoisonError::into_inner);
        built.version = self.mcp.version();
        built.toolbox = self.project.toolbox(&recipe, self.mcp.tools());
        built.recipe = recipe;
        Arc::clone(&built.toolbox)
    }
}

impl std::fmt::Debug for McpToolUpdates {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolUpdates").field("mcp", &self.mcp).finish_non_exhaustive()
    }
}

impl ToolSource for McpToolUpdates {
    fn update(&self, current: &Arc<Toolbox>) -> Option<Arc<Toolbox>> {
        let toolbox = self.toolbox();
        (!Arc::ptr_eq(&toolbox, current)).then_some(toolbox)
    }

    fn current(&self) -> Option<Arc<Toolbox>> {
        Some(self.toolbox())
    }

    fn servers(&self, toolbox: &Toolbox) -> ServerNotes {
        server_notes(Some(&self.mcp), toolbox)
    }
}

/// What the agent's ToolSearch waits for and reports.
impl ConnectingServers for McpToolUpdates {
    fn pending(&self) -> Vec<PendingServer> {
        let prefix = |name: &str| {
            let tool = eos_agent_mcp::tool_name(name, "x");
            tool.strip_prefix("mcp__").and_then(|t| t.strip_suffix("__x")).unwrap_or(name).to_owned()
        };
        self.mcp.pending().into_iter().map(|name| PendingServer { prefix: prefix(&name), name }).collect()
    }

    fn failed(&self) -> Vec<String> {
        server_notes(Some(&self.mcp), &Toolbox::default()).failed.iter().map(FailedServer::line).collect()
    }

    fn wait<'a>(&'a self, servers: &'a [String], limit: std::time::Duration) -> eos_agent_core::BoxFuture<'a, ()> {
        Box::pin(async move {
            self.mcp.wait_for_pending(servers, limit).await;
        })
    }

    fn deferred(&self) -> Vec<DeferredTool> {
        let toolbox = self.toolbox();
        toolbox.tools().iter().filter(|t| t.spec().is_deferred()).map(|t| DeferredTool::from_spec(t.spec())).collect()
    }
}

/// The engine spec for an agent built from `blueprint`; `extensions` adds to
/// the session-wide state every agent of the tree gets.
pub(crate) fn agent_spec(
    services: &Services,
    blueprint: &Blueprint,
    id: String,
    extensions: Option<Extensions>,
) -> AgentSpec {
    let extensions = extensions.unwrap_or_default();
    if let Some(seed) = &blueprint.seed {
        extensions.extend_from(seed);
    }
    if let Some(connecting) = &blueprint.connecting {
        extensions.insert(connecting.clone());
    }
    // TaskStop reaches the session's agents through it.
    extensions.insert(SubagentPort(Arc::clone(&services.subagents)));
    extensions.insert(TaskListId(blueprint.task_list.to_string()));
    extensions.insert(blueprint.search_budget.clone());
    extensions.insert(SandboxSession(blueprint.task_list.to_string()));
    extensions.insert(eos_agent_artifacts::ArtifactSession(blueprint.task_list.to_string()));
    if let Some(policy) = &blueprint.sandbox {
        extensions.insert(AgentSandbox(Arc::clone(policy)));
    }
    if let Some(to) = &blueprint.task_events {
        let to = to.clone();
        extensions.insert(TaskEvents::new(move |event| {
            let to = to.clone();
            Box::pin(async move {
                let _ = to.send(event).await;
            })
        }));
    }
    // The tools' own model requests (WebFetch, WebSearch) bill where the agent does.
    if let Some(auth) = &blueprint.config.auth {
        extensions.insert(auth.clone());
    }
    let mut spec = AgentSpec::new(blueprint.config.clone(), blueprint.system.clone(), Arc::clone(&blueprint.toolbox));
    spec.cwd.clone_from(&blueprint.project.cwd);
    spec.roots.clone_from(&blueprint.roots);
    spec.scratch = Some(services.data_dir.join("scratch").join(&id));
    spec.internal = blueprint.internal.clone();
    spec.id = Some(id);
    spec.extensions = extensions;
    spec
}

/// Claude Code lets its agents read its project folder, every session of
/// the directory it runs in ("Project directory files are allowed for
/// reading": a workflow's transcripts, journal and record, a resumed
/// conversation's earlier sessions too) and edit the session's workflow
/// scripts; here the session is the tree's top-level agent, working in `cwd`.
fn internal_paths(services: &Services, cwd: &std::path::Path, session: &str) -> InternalPaths {
    let Some(workflows) = &services.workflows else { return InternalPaths::default() };
    let project = workflows.root.join(eos_agent_compat::project_dir_name(cwd));
    let dir = eos_agent_workflow::SessionDir::new(&project, session);
    InternalPaths { readable: vec![project], writable: vec![(dir.scripts(), ".js".to_owned())] }
}

fn system_prompt(profile: &ModelProfile, custom: Option<&SystemPromptOverride>) -> SystemPrompt {
    match custom {
        None => profile.system.clone(),
        Some(SystemPromptOverride::Replace(text)) => profile.prompt(&[text]),
        Some(SystemPromptOverride::Append(text)) => {
            let mut texts: Vec<&str> = profile.system_texts.iter().map(AsRef::as_ref).collect();
            texts.push(text);
            profile.prompt(&texts)
        }
    }
}

/// The request extras for an agent on `profile`: Claude Code's, with its
/// `metadata.user_id` when the device has an id; none on OpenAI.
pub(crate) fn request_extra(profile: &ModelProfile, project: &Project, session_id: &str) -> Option<bytes::Bytes> {
    let base = profile.request_extra.clone()?;
    Some(match &project.global.device_id {
        Some(device) => request_extra_with_metadata(device, project.global.account_uuid.as_deref(), session_id),
        None => base,
    })
}

pub(crate) fn reminders(services: &Services, project: &Project, budget: ListingBudget) -> Vec<Arc<dyn ReminderSource>> {
    let mut sources = services.reminders.clone();
    sources.push(Arc::new(SkillListingReminder::new(Arc::clone(&project.skills), budget, true)));
    sources
}

/// The agent's MCP servers: the host's own first (they win a name that
/// repeats), then, with `configured`, `~/.claude.json` user and local entries
/// and the project's approved `.mcp.json`, merged by Claude Code's precedence.
async fn connect_mcp(
    services: &Services,
    project: &Project,
    configured: bool,
    mut servers: Vec<McpServerConfig>,
) -> McpToolset {
    if configured {
        let docs = &project.mcp;
        let scoped = [(McpScope::User, &docs.user), (McpScope::Project, &docs.project), (McpScope::Local, &docs.local)];
        let present: Vec<(McpScope, &serde_json::Value)> =
            scoped.iter().filter_map(|(scope, doc)| doc.as_ref().map(|d| (*scope, d))).collect();
        let mut config = McpConfig::merge(&present);
        for server in &mut config.servers {
            server.expand_vars(|name| std::env::var(name).ok());
        }
        servers.extend(config.servers);
    }
    services.hub.toolset(&servers).await
}
