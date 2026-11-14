//! Saved workflows, run by name: `*.js` files with a valid `meta` in the
//! user's `~/.claude/workflows/` and every `.claude/workflows/` from the
//! working directory up (the closest wins, then the user's).

use std::path::{Path, PathBuf};

use crate::meta::{self, MAX_SCRIPT};
use crate::store::normalize;

#[derive(Debug, Clone)]
pub(crate) struct Saved {
    pub(crate) name: String,
    pub(crate) script: String,
}

/// Every saved workflow visible from `cwd`, by name, sorted.
pub(crate) fn all(cwd: Option<&Path>, user_dir: Option<&Path>) -> Vec<Saved> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(cwd) = cwd {
        dirs.extend(cwd.ancestors().map(|d| d.join(".claude").join("workflows")));
    }
    if let Some(user) = user_dir {
        dirs.push(user.join("workflows"));
    }
    let mut found: Vec<Saved> = Vec::new();
    for dir in dirs {
        for saved in read_dir(&dir) {
            if !found.iter().any(|f| f.name == saved.name) {
                found.push(saved);
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

pub(crate) fn find(name: &str, cwd: Option<&Path>, user_dir: Option<&Path>) -> Option<Saved> {
    all(cwd, user_dir).into_iter().find(|s| s.name == name)
}

/// Claude Code's error for an unknown name.
pub(crate) fn not_found(name: &str, cwd: Option<&Path>, user_dir: Option<&Path>) -> String {
    let names: Vec<String> = all(cwd, user_dir).into_iter().map(|s| s.name).collect();
    let list = if names.is_empty() { "(none)".to_owned() } else { names.join(", ") };
    format!("Workflow \"{name}\" not found. Available: {list}")
}

fn read_dir(dir: &Path) -> Vec<Saved> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let Ok(meta_data) = std::fs::metadata(&path) else { continue };
        if !meta_data.is_file() || meta_data.len() > MAX_SCRIPT as u64 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let script = normalize(&text);
        let Ok(parsed) = meta::parse(&script) else { continue };
        out.push(Saved { name: parsed.meta.name, script });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_wins_and_invalid_files_are_skipped() {
        let root = std::env::temp_dir().join(format!("eos-wf-named-{}", uuid::Uuid::new_v4()));
        let project = root.join("repo");
        let nested = project.join("pkg");
        let user = root.join("home");
        for d in [project.join(".claude/workflows"), nested.join(".claude/workflows"), user.join("workflows")] {
            std::fs::create_dir_all(d).expect("dir");
        }
        let script =
            |name: &str, d: &str| format!("export const meta = {{name: '{name}', description: '{d}'}}\nreturn 1");
        std::fs::write(project.join(".claude/workflows/a.js"), script("audit", "far")).expect("w");
        std::fs::write(nested.join(".claude/workflows/a.js"), script("audit", "near")).expect("w");
        std::fs::write(user.join("workflows/b.js"), script("bisect", "user")).expect("w");
        std::fs::write(user.join("workflows/bad.js"), "no meta").expect("w");
        std::fs::write(user.join("workflows/c.ts"), script("ts", "x")).expect("w");
        let found = all(Some(&nested), Some(&user));
        let names: Vec<(&str, bool)> = found.iter().map(|s| (s.name.as_str(), s.script.contains("near"))).collect();
        assert_eq!(names, vec![("audit", true), ("bisect", false)]);
        assert_eq!(not_found("x", Some(&nested), Some(&user)), "Workflow \"x\" not found. Available: audit, bisect");
        std::fs::remove_dir_all(root).ok();
    }
}
