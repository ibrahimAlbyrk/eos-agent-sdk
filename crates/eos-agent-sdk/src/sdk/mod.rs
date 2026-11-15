//! Building what one process shares between all its agents, once.

mod build;
mod builder;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use eos_agent_agents::SpawnLimits;
use eos_agent_anthropic::auth::{FallbackSecrets, KeychainSecrets};
use eos_agent_anthropic::{AnthropicClient, ClientConfig};
use eos_agent_artifacts::{ArtifactOpener, ArtifactWatcher, CapabilitiesGuide, FrameConfig, WatchConfig};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{
    CacheTtl, Credential, ModelClient, ModelError, PermissionGate, ReminderSource, TokenSource, Tool, TranscriptStore,
};
use eos_agent_engine::{AgentHandle, DEFAULT_IDLE_EVICTION, Engine};
use eos_agent_mcp::{AuthPrompter, McpCredentials, McpHub, McpLimits, OAuthOptions, PrintPrompter, XaaIdp};
use eos_agent_openai::{ChatGptLogin, ModelCatalog, OpenAiClient, ToolPlacement};
use eos_agent_prompt::{
    ClaudeCodeCompaction, DateChangeReminder, FileChangeReminders, Fragment, Prompts, request_extra, system_prompt,
    system_prompt_early_stopping, system_prompt_full, system_prompt_full_turns, system_prompt_openai,
    system_prompt_tags,
};
use eos_agent_skills::SkillRenderer;
use eos_agent_tools_session::{
    AskUserQuestion, NoUser, NudgeKind, TaskCreate, TaskGet, TaskList, TaskStore, TaskUpdate, TrackingNudge,
    UserPrompter,
};
use eos_agent_tools_shell::{SandboxConfig, ShellConfig, ShellTools};
use eos_agent_tools_web::{WebFetchConfig, WebFetchTool, WebSearchConfig, WebSearchTool};

use crate::auth::AuthFailover;
use crate::decorate::{Deferred, SpillOver, Undeferred, classic, full_tools};
use crate::gate::{SandboxAutoAllow, TreeGate};
use crate::inline::{Shell, ShellCell};
use crate::models;
use crate::profile::{ModelProfile, Provider};
use crate::registry::{AgentInfo, Registry};
use crate::routing::RoutingClient;
use crate::sandbox::Sandboxes;
use crate::services::{ClaudeModels, Global, OpenAiModels, Services};
use crate::shared::SharedContexts;
use crate::subagents::EngineSubagents;
use crate::watch::{AgentNotes, MainAgentsAsk};
use crate::workflows::WorkflowSettings;
use eos_agent_workflow::WorkflowConfig;

/// Needed to send deferred tools (`defer_loading`) and read `tool_reference` results.
const TOOL_SEARCH_BETA: &str = "advanced-tool-use-2025-11-20";

#[derive(Debug, thiserror::Error)]
pub enum SdkError {
    #[error(
        "no credential: pass one to SdkBuilder::auth (or set ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN), or a ChatGPT \
         sign-in to SdkBuilder::chatgpt_store"
    )]
    NoAuth,
    /// The model runs on a provider this SDK has no credential for.
    #[error("{model} runs on {}, which has no credential here", provider.name())]
    NoProvider { provider: Provider, model: String },
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("artifact tools: {0}")]
    Artifacts(String),
    #[error(transparent)]
    SandboxUnavailable(#[from] eos_agent_tools_shell::sandbox::SandboxUnavailable),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Built once per process; cheap to clone.
#[derive(Clone)]
pub struct Sdk {
    pub(crate) services: Arc<Services>,
}

impl std::fmt::Debug for Sdk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sdk").field("agents", &self.services.registry.len()).finish_non_exhaustive()
    }
}

impl Sdk {
    pub fn builder() -> SdkBuilder {
        SdkBuilder::default()
    }

    /// Agents alive in the registry (top-level ones and their subagents).
    pub fn agent_count(&self) -> usize {
        self.services.registry.len()
    }

    /// Every live agent: top-level ones and the subagents they started.
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.services.registry.infos(None)
    }

    /// Stops the running turn of any live agent, top-level or subagent.
    /// False when no such agent is running.
    pub fn interrupt(&self, id: &str) -> bool {
        self.services.registry.handle(id).inspect(AgentHandle::interrupt).is_some()
    }

    /// The models of the ChatGPT account, read again when the list held is
    /// stale (`None`: OpenAI models are not set up; empty: none was read yet).
    pub async fn openai_models(&self) -> Option<Arc<eos_agent_openai::Catalog>> {
        Some(self.services.openai.as_ref()?.catalog.ensure().await)
    }

    /// The context window an agent on `model` (an id or alias) gets, in tokens.
    pub fn context_window(&self, model: &str) -> u64 {
        self.services.limits(&self.services.resolve_model(model)).1
    }

    /// `id`'s tree: the agent itself and every subagent below it.
    pub fn agent_tree(&self, id: &str) -> Vec<AgentInfo> {
        self.services.registry.infos(Some(id))
    }
}

pub use builder::SdkBuilder;
use builder::{CATALOG_WAIT, CLAUDE_DEFAULT, CLAUDE_SEARCH};

/// Claude Code's store for MCP sign-ins (its Keychain credentials item and
/// the folder of its locks): the one for `store_dir` (a sign-in store, as
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR`), else the one Claude Code itself uses
/// in this environment (`$CLAUDE_SECURESTORAGE_CONFIG_DIR`, then
/// `$CLAUDE_CONFIG_DIR`, then the default item and `~/.claude`).
pub fn claude_mcp_credentials(store_dir: Option<&Path>) -> McpCredentials {
    let var = |k: &str| std::env::var_os(k);
    let home = || var("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from).join(".claude");
    let dir = match store_dir {
        Some(dir) => Some(dir.to_path_buf()),
        None => match var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            Some(d) if d.is_empty() => None,
            Some(d) => Some(PathBuf::from(d)),
            None => var("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()).map(PathBuf::from),
        },
    };
    let keychain = Arc::new(KeychainSecrets::for_claude_store(dir.as_deref()));
    let dir = dir.unwrap_or_else(home);
    // Claude Code reads its plaintext `.credentials.json` when the Keychain item is empty.
    let secrets = Arc::new(FallbackSecrets::new(keychain, dir.join(".credentials.json")));
    let config_dir = var("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()).map_or_else(home, PathBuf::from);
    McpCredentials::new(secrets, dir).with_config_dir(&config_dir)
}
