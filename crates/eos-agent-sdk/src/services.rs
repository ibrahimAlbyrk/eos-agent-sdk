//! What one process shares between all its agents, and the per-directory
//! views it keeps fresh.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eos_agent_artifacts::ArtifactWatcher;
use eos_agent_compat::{ClaudePaths, GlobalConfig, read_global_config};
use eos_agent_core::{CompactionText, ModelClient, ReminderSource, Subagents, Thinking, TokenSource, TranscriptStore};
use eos_agent_engine::Engine;
use eos_agent_mcp::McpHub;
use eos_agent_openai::ModelCatalog;
use eos_agent_prompt::{ModelInfo, Prompts};
use eos_agent_skills::SkillRenderer;
use eos_agent_workflow::WorkflowConfig;

use crate::git;
use crate::models;
use crate::profile::{ModelProfile, Provider};
use crate::project::Project;
use crate::registry::Registry;
use crate::shared::SharedContexts;
use crate::stamps::stamp;

/// A working directory and the plugin folders loaded for it.
type ProjectKey = (PathBuf, Vec<PathBuf>);

/// A directory's git snapshot (`None` outside a work tree) and when it was taken.
type GitSnapshot = (Instant, Option<Arc<str>>);

/// Agents started this close together share one git snapshot.
const GIT_TTL: Duration = Duration::from_secs(5);

/// OpenAI models: their profile and the signed-in account's catalog.
pub(crate) struct OpenAiModels {
    pub(crate) profile: ModelProfile,
    pub(crate) catalog: Arc<ModelCatalog>,
}

/// The shared services. Agents reach them through [`Sdk`](crate::Sdk)
/// (strong) or, from inside their own tools, weakly, so nothing keeps the
/// process's services alive in a cycle.
pub(crate) struct Services {
    pub(crate) engine: Engine,
    /// Every provider's client behind one, chosen by a request's model.
    pub(crate) client: Arc<dyn ModelClient>,
    pub(crate) prompts: Prompts,
    /// Claude models (`None`: no Claude credential).
    pub(crate) claude: Option<ClaudeModels>,
    /// OpenAI models (`None`: no ChatGPT sign-in).
    pub(crate) openai: Option<OpenAiModels>,
    /// The credential of agents that were given none, per provider.
    pub(crate) claude_auth: Option<Arc<dyn TokenSource>>,
    pub(crate) openai_auth: Option<Arc<dyn TokenSource>>,
    /// Asked for another credential when an agent's stops working.
    pub(crate) auth_failover: Option<Arc<dyn crate::auth::AuthFailover>>,
    /// Shared by every agent.
    pub(crate) compaction: Arc<dyn CompactionText>,
    pub(crate) reminders: Vec<Arc<dyn ReminderSource>>,
    pub(crate) hub: Arc<McpHub>,
    pub(crate) paths: ClaudePaths,
    pub(crate) registry: Arc<Registry>,
    /// Artifact watches (with the artifact tools): one connection per
    /// watched artifact for the whole process.
    pub(crate) watcher: Option<ArtifactWatcher>,
    /// Composes the `artifact-capabilities` skill (with the artifact tools).
    pub(crate) artifact_skills: Option<Arc<dyn SkillRenderer>>,
    pub(crate) subagents: Arc<dyn Subagents>,
    pub(crate) data_dir: PathBuf,
    pub(crate) default_model: String,
    pub(crate) store: Option<Arc<dyn TranscriptStore>>,
    /// How long a finished subagent stays in memory (`None` = until its
    /// top-level agent is dropped; always so without a store).
    pub(crate) release_after: Option<Duration>,
    pub(crate) global: Mutex<Global>,
    /// By working directory and plugin folders.
    pub(crate) projects: Mutex<HashMap<ProjectKey, Arc<Project>>>,
    /// One directory is read at a time, so a burst of spawns reads it once.
    pub(crate) loading: tokio::sync::Mutex<()>,
    pub(crate) git: Mutex<HashMap<PathBuf, GitSnapshot>>,
    /// Session contexts, one copy for all agents that have the same.
    pub(crate) shared: SharedContexts,
    /// Claude Code's `sandbox` settings tiers and the shared sandbox runtime.
    pub(crate) sandboxes: crate::sandbox::Sandboxes,
    /// The Workflow tool's settings, when workflows are on.
    pub(crate) workflows: Option<WorkflowConfig>,
    /// The "ultracode" keyword opts a turn into a workflow.
    pub(crate) workflow_keyword: bool,
    /// Background subagents on or off, and how deep subagents may nest.
    pub(crate) spawn_limits: eos_agent_agents::SpawnLimits,
    /// Each session's turn token target and spending (`budget` in scripts).
    pub(crate) budgets: Arc<crate::workflows::Budgets>,
    /// Background tasks (workflow runs) still running, per top-level agent.
    pub(crate) running_tasks: Mutex<HashMap<String, usize>>,
    /// The shell tool and gate skills' inline commands run through.
    pub(crate) shell: Arc<crate::inline::ShellCell>,
    /// Refusals of Claude Code's tool-change betas, for every agent.
    pub(crate) tool_change_refusals: Arc<eos_agent_core::ToolChangeRefusals>,
    /// Late tools carry their definitions (Claude Code's inline tools).
    pub(crate) inline_tools: bool,
}

/// Claude Code's `Bon`: inline tools unless `CLAUDE_CODE_INLINE_TOOLS` turns
/// them off, and never in a `CCR_SESSION_PROFILE` session. Unset, the
/// engine sends them, as Claude Code does on the accounts its server turns
/// `tengu_brisk_meadow` on for (the user's own account among them).
pub(crate) fn inline_tools_from_env() -> bool {
    std::env::var_os("CCR_SESSION_PROFILE").is_none_or(|v| v.is_empty())
        && env_flag("CLAUDE_CODE_INLINE_TOOLS").unwrap_or(true)
}

/// An environment switch as Claude Code reads it: unset is `None`.
fn env_flag(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `~/.claude.json` as last read, with its stamp then.
pub(crate) struct Global {
    pub(crate) stamp: Option<(std::time::SystemTime, u64)>,
    pub(crate) config: Arc<GlobalConfig>,
}

impl Global {
    pub(crate) fn read(path: &Path) -> Self {
        let stamp = stamp(path);
        Self { stamp, config: Arc::new(read_global_config(path).unwrap_or_default()) }
    }
}

/// Claude models: Claude Code's wordings of the prompt and tools.
pub(crate) struct ClaudeModels {
    /// Claude Code's lean prompt.
    pub(crate) standard: ModelProfile,
    /// For models with Claude Code's early-stopping guidance (Haiku 5.5).
    pub(crate) early_stopping: ModelProfile,
    /// The lean prompt naming `<system-reminder>` tags (Opus 4.8).
    pub(crate) tags: ModelProfile,
    /// Claude Code's full prompt, for models without `lean_prompt`.
    pub(crate) full: ModelProfile,
    /// The full prompt naming mid-conversation system turns (Mythos 5).
    pub(crate) full_turns: ModelProfile,
}

impl Services {
    /// The profile for `model` (`None`: its provider has no credential here).
    pub(crate) fn profile(&self, model: &str) -> Option<&ModelProfile> {
        match Provider::of(model) {
            Provider::OpenAi => self.openai.as_ref().map(|o| &o.profile),
            Provider::Anthropic => self.claude.as_ref().map(|c| match models::known(model) {
                Some(m) if m.early_stopping_guidance() => &c.early_stopping,
                Some(m) if !m.lean_prompt() && models::explains_system_turns(model) => &c.full_turns,
                Some(m) if !m.lean_prompt() => &c.full,
                Some(_) if !models::explains_system_turns(model) => &c.tags,
                _ => &c.standard,
            }),
        }
    }

    /// The model id for a name: Claude Code's aliases, `gpt` for the
    /// account's first listed OpenAI model.
    pub(crate) fn resolve_model(&self, name: &str) -> String {
        if name == eos_agent_openai::models::ALIAS {
            let catalog = self.openai.as_ref().map(|o| o.catalog.snapshot());
            return catalog
                .as_ref()
                .and_then(|c| c.default_model())
                .map_or_else(|| models::GPT_FALLBACK.to_owned(), |m| m.slug.clone());
        }
        models::resolve_model(name)
    }

    /// `max_tokens` and the context window for `model`.
    pub(crate) fn limits(&self, model: &str) -> (u32, u64) {
        match self.openai_catalog(model) {
            Some(catalog) => models::openai_limits(&catalog, model),
            None => models::limits(model),
        }
    }

    pub(crate) fn effort_for(&self, model: &str, wanted: Option<&str>) -> Option<String> {
        match self.openai_catalog(model) {
            Some(catalog) => catalog.effort(eos_agent_openai::models::slug(model), wanted),
            None => models::effort_for(model, wanted),
        }
    }

    pub(crate) fn thinking_for(&self, model: &str, summarized: bool, max_tokens: u32) -> Option<Thinking> {
        match self.openai_catalog(model) {
            Some(catalog) => {
                let offers = catalog.get(eos_agent_openai::models::slug(model)).is_none_or(|m| m.summaries);
                Some(Thinking::Adaptive { summarized: summarized && offers })
            }
            None => models::thinking_for(model, summarized, max_tokens),
        }
    }

    /// Whether `model` takes mid-conversation `role: "system"` messages (an
    /// OpenAI request turns them into developer messages either way).
    pub(crate) fn system_turns(&self, model: &str) -> bool {
        Provider::of(model) == Provider::OpenAi || models::takes_system_turns(model)
    }

    /// Claude Code's tool-change betas for `model`: on the Anthropic API for
    /// a model that takes them (its `NAe`; `tengu_foamy_spring`, which gates
    /// both betas, is on by default), unless a refusal so far took them away;
    /// by value only with inline tools on.
    pub(crate) fn tool_changes(&self, model: &str) -> eos_agent_core::ToolChanges {
        use eos_agent_core::ToolChanges;
        if Provider::of(model) != Provider::Anthropic || !models::takes_tool_changes(model) {
            return ToolChanges::InTools;
        }
        let mode = if self.inline_tools { ToolChanges::ByValue } else { ToolChanges::ByReference };
        self.tool_change_refusals.allowed(mode, model)
    }

    /// What the session context says about `model`.
    pub(crate) fn model_info(&self, model: &str) -> ModelInfo {
        match self.openai_catalog(model) {
            Some(catalog) => models::openai_info(&catalog, model),
            None => models::model_info(model),
        }
    }

    /// The catalog, for an OpenAI model (refreshed in the background when stale).
    fn openai_catalog(&self, model: &str) -> Option<Arc<eos_agent_openai::Catalog>> {
        if Provider::of(model) != Provider::OpenAi {
            return None;
        }
        let openai = self.openai.as_ref()?;
        openai.catalog.refresh_if_stale();
        Some(openai.catalog.snapshot())
    }

    /// Claude Code's git snapshot of `cwd`, shared by agents started within
    /// a few seconds of each other.
    pub(crate) async fn git_status(&self, cwd: &Path) -> Option<Arc<str>> {
        if let Some((at, status)) = lock(&self.git).get(cwd)
            && at.elapsed() < GIT_TTL
        {
            return status.clone();
        }
        let dir = cwd.to_path_buf();
        let fresh: Option<Arc<str>> =
            tokio::task::spawn_blocking(move || git::status(&dir)).await.ok().flatten().map(Arc::from);
        let mut cache = lock(&self.git);
        cache.retain(|_, (at, _)| at.elapsed() < GIT_TTL);
        cache.insert(cwd.to_path_buf(), (Instant::now(), fresh.clone()));
        fresh
    }

    /// The shared view of `cwd` with `plugin_dirs`: read from disk on first
    /// use and again whenever a file it came from changed (one `stat` per
    /// file to tell).
    pub(crate) async fn project(self: &Arc<Self>, cwd: &Path, plugin_dirs: &[PathBuf]) -> Arc<Project> {
        let global = self.global().await;
        let key = (cwd.to_path_buf(), plugin_dirs.to_vec());
        if let Some(fresh) = self.cached(&key, &global) {
            return fresh;
        }
        let _loading = self.loading.lock().await;
        if let Some(fresh) = self.cached(&key, &global) {
            return fresh;
        }
        let services = Arc::clone(self);
        let (dir, plugins) = key.clone();
        let loaded = tokio::task::spawn_blocking(move || Project::load(&dir, plugins, &services, global))
            .await
            .map(Arc::new)
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
        lock(&self.projects).insert(key, Arc::clone(&loaded));
        loaded
    }

    fn cached(&self, key: &ProjectKey, global: &Arc<GlobalConfig>) -> Option<Arc<Project>> {
        let project = lock(&self.projects).get(key).cloned()?;
        (Arc::ptr_eq(&project.global, global) && !project.stamps.changed()).then_some(project)
    }

    /// Claude Code's global config, re-read when the file changed.
    async fn global(&self) -> Arc<GlobalConfig> {
        let path = self.paths.global_config.clone();
        let now = stamp(&path);
        {
            let global = lock(&self.global);
            if global.stamp == now {
                return Arc::clone(&global.config);
            }
        }
        let fresh = tokio::task::spawn_blocking(move || Global::read(&path))
            .await
            .unwrap_or_else(|_| Global { stamp: None, config: Arc::default() });
        let mut global = lock(&self.global);
        *global = fresh;
        Arc::clone(&global.config)
    }

    /// Lets a finished subagent go once it has been idle long enough (it
    /// restarts from its transcript when messaged).
    pub(crate) fn release_later(&self, id: &str, generation: u64) {
        let Some(after) = self.release_after else { return };
        let registry = Arc::downgrade(&self.registry);
        let id = id.to_owned();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(after).await;
                let Some(registry) = registry.upgrade() else { return };
                if registry.release(&id, generation) {
                    return;
                }
            }
        });
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
