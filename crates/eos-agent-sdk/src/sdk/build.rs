//! Building the shared services from the builder's settings.

use super::*;

impl SdkBuilder {
    pub async fn build(mut self) -> Result<Sdk, SdkError> {
        let openai = self.openai.take();
        // With OpenAI set up, Claude runs only on a credential given here.
        let claude_auth = match self.auth.take() {
            Some(auth) => Some(auth),
            None if openai.is_none() => eos_agent_anthropic::auth::from_env(),
            None => None,
        };
        if claude_auth.is_none() && openai.is_none() {
            return Err(SdkError::NoAuth);
        }
        if !self.client.betas.iter().any(|b| b == TOOL_SEARCH_BETA) {
            self.client.betas.push(TOOL_SEARCH_BETA.into());
        }
        let anthropic: Option<Arc<dyn ModelClient>> = match &claude_auth {
            Some(auth) => Some(Arc::new(AnthropicClient::new(Arc::clone(auth), self.client.clone())?)),
            None => None,
        };
        let openai_auth = openai.as_ref().map(|setup| Arc::clone(&setup.auth));
        let (openai_client, catalog) = match openai {
            Some(setup) => {
                let client = Arc::new(OpenAiClient::new(setup.auth, setup.client)?);
                let catalog = ModelCatalog::new(Arc::clone(&client), setup.store.as_deref());
                // Read before the first agent starts; a failure (or a slow
                // answer) keeps the cached catalog, or none.
                if setup.read_models_first {
                    let _ = tokio::time::timeout(CATALOG_WAIT, catalog.ensure()).await;
                }
                (Some(client), Some(catalog))
            }
            None => (None, None),
        };
        let others = Arc::new(Providers::default());
        let mut router = RoutingClient::new(Arc::clone(&others));
        let claude_id = ProviderId::new(eos_agent_anthropic::PROVIDER);
        if let Some(client) = anthropic {
            router.add(claude_id.clone(), client);
        }
        if let Some(client) = &openai_client {
            router.add(ProviderId::new(eos_agent_openai::PROVIDER), Arc::clone(client) as Arc<dyn ModelClient>);
        }
        let has_claude = router.has(&claude_id);
        let client: Arc<dyn ModelClient> = Arc::new(router);
        let default_model = self
            .model
            .take()
            .unwrap_or_else(|| if has_claude { CLAUDE_DEFAULT } else { eos_agent_openai::models::ALIAS }.into());
        let mut engine = Engine::builder(Arc::clone(&client)).idle_eviction(self.idle_eviction);
        let host_gate = self.permissions.take();
        let store = self.store.take();
        if let Some(store) = &store {
            engine = engine.store(Arc::clone(store));
        }
        let (artifact_auth, artifacts) = self.artifact_credential(claude_auth.as_ref()).await?;
        let paths = self.paths.take().or_else(ClaudePaths::from_env).unwrap_or_else(|| {
            let home = std::env::temp_dir().join("eos-agent-home");
            ClaudePaths::for_home(&home)
        });
        let global_path = paths.global_config.clone();
        let global = tokio::task::spawn_blocking(move || Global::read(&global_path))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let data_dir = self.data_dir.take().unwrap_or_else(|| std::env::temp_dir().join("eos-agent"));
        let tasks = TaskStore::new(data_dir.join("tasks"));
        let registry: Arc<Registry> = Arc::default();
        if let Some(after) = self.task_list_retention {
            registry.set_retention(after);
        }
        let settings_paths = paths.clone();
        let (user_settings, managed_settings) = tokio::task::spawn_blocking(move || {
            (eos_agent_compat::user_settings(&settings_paths), eos_agent_compat::managed_settings(&settings_paths))
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        let settings: Vec<serde_json::Value> = user_settings.iter().chain(&managed_settings).cloned().collect();
        let tree = |inner| Arc::new(TreeGate { inner, registry: Arc::downgrade(&registry) }) as Arc<dyn PermissionGate>;
        let network_gate = tree(host_gate.clone().unwrap_or_else(|| Arc::new(eos_agent_core::AllowAll)));
        let sandboxes =
            Sandboxes::new(&paths, user_settings.as_ref(), &managed_settings, self.sandbox.take(), network_gate);
        let budgets: Arc<crate::workflows::Budgets> = Arc::default();
        let workflow_settings = WorkflowSettings::read(&settings, |k| std::env::var(k).ok());
        let mut protected = vec![data_dir.clone()];
        protected.extend(self.workflow_dir.clone());
        protected.append(&mut self.protected);
        let workflows = self.workflows.unwrap_or(workflow_settings.enabled).then(|| {
            let mut config = WorkflowConfig::new(self.workflow_dir.take().unwrap_or_else(|| data_dir.clone()));
            config.size = workflow_settings.size;
            config.size_is_default = workflow_settings.size_is_default;
            config.budgets = Some(Arc::clone(&budgets) as Arc<dyn eos_agent_workflow::TokenBudgets>);
            config
        });
        // Each agent's own sandbox settings decide whether its sandboxed
        // commands skip the host's gate (`autoAllowBashIfSandboxed`).
        let gate = tree(match host_gate {
            Some(gate) if gate.lets_sandboxed_commands_through() => {
                Arc::new(SandboxAutoAllow::new(gate, Arc::downgrade(&registry)))
            }
            Some(gate) => gate,
            None => Arc::new(eos_agent_core::AllowAll),
        });
        engine = engine.permissions(Arc::clone(&gate));
        let notes =
            AgentNotes { registry: Arc::downgrade(&registry), client: Arc::clone(&client), gate: Arc::clone(&gate) };
        let (shared_tools, artifact_kit) =
            self.base_tools(artifact_auth.as_ref(), &data_dir, &tasks, artifacts, notes)?;
        // Skills' inline commands run as Bash calls through the same gate.
        let shell = Arc::new(ShellCell::default());
        if let Some(bash) = shared_tools.iter().find(|t| t.spec().name() == "Bash") {
            shell.set(Shell { tool: Arc::clone(bash), gate });
        }
        let (watcher, artifact_skills) = match artifact_kit {
            Some((watcher, guide)) => {
                (Some(watcher), Some(Arc::new(CapabilitiesSkill(guide)) as Arc<dyn SkillRenderer>))
            }
            None => (None, None),
        };
        // WebSearch runs on Anthropic's server tool, so on a Claude model
        // whatever model the agent itself is on.
        let search_model = if Provider::of(&default_model) == Provider::Anthropic {
            models::resolve_model(&default_model)
        } else {
            CLAUDE_SEARCH.into()
        };
        let search = has_claude.then(|| self.search_tool(&client, &search_model));
        let claude = has_claude.then(|| {
            let mut tools = shared_tools.clone();
            tools.push(self.fetch_tool(&client, None));
            tools.extend(search.clone());
            self.claude_profiles(tools)
        });
        let openai = catalog.map(|catalog| {
            let snapshot = catalog.snapshot();
            let utility = snapshot.small_model().map_or_else(|| models::GPT_FALLBACK.to_owned(), |m| m.slug.clone());
            let mut tools = shared_tools.clone();
            tools.push(self.fetch_tool(&client, Some(utility)));
            tools.extend(search.clone());
            OpenAiModels { profile: self.openai_profile(&tools), catalog }
        });
        // Pages are summarized by Claude Code's small model, as for Claude agents.
        let mut other_tools = shared_tools.clone();
        other_tools.push(self.fetch_tool(&client, None));
        other_tools.extend(search.clone());
        let other_profile = self.other_profile(&other_tools);
        let reminders: Vec<Arc<dyn ReminderSource>> = vec![
            Arc::new(FileChangeReminders::new(&self.prompts)),
            Arc::new(DateChangeReminder::new(&self.prompts)),
            Arc::new(TrackingNudge::new(NudgeKind::Tasks(tasks))),
        ];
        let services = Arc::new_cyclic(|weak: &Weak<Services>| Services {
            engine: engine.build(),
            client,
            claude,
            openai,
            claude_auth,
            openai_auth,
            others,
            other_profile,
            auth_failover: self.auth_failover,
            compaction: Arc::new(ClaudeCodeCompaction::new(&self.prompts)),
            prompts: self.prompts,
            reminders,
            hub: McpHub::with_oauth_options(
                self.mcp,
                self.mcp_credentials.take().unwrap_or_else(|| claude_mcp_credentials(None)),
                Arc::new(MainAgentsAsk {
                    inner: self.mcp_prompter.take().unwrap_or_else(|| Arc::new(PrintPrompter::default())),
                    registry: Arc::downgrade(&registry),
                }),
                // Claude Code's `xaaIdp` setting (the last settings file that sets it).
                OAuthOptions {
                    xaa_idp: settings.iter().rev().find_map(|doc| XaaIdp::from_settings(doc.get("xaaIdp")?)),
                    ..OAuthOptions::default()
                },
            ),
            paths,
            registry,
            watcher,
            artifact_skills,
            subagents: Arc::new(EngineSubagents::new(weak.clone())),
            data_dir,
            default_model,
            release_after: store.as_ref().and(self.idle_eviction),
            store,
            global: Mutex::new(global),
            projects: Mutex::default(),
            loading: tokio::sync::Mutex::default(),
            git: Mutex::default(),
            shared: SharedContexts::default(),
            sandboxes,
            workflows,
            workflow_keyword: workflow_settings.keyword_trigger,
            spawn_limits: self.spawn_limits.unwrap_or_else(|| SpawnLimits::from_env(|k| std::env::var(k).ok())),
            budgets,
            running_tasks: Mutex::default(),
            shell,
            tool_change_refusals: Arc::default(),
            inline_tools: self.inline_tools.unwrap_or_else(crate::services::inline_tools_from_env),
            protected,
        });
        Ok(Sdk { services })
    }

    /// Claude Code's two wordings of the prompt and tools.
    fn claude_profiles(&self, base_tools: Vec<Arc<dyn Tool>>) -> ClaudeModels {
        let texts: Vec<Arc<str>> =
            [Fragment::Identity, Fragment::System].iter().map(|f| Arc::from(self.prompts.get(*f))).collect();
        let profile = |system, base_tools: Vec<Arc<dyn Tool>>, full_wording| ModelProfile {
            provider: Provider::Anthropic,
            system,
            system_texts: texts.clone(),
            identity: Arc::from(self.prompts.get(Fragment::Identity)),
            subagent_suffix: None,
            base_tools: base_tools.into(),
            request_extra: Some(request_extra()),
            cache_ttl: Some(CacheTtl::OneHour),
            defer_tools: true,
            full_wording,
        };
        ClaudeModels {
            early_stopping: profile(system_prompt_early_stopping(&self.prompts), classic(&base_tools), false),
            tags: profile(system_prompt_tags(&self.prompts), base_tools.clone(), false),
            full_turns: profile(system_prompt_full_turns(&self.prompts), full_tools(&base_tools), true),
            full: profile(system_prompt_full(&self.prompts), full_tools(&base_tools), true),
            standard: profile(system_prompt(&self.prompts), base_tools, false),
        }
    }

    /// GPT models: the Codex-style prompt, every tool sent in full (no tool
    /// search), Bash worded to steer away from `cat` and `sed`.
    fn openai_profile(&self, tools: &[Arc<dyn Tool>]) -> ModelProfile {
        let texts: Vec<Arc<str>> =
            [Fragment::OpenAiBase, Fragment::OpenAiTools].iter().map(|f| Arc::from(self.prompts.get(*f))).collect();
        ModelProfile {
            provider: Provider::OpenAi,
            system: system_prompt_openai(&self.prompts),
            system_texts: texts,
            identity: Arc::from(self.prompts.get(Fragment::OpenAiIdentity)),
            subagent_suffix: Some(Arc::from(self.prompts.get(Fragment::OpenAiTools))),
            base_tools: classic(tools).into_iter().map(Undeferred::wrap).collect(),
            request_extra: None,
            cache_ttl: None,
            defer_tools: false,
            full_wording: false,
        }
    }

    /// The prompt and tools of a model on a provider the host configured:
    /// the OpenAI ones, without naming a model family.
    fn other_profile(&self, tools: &[Arc<dyn Tool>]) -> ModelProfile {
        let neutral = |f: Fragment| self.prompts.get(f).replacen(" running on an OpenAI GPT model", "", 1);
        let (base, tool_notes) = (neutral(Fragment::OpenAiBase), self.prompts.get(Fragment::OpenAiTools));
        ModelProfile {
            provider: Provider::Other,
            system: SystemPrompt::new(&[(&base, None), (tool_notes, None)]),
            system_texts: vec![Arc::from(base.as_str()), Arc::from(tool_notes)],
            identity: Arc::from(neutral(Fragment::OpenAiIdentity)),
            ..self.openai_profile(tools)
        }
    }

    /// WebFetch, summarizing pages with Claude Code's small model, or with
    /// `utility` (an OpenAI model, which gets no Claude identity line).
    fn fetch_tool(&self, client: &Arc<dyn ModelClient>, utility: Option<String>) -> Arc<dyn Tool> {
        let mut config = self.web_fetch.clone();
        if let Some(model) = utility {
            config.side.model = model;
            config.side.identity = None;
        }
        config.side.always_thinks = models::thinking_off(&config.side.model, None).thinks;
        SpillOver::wrap(defer(Arc::new(WebFetchTool::new(Arc::clone(client), config))))
    }

    fn search_tool(&self, client: &Arc<dyn ModelClient>, model: &str) -> Arc<dyn Tool> {
        let mut config = WebSearchConfig::new(model);
        config.side.always_thinks = models::thinking_off(model, None).thinks;
        SpillOver::wrap(defer(Arc::new(WebSearchTool::new(Arc::clone(client), config))))
    }

    /// The artifact tools' credential and whether they are on. A separate
    /// artifact credential that cannot be read leaves them off unless they
    /// were turned on: the model's credential still works.
    async fn artifact_credential(
        &mut self,
        claude: Option<&Arc<dyn TokenSource>>,
    ) -> Result<(Option<Arc<dyn TokenSource>>, bool), SdkError> {
        let own = self.artifact_auth.take();
        let auth = own.clone().or_else(|| claude.cloned());
        let on = match (self.artifacts, own, &auth) {
            // The frame API takes only a Claude credential.
            (_, _, None) => false,
            (Some(on), _, _) => on,
            (None, None, Some(auth)) => matches!(auth.credential().await?, Credential::Bearer(_)),
            (None, Some(own), _) => matches!(own.credential().await, Ok(Credential::Bearer(_))),
        };
        Ok((auth, on))
    }

    /// The tools every agent of the process shares (the web tools apart:
    /// they differ per provider), decorated as Claude Code sends them.
    fn base_tools(
        &mut self,
        artifact_auth: Option<&Arc<dyn TokenSource>>,
        data_dir: &Path,
        tasks: &TaskStore,
        artifacts: bool,
        notes: AgentNotes,
    ) -> Result<BaseTools, SdkError> {
        let mut tools = eos_agent_tools_fs::tools();
        tools.extend(ShellTools::new(ShellConfig::new(data_dir.join("shell-snapshots"))));
        self.web_fetch.artifact_tool = artifacts;
        tools.push(Arc::new(TaskCreate::new(tasks.clone()).deferred()));
        tools.push(Arc::new(TaskGet::new(tasks.clone()).deferred()));
        tools.push(Arc::new(TaskUpdate::new(tasks.clone()).deferred()));
        tools.push(Arc::new(TaskList::new(tasks.clone()).deferred()));
        tools.push(Arc::new(AskUserQuestion::new(Arc::clone(&self.prompter))));
        let mut kit = None;
        if let Some(artifact_auth) = artifact_auth.filter(|_| artifacts) {
            let notes = Arc::new(notes);
            let (frame, watch) = self.artifact_api.clone();
            // Claude Code's `<tmp>/bundled-skills/<version>/<random>/<skill>`: one folder per process.
            let skill_dir = data_dir
                .join("bundled-skills")
                .join(uuid::Uuid::new_v4().simple().to_string())
                .join("artifact-capabilities");
            let artifact_kit = eos_agent_artifacts::kit(
                Arc::clone(artifact_auth),
                frame,
                notes,
                watch,
                skill_dir,
                self.artifact_opener.clone(),
            )
            .map_err(|e| SdkError::Artifacts(e.to_string()))?;
            tools.extend(artifact_kit.tools);
            kit = Some((artifact_kit.watcher, artifact_kit.capabilities));
        }
        Ok((tools.into_iter().map(|t| SpillOver::wrap(defer(t))).collect(), kit))
    }
}

/// The shared tools, and with the artifact tools their watcher and the
/// `artifact-capabilities` skill's composer.
type BaseTools = (Vec<Arc<dyn Tool>>, Option<(ArtifactWatcher, CapabilitiesGuide)>);

/// The bundled `artifact-capabilities` skill, composed from the frame API.
#[derive(Debug)]
struct CapabilitiesSkill(CapabilitiesGuide);

impl SkillRenderer for CapabilitiesSkill {
    fn render<'a>(
        &'a self,
        _args: &'a str,
        call: Option<eos_agent_core::ToolContext<'a>>,
    ) -> eos_agent_core::BoxFuture<'a, String> {
        Box::pin(self.0.render_for(call.map(|c| (c.extensions, c.agent_id))))
    }
}

/// Claude Code defers these behind ToolSearch; the crates send them in full.
fn defer(tool: Arc<dyn Tool>) -> Arc<dyn Tool> {
    let hint = match tool.spec().name() {
        "NotebookEdit" => "edit Jupyter notebook cells",
        "Monitor" => "stream a command's output lines as events",
        "TaskStop" => "stop a running background task",
        _ => return tool,
    };
    Deferred::wrap(tool, hint)
}
