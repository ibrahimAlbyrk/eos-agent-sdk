//! Plugin skills and commands: a plugin root is a folder with
//! `.claude-plugin/plugin.json`, `skills/<name>/SKILL.md` and `commands/*.md`;
//! its items are named `plugin:item`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::discover::{SkillRoots, command_dir, first_visit, qualified, skill_dir, skill_file};
use crate::skill::{Origin, Skill};

const MAX_MANIFEST: u64 = 1_000_000;

fn read_json(path: &Path) -> Option<Value> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Explicit plugin dirs, then the enabled installed plugins.
pub(crate) fn plugin_roots(roots: &SkillRoots, projects: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = roots.plugin_dirs.clone();
    if roots.installed_plugins
        && let Some(user) = &roots.user_dir
    {
        out.extend(installed(user, projects));
    }
    out
}

/// `plugins/installed_plugins.json` entries switched on in `enabledPlugins`;
/// project- and local-scoped installs only for their own project.
fn installed(user: &Path, projects: &[PathBuf]) -> Vec<PathBuf> {
    let Some(list) = read_json(&user.join("plugins").join("installed_plugins.json")) else { return Vec::new() };
    let Some(plugins) = list.get("plugins").and_then(Value::as_object) else { return Vec::new() };
    let enabled = enabled_plugins(user, projects);
    let project_roots: Vec<&Path> = projects.iter().filter_map(|p| p.parent()).collect();
    let mut out = Vec::new();
    for (key, installs) in plugins {
        if enabled.get(key) != Some(&true) {
            continue;
        }
        let installs: Vec<&Value> = match installs {
            Value::Array(list) => list.iter().collect(),
            single @ Value::Object(_) => vec![single],
            _ => continue,
        };
        let applies = |install: &&Value| match install.get("scope").and_then(Value::as_str) {
            Some("project" | "local") => install
                .get("projectPath")
                .and_then(Value::as_str)
                .is_some_and(|p| project_roots.iter().any(|root| *root == Path::new(p))),
            _ => true,
        };
        let path = installs.into_iter().find(applies).and_then(|i| i.get("installPath")).and_then(Value::as_str);
        if let Some(path) = path.map(PathBuf::from).filter(|p| p.is_dir()) {
            out.push(path);
        }
    }
    out
}

/// `enabledPlugins` merged from user, then project (farthest first), then local settings.
fn enabled_plugins(user: &Path, projects: &[PathBuf]) -> HashMap<String, bool> {
    let mut files = vec![user.join("settings.json")];
    for project in projects.iter().rev() {
        files.push(project.join("settings.json"));
        files.push(project.join("settings.local.json"));
    }
    let mut enabled = HashMap::new();
    for file in files {
        if let Some(Value::Object(map)) = read_json(&file).and_then(|v| v.get("enabledPlugins").cloned()) {
            for (key, value) in map {
                if let Some(on) = value.as_bool() {
                    enabled.insert(key, on);
                }
            }
        }
    }
    enabled
}

/// One path or a list of paths from the manifest.
fn manifest_paths(value: Option<&Value>) -> Option<Vec<String>> {
    match value? {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(list) => Some(list.iter().filter_map(Value::as_str).map(str::to_owned).collect()),
        _ => Some(Vec::new()),
    }
}

/// `root.join(rel)`, refused when it leads outside the plugin.
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let path = root.join(rel);
    let real = fs::canonicalize(&path).ok()?;
    let root = fs::canonicalize(root).ok()?;
    real.starts_with(&root).then_some(path)
}

/// Loads one plugin's skills (`skills/` plus manifest `skills` paths) and
/// commands (manifest `commands` paths, which replace `commands/`).
pub(crate) fn load(root: &Path, seen: &mut HashSet<PathBuf>, out: &mut Vec<Skill>) {
    let manifest = read_json(&root.join(".claude-plugin").join("plugin.json"));
    let dir_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = manifest
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .map_or(dir_name, str::to_owned);
    let skill_origin = Origin::PluginSkill { plugin: name.clone() };
    let command_origin = Origin::PluginCommand { plugin: name.clone() };
    let first = out.len();

    skill_dir(&root.join("skills"), &skill_origin, Some(&name), seen, out);
    for rel in manifest_paths(manifest.as_ref().and_then(|m| m.get("skills"))).unwrap_or_default() {
        let Some(path) = inside(root, &rel) else { continue };
        match skill_file(&path) {
            Some(file) if first_visit(&file, seen) => {
                let item = qualified(Some(&name), &path.file_name().unwrap_or_default().to_string_lossy());
                out.extend(Skill::from_file(&item, &file, skill_origin.clone(), Some(path)));
            }
            Some(_) => {}
            None => skill_dir(&path, &skill_origin, Some(&name), seen, out),
        }
    }

    match manifest_paths(manifest.as_ref().and_then(|m| m.get("commands"))) {
        None => command_dir(&root.join("commands"), &command_origin, Some(&name), seen, out),
        Some(paths) => {
            for rel in paths {
                let Some(path) = inside(root, &rel) else { continue };
                if path.is_dir() {
                    command_dir(&path, &command_origin, Some(&name), seen, out);
                } else if path.extension().is_some_and(|e| e == "md") && first_visit(&path, seen) {
                    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    out.extend(Skill::from_file(&qualified(Some(&name), &stem), &path, command_origin.clone(), None));
                }
            }
        }
    }
    for skill in &mut out[first..] {
        skill.plugin_root = Some(root.to_path_buf());
    }
}
