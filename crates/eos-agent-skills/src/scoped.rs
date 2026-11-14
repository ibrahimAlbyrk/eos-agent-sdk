//! Skills found as the agents work: directory-scoped skills and skills
//! that apply to certain paths.
//!
//! - **Directory-scoped skills** live in a `.claude/skills/` folder of a
//!   subdirectory of the project (`apps/web/.claude/skills/deploy/SKILL.md`).
//!   Nothing walks the tree for them: when an agent has read or written a
//!   file, each folder from that file's directory up to (not including) the
//!   project root is checked once for `.claude/skills/` (a symbolic link to a
//!   folder counts). A folder git ignores is skipped. What a folder holds
//!   joins the listing from then on, and the agent is told of it.
//! - **Naming**: a scoped skill keeps its own name while no other skill
//!   has it, its description noting where it applies. When the name is taken
//!   (an unscoped skill, or a scoped one in another folder), it is listed with
//!   its folder as a prefix, `apps/web:deploy`, and the Skill tool takes that
//!   name; a prefixed name that is taken too is not offered.
//! - **Path-conditional skills** (frontmatter `paths`) stay out of the
//!   listing until an agent touches a file under the project whose relative
//!   path matches one of their patterns (gitignore-style).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ignore::gitignore::GitignoreBuilder;

use crate::discover;
use crate::skill::{Origin, Skill};

/// Folders checked for `.claude/skills` per catalog, at most.
const MAX_CHECKED_DIRS: usize = 8192;
/// Scoped skills kept per catalog, at most.
const MAX_SCOPED: usize = 512;

/// One scoped skill as found, before naming.
#[derive(Debug, Clone)]
pub(crate) struct Scoped {
    /// The folder holding `.claude/skills`, relative to the project root,
    /// with `/` separators (`apps/web`).
    pub(crate) scope: String,
    pub(crate) skill: Skill,
}

/// A folder whose skills were just found, for the agent's notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// `apps/web/.claude/skills`, relative to the project root.
    pub dir: String,
    /// The names the skills are listed under.
    pub names: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Found {
    pub(crate) root: Option<PathBuf>,
    checked: HashSet<PathBuf>,
    checked_order: VecDeque<PathBuf>,
    pub(crate) scoped: Vec<Scoped>,
    /// Path-conditional skills an agent's file activated (by name).
    pub(crate) activated: HashSet<String>,
}

impl Found {
    pub(crate) fn new(root: Option<PathBuf>) -> Self {
        Self { root, ..Self::default() }
    }

    /// Checks the folders above `file` and the path patterns of
    /// `conditional`. Returns the folders with new skills and whether
    /// anything changed.
    /// `found`: each folder with new skills and their indices in `scoped`.
    pub(crate) fn touch(&mut self, file: &Path, conditional: &[&Skill]) -> (Vec<(String, Vec<usize>)>, bool) {
        let Some(root) = self.root.clone() else { return (Vec::new(), false) };
        let Some(rel) = relative(&root, file) else { return (Vec::new(), false) };
        let mut changed = false;
        for skill in conditional {
            if !self.activated.contains(&skill.name) && matches_paths(&root, &skill.paths, &rel) {
                self.activated.insert(skill.name.clone());
                changed = true;
            }
        }
        let mut found = Vec::new();
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == root || !d.starts_with(&root) {
                break;
            }
            let skills_dir = d.join(".claude").join("skills");
            if self.first_check(&skills_dir) && skills_dir.is_dir() && !git_ignores(&root, d) {
                let scope = relative(&root, d).unwrap_or_default();
                let mut skills = Vec::new();
                discover::skill_dir(&skills_dir, &Origin::ProjectSkill, None, &mut HashSet::new(), &mut skills);
                let mut added = Vec::new();
                for skill in skills {
                    if self.scoped.len() < MAX_SCOPED {
                        added.push(self.scoped.len());
                        self.scoped.push(Scoped { scope: scope.clone(), skill });
                        changed = true;
                    }
                }
                if !added.is_empty() {
                    found.push((format!("{scope}/.claude/skills"), added));
                }
            }
            dir = d.parent();
        }
        (found, changed)
    }

    fn first_check(&mut self, dir: &Path) -> bool {
        if !self.checked.insert(dir.to_path_buf()) {
            return false;
        }
        self.checked_order.push_back(dir.to_path_buf());
        while self.checked_order.len() > MAX_CHECKED_DIRS {
            if let Some(old) = self.checked_order.pop_front() {
                self.checked.remove(&old);
            }
        }
        true
    }
}

/// `path` relative to `root` with `/` separators, when it lies under it.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.iter().map(|c| c.to_string_lossy().into_owned()).collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Whether `rel` (a file under `root`) matches one of `patterns`, read as
/// gitignore lines.
fn matches_paths(root: &Path, patterns: &[String], rel: &str) -> bool {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        let _ = builder.add_line(None, pattern);
    }
    let Ok(matcher) = builder.build() else { return false };
    matcher.matched_path_or_any_parents(rel, false).is_ignore()
}

/// `git check-ignore` on `dir` (not a repository, or no git: not ignored).
fn git_ignores(root: &Path, dir: &Path) -> bool {
    Command::new("git")
        .args(["check-ignore", "-q", "--"])
        .arg(dir)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The scoped skills under the names they are listed by, after `base`,
/// each with its index in `scoped`.
pub(crate) fn named(base: &[Skill], scoped: &[Scoped]) -> Vec<(usize, Skill)> {
    let taken: HashSet<&str> = base.iter().map(|s| s.name.as_str()).collect();
    let mut per_name: HashMap<&str, usize> = HashMap::new();
    for s in scoped {
        *per_name.entry(s.skill.name.as_str()).or_default() += 1;
    }
    let mut out: Vec<(usize, Skill)> = Vec::new();
    let mut used: HashSet<String> = HashSet::new();
    for (i, Scoped { scope, skill }) in scoped.iter().enumerate() {
        let bare = skill.name.as_str();
        let clash = taken.contains(bare) || per_name.get(bare).copied().unwrap_or(0) > 1;
        let mut s = skill.clone();
        let from = format!("from {scope}/.claude/skills \u{2014} applies when working on files under {scope}/");
        if !clash {
            s.description = format!("{} ({from})", s.description);
            used.insert(s.name.clone());
        } else {
            let prefixed = format!("{scope}:{bare}");
            if taken.contains(prefixed.as_str()) || used.contains(&prefixed) {
                continue;
            }
            let note = if taken.contains(bare) || used.contains(bare) {
                format!(
                    "scoped to {scope}/ \u{2014} use this instead of the unscoped \"{bare}\" skill when the files being changed are under {scope}/"
                )
            } else {
                from
            };
            s.description = format!("{} ({note})", s.description);
            s.name = prefixed.clone();
            used.insert(prefixed);
        }
        s.digest = s.compute_digest();
        out.push((i, s));
    }
    out
}
