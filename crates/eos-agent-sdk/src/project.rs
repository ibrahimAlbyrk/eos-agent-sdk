//! What every agent working in one directory shares, read from disk once and
//! again whenever one of the files it came from changes: its instruction
//! files, skills, agent types, MCP configs and the tools that depend on them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use eos_agent_agents::{
    AgentDefinitions, AgentTool, ListAgentsTool, SearchTools, SendMessageTool, agent_roots, builtin_definitions,
    discover,
};
use eos_agent_compat::{ClaudePaths, GlobalConfig, McpDocuments, MemoryScope, discover_claude_md, mcp_documents};
use eos_agent_core::{Tool, Toolbox};
use eos_agent_prompt::{InstructionFile, InstructionScope};
use eos_agent_skills::{CommandRunner, SkillCatalog, SkillRoots, SkillTool};
use eos_agent_workflow::WorkflowTool;

use crate::decorate::SpillOver;
use crate::host::HOST_RESULT_LIMIT;
use crate::sandbox::{ProjectSandbox, with_sandbox_schema};
use crate::services::Services;
use crate::stamps::Stamps;
use crate::toolbox::{self, ToolFilter, ToolRecipe};

/// The identities of an agent's base tool set, MCP tools and host tools, and
/// its tool filter.
#[derive(PartialEq, Eq, Hash)]
struct ToolboxKey {
    base: usize,
    mcp: Vec<usize>,
    host: Vec<usize>,
    filter: ToolFilter,
    defer: bool,
}

pub(crate) struct Project {
    pub(crate) cwd: PathBuf,
    /// Plugin folders loaded as they are, beside the installed plugins.
    pub(crate) plugin_dirs: Vec<PathBuf>,
    /// Claude Code's global config as it was when this was read.
    pub(crate) global: Arc<GlobalConfig>,
    /// The files and folders all of this came from.
    pub(crate) stamps: Stamps,
    pub(crate) instructions: Vec<InstructionFile>,
    pub(crate) skills: Arc<SkillCatalog>,
    pub(crate) definitions: Arc<AgentDefinitions>,
    /// Skill, Agent, SendMessage, ListAgents and Workflow, bound to this directory.
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) mcp: McpDocuments,
    /// The sandbox of agents working here (Claude Code's `sandbox` settings
    /// of this directory over the user's, the host's and the managed ones).
    pub(crate) sandbox: ProjectSandbox,
    /// Agents with the same tools share one toolbox (one serialized `tools`
    /// array); keyed by the identity of their MCP and host tools and their filter.
    toolboxes: Mutex<HashMap<ToolboxKey, Weak<Toolbox>>>,
}

impl std::fmt::Debug for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Project").field("cwd", &self.cwd).finish_non_exhaustive()
    }
}

impl Project {
    /// Blocking I/O: CLAUDE.md files, skill and agent files, `.mcp.json`,
    /// the project's settings.
    pub(crate) fn load(cwd: &Path, plugin_dirs: Vec<PathBuf>, services: &Services, global: Arc<GlobalConfig>) -> Self {
        let paths = &services.paths;
        let subagents = Arc::clone(&services.subagents);
        let artifact_skills = services.artifact_skills.clone();
        let workflows = services.workflows.as_ref();
        let runner: Arc<dyn CommandRunner> = Arc::new(services.shell.runner());
        let (shared, local) = eos_agent_compat::project_settings(cwd);
        let sandbox = services.sandboxes.for_project(cwd, shared.as_ref(), local.as_ref());
        let instructions: Vec<InstructionFile> = discover_claude_md(paths, cwd, global.allows_external_imports(cwd))
            .into_iter()
            .map(|f| InstructionFile { path: f.path, scope: scope(f.scope), content: f.content })
            .collect();
        let roots = SkillRoots {
            user_dir: Some(paths.config_dir.clone()),
            cwd: Some(cwd.to_path_buf()),
            home: Some(paths.home.clone()),
            plugin_dirs: plugin_dirs.clone(),
            installed_plugins: true,
            bundled: true,
            artifacts: artifact_skills,
            extra: workflows.map(|_| crate::workflows::authoring_skill()).into_iter().collect(),
        };
        let skills = Arc::new(SkillCatalog::discover(&roots));
        let repo_root = cwd.ancestors().find(|d| d.join(".git").exists());
        let found = discover(&agent_roots(&paths.home, cwd, repo_root));
        let definitions = Arc::new(AgentDefinitions::new(
            found.definitions.into_iter().chain(builtin_definitions(SearchTools::Dedicated)),
        ));
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(SkillTool::new(Arc::clone(&skills), runner).with_subagents(Arc::clone(&subagents))),
            Arc::new(
                AgentTool::new(Arc::clone(&subagents), Arc::clone(&definitions)).with_limits(services.spawn_limits),
            ),
            Arc::new(SendMessageTool::new(Arc::clone(&subagents))),
        ];
        if let Some(config) = workflows {
            let mut config = config.clone();
            // Saved workflows are read from Claude Code's config directory;
            // runs keep their files in the project's folder, by session.
            config.user_dir = Some(paths.config_dir.clone());
            config.root = config.root.join(eos_agent_compat::project_dir_name(cwd));
            tools.push(Arc::new(WorkflowTool::new(config, Arc::clone(&subagents))));
        }
        tools.push(Arc::new(ListAgentsTool::new(subagents)));
        let read: Vec<PathBuf> = instructions
            .iter()
            .map(|f| f.path.clone())
            .chain(skills.skills().iter().filter_map(|s| s.base_dir.as_ref().map(|d| d.join("SKILL.md"))))
            .chain(definitions.iter().filter_map(|d| d.path.clone()))
            .collect();
        Self {
            cwd: cwd.to_path_buf(),
            stamps: Stamps::of(watched(cwd, paths).into_iter().chain(plugin_parts(&plugin_dirs)).chain(read)),
            plugin_dirs,
            mcp: mcp_documents(&global, cwd),
            sandbox,
            global,
            instructions,
            skills,
            definitions,
            tools: tools.into_iter().map(SpillOver::wrap).collect(),
            toolboxes: Mutex::default(),
        }
    }

    /// The toolbox for `recipe` (the process-wide tools and the host's), this
    /// project's tools and `mcp`, shared with every live agent that has the
    /// same set.
    pub(crate) fn toolbox(&self, recipe: &ToolRecipe, mcp: Vec<Arc<dyn Tool>>) -> Arc<Toolbox> {
        let id = |tool: &Arc<dyn Tool>| Arc::as_ptr(tool).cast::<()>() as usize;
        // `base` differs between wordings, so it keys the wording too.
        let key = ToolboxKey {
            base: recipe.base.as_ptr() as usize,
            mcp: mcp.iter().map(id).collect(),
            host: recipe.host.iter().map(|t| id(&t.tool)).collect(),
            filter: recipe.filter.clone(),
            defer: recipe.defer,
        };
        let mut cache = self.toolboxes.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(toolbox) = cache.get(&key).and_then(Weak::upgrade) {
            return toolbox;
        }
        cache.retain(|_, w| w.strong_count() > 0);
        let own = if recipe.full_wording { crate::decorate::full_tools(&self.tools) } else { self.tools.clone() };
        // With the sandbox on, Bash and Monitor take Claude Code's sandbox schema.
        let schema = self.policy().map(|p| p.config().allow_unsandboxed_commands());
        let base = recipe.base.iter().map(|t| match schema {
            Some(allow_unsandboxed) => with_sandbox_schema(Arc::clone(t), allow_unsandboxed),
            None => Arc::clone(t),
        });
        // A host tool takes the place of a built-in one of its name.
        let replaced = |t: &Arc<dyn Tool>| recipe.host.iter().any(|h| h.name() == t.spec().name());
        let host = recipe.host.iter().map(|t| SpillOver::with_limit(Arc::clone(&t.tool), HOST_RESULT_LIMIT));
        let all = base.chain(own).chain(mcp).filter(|t| !replaced(t)).chain(host);
        let toolbox = Arc::new(toolbox::compose(all.filter(|t| recipe.filter.allows(t.spec().name())), recipe.defer));
        cache.insert(key, Arc::downgrade(&toolbox));
        toolbox
    }
}

impl Project {
    /// The sandbox of agents working here, when it is on.
    pub(crate) fn policy(&self) -> Option<&Arc<eos_agent_tools_shell::SandboxPolicy>> {
        self.sandbox.as_ref().ok().and_then(Option::as_ref)
    }
}

/// Every place an instruction file, skill, command, agent type or MCP config
/// could appear for `cwd`, present or not.
fn watched(cwd: &Path, paths: &ClaudePaths) -> Vec<PathBuf> {
    let user = &paths.config_dir;
    let mut out = vec![
        paths.managed_dir.join("CLAUDE.md"),
        paths.managed_dir.join(".claude/rules"),
        user.join("CLAUDE.md"),
        user.join("rules"),
        user.join("skills"),
        user.join("commands"),
        user.join("agents"),
        user.join("settings.json"),
        user.join("plugins/installed_plugins.json"),
        paths.home.join(".claude/agents"),
    ];
    for dir in cwd.ancestors() {
        let claude = dir.join(".claude");
        out.extend([dir.join("CLAUDE.md"), dir.join("CLAUDE.local.md"), dir.join(".mcp.json")]);
        for sub in ["CLAUDE.md", "rules", "skills", "commands", "agents", "settings.json", "settings.local.json"] {
            out.push(claude.join(sub));
        }
        out.push(claude);
    }
    out
}

/// Where a plugin folder's skills and commands appear.
fn plugin_parts(dirs: &[PathBuf]) -> impl Iterator<Item = PathBuf> + '_ {
    dirs.iter().flat_map(|d| [d.clone(), d.join(".claude-plugin/plugin.json"), d.join("skills"), d.join("commands")])
}

fn scope(scope: MemoryScope) -> InstructionScope {
    match scope {
        MemoryScope::Managed => InstructionScope::Managed,
        MemoryScope::User => InstructionScope::User,
        MemoryScope::Project => InstructionScope::Project,
        MemoryScope::Local => InstructionScope::Local,
    }
}
