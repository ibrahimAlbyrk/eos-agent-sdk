//! The builder's settings.

use super::*;

pub struct SdkBuilder {
    pub(super) auth: Option<Arc<dyn TokenSource>>,
    pub(super) artifact_auth: Option<Arc<dyn TokenSource>>,
    pub(super) auth_failover: Option<Arc<dyn AuthFailover>>,
    pub(super) client: ClientConfig,
    pub(super) openai: Option<OpenAiSetup>,
    pub(super) model: Option<String>,
    pub(super) permissions: Option<Arc<dyn PermissionGate>>,
    pub(super) store: Option<Arc<dyn TranscriptStore>>,
    pub(super) prompter: Arc<dyn UserPrompter>,
    pub(super) data_dir: Option<PathBuf>,
    pub(super) paths: Option<ClaudePaths>,
    pub(super) artifacts: Option<bool>,
    pub(super) artifact_api: (FrameConfig, WatchConfig),
    pub(super) prompts: Prompts,
    pub(super) web_fetch: WebFetchConfig,
    pub(super) mcp: McpLimits,
    pub(super) mcp_credentials: Option<McpCredentials>,
    pub(super) mcp_prompter: Option<Arc<dyn AuthPrompter>>,
    pub(super) idle_eviction: Option<Duration>,
    pub(super) sandbox: Option<SandboxConfig>,
    pub(super) workflows: Option<bool>,
    pub(super) workflow_dir: Option<PathBuf>,
    pub(super) spawn_limits: Option<SpawnLimits>,
    pub(super) inline_tools: Option<bool>,
    pub(super) task_list_retention: Option<Duration>,
    pub(super) protected: Vec<PathBuf>,
}

impl std::fmt::Debug for SdkBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkBuilder")
            .field("model", &self.model)
            .field("openai", &self.openai.is_some())
            .finish_non_exhaustive()
    }
}

/// A ChatGPT plan's credential, where its catalog is kept, how to reach the API.
pub(super) struct OpenAiSetup {
    pub(super) auth: Arc<dyn TokenSource>,
    pub(super) store: Option<PathBuf>,
    pub(super) client: eos_agent_openai::ClientConfig,
}

/// How long building waits for the OpenAI model catalog.
pub(super) const CATALOG_WAIT: Duration = Duration::from_secs(10);
/// The default model with a Claude credential.
pub(super) const CLAUDE_DEFAULT: &str = "claude-opus-5-5";
/// WebSearch's model when agents default to an OpenAI model (the search
/// runs on Anthropic's server tool).
pub(super) const CLAUDE_SEARCH: &str = "claude-sonnet-5-5";

impl Default for SdkBuilder {
    fn default() -> Self {
        Self {
            auth: None,
            artifact_auth: None,
            auth_failover: None,
            client: ClientConfig::default(),
            openai: None,
            model: None,
            permissions: None,
            store: None,
            prompter: Arc::new(NoUser),
            data_dir: None,
            paths: None,
            artifacts: None,
            artifact_api: (FrameConfig::default(), WatchConfig::default()),
            prompts: Prompts::default(),
            web_fetch: WebFetchConfig::default(),
            mcp: McpLimits::default(),
            mcp_credentials: None,
            mcp_prompter: None,
            idle_eviction: Some(DEFAULT_IDLE_EVICTION),
            sandbox: None,
            workflows: None,
            workflow_dir: None,
            spawn_limits: None,
            inline_tools: None,
            task_list_retention: None,
            protected: Vec::new(),
        }
    }
}

impl SdkBuilder {
    /// The Claude credential; without it, `ANTHROPIC_API_KEY` /
    /// `ANTHROPIC_AUTH_TOKEN` (only when no OpenAI credential is set).
    pub fn auth(mut self, auth: Arc<dyn TokenSource>) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Who is asked for another credential when an agent's stops working
    /// (its plan's limit is reached, its sign-in is over), before the
    /// agent's turn fails. Without one such a turn fails at once.
    pub fn auth_failover(mut self, failover: Arc<dyn AuthFailover>) -> Self {
        self.auth_failover = Some(failover);
        self
    }

    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.client.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }

    /// Speak HTTP/2 to a plain-HTTP `base_url` (a local h2c endpoint).
    pub fn http2_prior_knowledge(mut self, on: bool) -> Self {
        self.client.http2_prior_knowledge = on;
        if let Some(openai) = &mut self.openai {
            openai.client.http2_prior_knowledge = on;
        }
        self
    }

    /// Agents' model unless their options say otherwise (also WebSearch's).
    /// Default: `claude-opus-5-5` with a Claude credential, else `gpt` (the
    /// ChatGPT account's first listed model).
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// OpenAI models on a ChatGPT plan, with this credential (a bearer
    /// token source). Agents and subagents whose model is an OpenAI one
    /// (`gpt-*`, `o3`, `openai/<slug>`, `gpt`) run there; a conversation never
    /// changes provider.
    pub fn openai(mut self, auth: Arc<dyn TokenSource>) -> Self {
        self.openai_setup().auth = auth;
        self
    }

    /// OpenAI models on the ChatGPT sign-in in `store_dir` (written by
    /// `eos-agent login chatgpt` or [`eos_agent_openai::sign_in`]); the
    /// account's model catalog is kept there too.
    pub fn chatgpt_store(mut self, store_dir: impl Into<PathBuf>) -> Self {
        let dir = store_dir.into();
        let setup = self.openai_setup();
        setup.auth = Arc::new(ChatGptLogin::new(&dir));
        setup.store = Some(dir);
        self
    }

    /// Where OpenAI requests go (default `https://api.openai.com/v1`).
    pub fn openai_base_url(mut self, url: impl Into<String>) -> Self {
        self.openai_setup().client.base_url = url.into().trim_end_matches('/').to_owned();
        self
    }

    /// How function tools are placed in OpenAI requests (default: one namespace).
    pub fn openai_tool_placement(mut self, placement: ToolPlacement) -> Self {
        self.openai_setup().client.tools = placement;
        self
    }

    fn openai_setup(&mut self) -> &mut OpenAiSetup {
        let http2 = self.client.http2_prior_knowledge;
        self.openai.get_or_insert_with(|| OpenAiSetup {
            auth: Arc::new(ChatGptLogin::new(std::env::temp_dir().join("eos-agent-chatgpt"))),
            store: None,
            client: eos_agent_openai::ClientConfig { http2_prior_knowledge: http2, ..Default::default() },
        })
    }

    pub fn permissions(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.permissions = Some(gate);
        self
    }

    pub fn store(mut self, store: Arc<dyn TranscriptStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Who answers AskUserQuestion (default: nobody, the tool says so).
    pub fn prompter(mut self, prompter: Arc<dyn UserPrompter>) -> Self {
        self.prompter = prompter;
        self
    }

    /// Task lists, shell snapshots and agents' scratch space (default: a folder in the temp dir).
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// A folder (or file) no tool writes outside an agent's working
    /// directories, whatever the permission gate allows: the host's own data,
    /// sign-in stores. The data dir and the workflow folder are always
    /// protected. Writes elsewhere are the gate's to decide.
    pub fn protect_from_writes(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected.push(path.into());
        self
    }

    /// Where Claude Code's files are (default: from `$HOME` / `$CLAUDE_CONFIG_DIR`).
    pub fn claude_paths(mut self, paths: ClaudePaths) -> Self {
        self.paths = Some(paths);
        self
    }

    /// The artifact tools' own credential (default: the model's). The frame
    /// API takes only a Claude sign-in, so a model billed on an API key can
    /// still publish through one.
    pub fn artifact_auth(mut self, auth: Arc<dyn TokenSource>) -> Self {
        self.artifact_auth = Some(auth);
        self
    }

    /// Artifact tools on or off (default: on exactly when the artifact
    /// credential, see [`artifact_auth`](Self::artifact_auth), is a Claude sign-in).
    pub fn artifacts(mut self, on: bool) -> Self {
        self.artifacts = Some(on);
        self
    }

    /// Where the artifact tools and their watches connect (default:
    /// claude.ai's frame API and live channel).
    pub fn artifact_api(mut self, frame: FrameConfig, watch: WatchConfig) -> Self {
        self.artifact_api = (frame, watch);
        self
    }

    pub fn prompts(mut self, prompts: Prompts) -> Self {
        self.prompts = prompts;
        self
    }

    pub fn web_fetch(mut self, config: WebFetchConfig) -> Self {
        self.web_fetch = config;
        self
    }

    pub fn mcp_limits(mut self, limits: McpLimits) -> Self {
        self.mcp = limits;
        self
    }

    /// Where MCP sign-ins are kept (default: Claude Code's credentials item,
    /// see [`claude_mcp_credentials`], so its logins work here and back).
    pub fn mcp_credentials(mut self, credentials: McpCredentials) -> Self {
        self.mcp_credentials = Some(credentials);
        self
    }

    /// How the user is sent to authorize an MCP server (default: the address
    /// is printed to stderr).
    pub fn mcp_prompter(mut self, prompter: Arc<dyn AuthPrompter>) -> Self {
        self.mcp_prompter = Some(prompter);
        self
    }

    /// How long an agent may sit idle before its history leaves memory, and a
    /// finished subagent before it is let go (`None` = never; default 10
    /// minutes). Needs a [`store`](Self::store) to page them back from.
    pub fn idle_eviction(mut self, after: Option<Duration>) -> Self {
        self.idle_eviction = after;
        self
    }

    /// Claude Code's `sandbox` settings for every agent's shell commands, in
    /// the place of Claude Code's `--settings`: over the user's
    /// `~/.claude/settings.json` and the agent's directory's
    /// `.claude/settings.json` and `.claude/settings.local.json`, under the
    /// managed settings (set values win, lists are joined, and the grants a
    /// repository could widen the sandbox with count only as Claude Code
    /// lets them). The sandbox is off unless some tier turns it on.
    pub fn sandbox(mut self, config: SandboxConfig) -> Self {
        self.sandbox = Some(config);
        self
    }

    /// Claude Code's Workflow tool on or off for top-level agents (default:
    /// as Claude Code decides it — on, unless `CLAUDE_CODE_DISABLE_WORKFLOWS`,
    /// `CLAUDE_CODE_WORKFLOWS=0` or the `disableWorkflows` /
    /// `enableWorkflows` settings turn it off). Subagents never get it.
    pub fn workflows(mut self, on: bool) -> Self {
        self.workflows = Some(on);
        self
    }

    /// Whether subagents may run in the background and how deep they may
    /// nest (default: as Claude Code reads `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`
    /// and `CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`, 3 when unset).
    pub fn spawn_limits(mut self, limits: SpawnLimits) -> Self {
        self.spawn_limits = Some(limits);
        self
    }

    /// Whether tools that arrive mid-conversation carry their definitions
    /// (Claude Code's `inline-tools` beta) instead of being added by
    /// reference. Default: on, unless `CLAUDE_CODE_INLINE_TOOLS` turns it
    /// off or a `CCR_SESSION_PROFILE` session forbids it, as Claude Code
    /// behaves on accounts with its `tengu_brisk_meadow` flag on.
    pub fn inline_tools(mut self, on: bool) -> Self {
        self.inline_tools = Some(on);
        self
    }

    /// How long a finished subagent stays on the task list ListAgents shows
    /// (Claude Code's 30 s); for tests.
    #[doc(hidden)]
    pub fn task_list_retention(mut self, after: Duration) -> Self {
        self.task_list_retention = Some(after);
        self
    }

    /// Where workflow runs keep their files, in Claude Code's layout under
    /// `<dir>/<project>/<session id>/`, `<project>` being Claude Code's name
    /// for the working directory's folder (default: the data dir).
    pub fn workflow_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.workflow_dir = Some(dir.into());
        self
    }
}
