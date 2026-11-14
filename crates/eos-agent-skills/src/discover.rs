//! Finding skill and command files where Claude Code looks for them.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::bundled::{self, SkillRenderer};
use crate::plugins;
use crate::skill::{Origin, Skill};

const MAX_DEPTH: usize = 8;

/// Where to look. Tests point these at temp dirs.
#[derive(Debug, Clone, Default)]
pub struct SkillRoots {
    /// The user config dir (`~/.claude`): `skills/`, `commands/`,
    /// `plugins/installed_plugins.json` and `settings.json`.
    pub user_dir: Option<PathBuf>,
    /// The session's working directory. Each `.claude/` from here up to the
    /// git root (or just below `home`) adds `skills/` and `commands/`.
    pub cwd: Option<PathBuf>,
    /// The walk up from `cwd` stops before reaching this directory.
    pub home: Option<PathBuf>,
    /// Plugin roots loaded as they are (like `--plugin-dir`).
    pub plugin_dirs: Vec<PathBuf>,
    /// Load the enabled plugins listed in `installed_plugins.json`.
    pub installed_plugins: bool,
    /// Include the bundled `/design` and `/slides`.
    pub bundled: bool,
    /// Include the Artifact skills (`artifact-design`, `artifact-diagramming`,
    /// `artifact-capabilities`, the last composed by this), as Claude Code
    /// does when the Artifact tools are on.
    pub artifacts: Option<Arc<dyn SkillRenderer>>,
    /// More bundled skills a host ships (Claude Code's `workflow-authoring`
    /// when workflows are on), after the others.
    pub extra: Vec<BundledSkill>,
}

/// A bundled skill whose text the host composes.
#[derive(Debug, Clone)]
pub struct BundledSkill {
    pub name: String,
    pub description: String,
    pub user_invocable: bool,
    pub renderer: Arc<dyn SkillRenderer>,
}

impl SkillRoots {
    /// Claude Code's locations for a session working in `cwd`.
    pub fn standard(cwd: impl Into<PathBuf>) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let user_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".claude")));
        Self {
            user_dir,
            cwd: Some(cwd.into()),
            home,
            plugin_dirs: Vec::new(),
            installed_plugins: true,
            bundled: true,
            artifacts: None,
            extra: Vec::new(),
        }
    }

    /// The project's `.claude` dirs, nearest first.
    pub(crate) fn project_dirs(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Some(mut dir) = self.cwd.clone() else { return out };
        loop {
            if self.home.as_deref() == Some(dir.as_path()) {
                break;
            }
            let claude = dir.join(".claude");
            if claude.is_dir() && Some(&claude) != self.user_dir.as_ref() {
                out.push(claude);
            }
            if dir.join(".git").exists() || !dir.pop() {
                break;
            }
        }
        out
    }
}

/// All skills in precedence order: user skills, project skills (nearest
/// first), legacy commands (user, then project), plugins, bundled. The same
/// file reached twice (symlinks) loads once.
pub(crate) fn discover(roots: &SkillRoots) -> Vec<Skill> {
    let projects = roots.project_dirs();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    if let Some(user) = &roots.user_dir {
        skill_dir(&user.join("skills"), &Origin::UserSkill, None, &mut seen, &mut out);
    }
    for project in &projects {
        skill_dir(&project.join("skills"), &Origin::ProjectSkill, None, &mut seen, &mut out);
    }
    if let Some(user) = &roots.user_dir {
        command_dir(&user.join("commands"), &Origin::UserCommand, None, &mut seen, &mut out);
    }
    for project in &projects {
        command_dir(&project.join("commands"), &Origin::ProjectCommand, None, &mut seen, &mut out);
    }
    for root in plugins::plugin_roots(roots, &projects) {
        plugins::load(&root, &mut seen, &mut out);
    }
    if roots.bundled {
        out.extend(bundled::all());
    }
    if let Some(capabilities) = &roots.artifacts {
        out.extend(bundled::artifacts(Arc::clone(capabilities)));
    }
    out.extend(roots.extra.iter().map(bundled::rendered));
    out
}

/// A file not loaded before (by canonical path).
pub(crate) fn first_visit(path: &Path, seen: &mut HashSet<PathBuf>) -> bool {
    seen.insert(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = fs::read_dir(dir) else { return Vec::new() };
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    entries
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The `SKILL.md` of a folder (any letter case).
pub(crate) fn skill_file(dir: &Path) -> Option<PathBuf> {
    sorted_entries(dir).into_iter().find(|p| file_name(p).eq_ignore_ascii_case("SKILL.md") && p.is_file())
}

/// `<base>/<name>/SKILL.md`, one level deep. Folders that are plugins are skipped.
pub(crate) fn skill_dir(
    base: &Path,
    origin: &Origin,
    prefix: Option<&str>,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<Skill>,
) {
    for dir in sorted_entries(base) {
        if !dir.is_dir() || dir.join(".claude-plugin").exists() {
            continue;
        }
        let Some(file) = skill_file(&dir) else { continue };
        if !first_visit(&file, seen) {
            continue;
        }
        let name = qualified(prefix, &file_name(&dir));
        out.extend(Skill::from_file(&name, &file, origin.clone(), Some(dir)));
    }
}

/// Legacy commands: every `*.md` under `base`, named by its path
/// (`frontend/component.md` → `frontend:component`). A folder holding a
/// `SKILL.md` is one skill named by the folder, with that folder as its base.
pub(crate) fn command_dir(
    base: &Path,
    origin: &Origin,
    prefix: Option<&str>,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<Skill>,
) {
    walk_commands(base, base, 0, origin, prefix, seen, out);
}

fn walk_commands(
    base: &Path,
    dir: &Path,
    depth: usize,
    origin: &Origin,
    prefix: Option<&str>,
    seen: &mut HashSet<PathBuf>,
    out: &mut Vec<Skill>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let namespace = |path: &Path| -> Vec<String> {
        path.strip_prefix(base)
            .map(|rel| rel.iter().map(|c| c.to_string_lossy().into_owned()).collect())
            .unwrap_or_default()
    };
    let entries = sorted_entries(dir);
    if depth > 0
        && let Some(file) = skill_file(dir)
    {
        if first_visit(&file, seen) {
            let name = qualified(prefix, &namespace(dir).join(":"));
            out.extend(Skill::from_file(&name, &file, origin.clone(), Some(dir.to_path_buf())));
        }
    } else {
        for file in entries.iter().filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md")) {
            if !first_visit(file, seen) {
                continue;
            }
            let mut parts = namespace(dir);
            parts.push(file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
            out.extend(Skill::from_file(&qualified(prefix, &parts.join(":")), file, origin.clone(), None));
        }
    }
    for sub in entries.iter().filter(|p| p.is_dir()) {
        walk_commands(base, sub, depth + 1, origin, prefix, seen, out);
    }
}

pub(crate) fn qualified(prefix: Option<&str>, name: &str) -> String {
    match prefix {
        Some(plugin) => format!("{plugin}:{name}"),
        None => name.to_owned(),
    }
}
