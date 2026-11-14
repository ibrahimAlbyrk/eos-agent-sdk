//! Each agent's sandbox, as Claude Code decides it for the directory it runs
//! in: the `sandbox` settings of the user, of the working directory's
//! `.claude/settings.json` and `.claude/settings.local.json`, the host's (in
//! the place of Claude Code's `--settings`) and the managed settings, in
//! Claude Code's precedence. Every agent shares the process's one proxy and
//! violation monitor.

use std::path::Path;
use std::sync::Arc;

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{
    BoxFuture, Extensions, PermissionGate, StoredMessage, Tool, ToolContext, ToolOutput, ToolReview, ToolSpec,
};
use eos_agent_tools_shell::sandbox::SandboxUnavailable;
use eos_agent_tools_shell::{SandboxConfig, SandboxDirs, SandboxLayers, SandboxPolicy, SandboxRuntime, sandbox_schema};
use serde_json::Value;
use serde_json::value::RawValue;

/// A directory's sandbox: `Ok(None)` when its settings leave it off (or it
/// cannot run here), `Err` when it is required and cannot run.
pub(crate) type ProjectSandbox = Result<Option<Arc<SandboxPolicy>>, SandboxUnavailable>;

/// The process-wide tiers and the shared runtime.
pub(crate) struct Sandboxes {
    runtime: Arc<SandboxRuntime>,
    user: Option<SandboxConfig>,
    flag: Option<SandboxConfig>,
    managed: Option<SandboxConfig>,
}

impl std::fmt::Debug for Sandboxes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandboxes").field("runtime", &self.runtime).finish_non_exhaustive()
    }
}

impl Sandboxes {
    /// The user's and managed settings documents (`user`, `managed`, lowest
    /// precedence first) and the host's settings. `gate` is asked about
    /// hosts no list decides. Cheap: nothing starts before the first
    /// sandboxed command.
    pub(crate) fn new(
        paths: &ClaudePaths,
        user: Option<&Value>,
        managed: &[Value],
        host: Option<SandboxConfig>,
        gate: Arc<dyn PermissionGate>,
    ) -> Self {
        let dirs = SandboxDirs {
            home: Some(paths.home.clone()),
            config_dir: Some(paths.config_dir.clone()),
            managed_dir: Some(paths.managed_dir.clone()),
        };
        // The user's relative paths start at its config directory.
        let user = user.and_then(SandboxConfig::from_settings).map(|c| c.rebased(&paths.config_dir));
        let managed = managed.iter().filter_map(SandboxConfig::from_settings).reduce(|mut all, doc| {
            all.merge(&doc);
            all
        });
        Self { runtime: SandboxRuntime::new(dirs, gate), user, flag: host, managed }
    }

    /// The sandbox of agents working in `cwd`, from its project settings
    /// `shared` and `local`. Blocking: the first sandbox asks whether the
    /// OS sandbox can run.
    pub(crate) fn for_project(&self, cwd: &Path, shared: Option<&Value>, local: Option<&Value>) -> ProjectSandbox {
        let tier = |doc: Option<&Value>| doc.and_then(SandboxConfig::from_settings).map(|c| c.rebased(cwd));
        let layers = SandboxLayers {
            user: self.user.clone(),
            project: tier(shared),
            local: tier(local),
            flag: self.flag.clone().map(|c| c.rebased(cwd)),
            managed: self.managed.clone().map(|c| c.rebased(cwd)),
        };
        self.runtime.policy(layers.effective())
    }
}

/// Bash or Monitor with Claude Code's sandbox schema (an `allowed_domains`
/// property, and no `dangerouslyDisableSandbox` when unsandboxed commands
/// are not allowed); other tools as they are.
pub(crate) fn with_sandbox_schema(tool: Arc<dyn Tool>, allow_unsandboxed: bool) -> Arc<dyn Tool> {
    let name = tool.spec().name();
    let def: Value = serde_json::from_slice(tool.spec().json()).unwrap_or_default();
    let mut schema = def["input_schema"].clone();
    if !sandbox_schema(name, &mut schema, allow_unsandboxed) {
        return tool;
    }
    let mut spec = ToolSpec::new(name, def["description"].as_str().unwrap_or_default(), schema);
    if tool.spec().is_deferred() {
        spec = spec.deferred();
    }
    if let Some(hint) = tool.spec().search_hint() {
        spec = spec.with_search_hint(hint);
    }
    Arc::new(SandboxSchema { inner: tool, spec })
}

struct SandboxSchema {
    inner: Arc<dyn Tool>,
    spec: ToolSpec,
}

impl std::fmt::Debug for SandboxSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SandboxSchema").field(&self.spec.name()).finish()
    }
}

impl Tool for SandboxSchema {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, input: &RawValue) -> bool {
        self.inner.is_concurrency_safe(input)
    }

    fn runs_sandboxed(&self, input: &RawValue, extensions: &Extensions) -> bool {
        self.inner.runs_sandboxed(input, extensions)
    }

    fn observable_input(&self, input: &RawValue, extensions: &Extensions) -> Option<Arc<RawValue>> {
        self.inner.observable_input(input, extensions)
    }

    fn review<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        self.inner.review(input, ctx)
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        self.inner.call(input, ctx)
    }

    fn restore<'a>(&'a self, history: &'a [StoredMessage], extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        self.inner.restore(history, extensions)
    }
}
