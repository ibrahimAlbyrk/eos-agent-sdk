//! Every skill and prompt command of a session, by name.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::discover::{self, SkillRoots};
use crate::expand::{self, ExpandContext, ExpandError, Expansion};
use crate::listing::{self, ListingBudget};
use crate::scoped::{self, Discovered, Found};
use crate::skill::{Origin, Skill};

/// Shared by every agent of a session (one `Arc`); [`SkillCatalog::reload`]
/// swaps the contents in place, so the Skill tool and listing reminders see
/// the change.
#[derive(Debug, Default)]
pub struct SkillCatalog {
    entries: RwLock<Arc<Entries>>,
    /// What discovery found, in precedence order.
    base: RwLock<Vec<Skill>>,
    /// Skills found as the agents touch files (see [`crate::scoped`]).
    found: Mutex<Found>,
}

#[derive(Debug, Default)]
struct Entries {
    skills: Vec<Arc<Skill>>,
    index: HashMap<String, usize>,
}

impl Entries {
    /// The first skill of a name wins; discovery orders sources by precedence.
    fn new(skills: Vec<Skill>) -> Self {
        let mut index = HashMap::with_capacity(skills.len());
        let mut kept = Vec::with_capacity(skills.len());
        for skill in skills {
            if !index.contains_key(&skill.name) {
                index.insert(skill.name.clone(), kept.len());
                kept.push(Arc::new(skill));
            }
        }
        Self { skills: kept, index }
    }
}

/// Why the model's request to load a skill was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvokeError {
    #[error("Unknown skill: {name}{}", suggestion.as_ref().map(|s| format!(". Did you mean {s}?")).unwrap_or_default())]
    Unknown { name: String, suggestion: Option<String> },
    #[error(
        "Skill {0} cannot be used with Skill tool due to disable-model-invocation. Ask the user to run /{0} themselves \
\u{2014} it cannot be invoked via the Skill tool. Do not replicate this skill's workflow by other means \u{2014} it is \
reserved for explicit user invocation."
    )]
    UserOnly(String),
    #[error(transparent)]
    Expand(#[from] ExpandError),
}

impl SkillCatalog {
    /// The skills of `roots`; the project root (`roots.cwd`) is where
    /// directory-scoped skills are looked for below.
    pub fn discover(roots: &SkillRoots) -> Self {
        let catalog = Self::from_skills(discover::discover(roots));
        *catalog.found.lock().unwrap_or_else(PoisonError::into_inner) = Found::new(roots.cwd.clone());
        catalog
    }

    pub fn from_skills(skills: Vec<Skill>) -> Self {
        let catalog = Self { base: RwLock::new(skills), ..Self::default() };
        catalog.rebuild();
        catalog
    }

    /// Discovers again (new or edited skill files); skills found below the
    /// project root stay.
    pub fn reload(&self, roots: &SkillRoots) {
        self.replace(discover::discover(roots));
    }

    pub fn replace(&self, skills: Vec<Skill>) {
        *self.base.write().unwrap_or_else(PoisonError::into_inner) = skills;
        self.rebuild();
    }

    fn rebuild(&self) {
        let base = self.base.read().unwrap_or_else(PoisonError::into_inner);
        let found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
        let mut all: Vec<Skill> = base
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if found.activated.contains(&s.name) {
                    // Its paths matched a touched file: listed from now on.
                    s.paths.clear();
                }
                s
            })
            .collect();
        all.extend(scoped::named(&base, &found.scoped).into_iter().map(|(_, s)| s));
        drop((base, found));
        *self.entries.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(Entries::new(all));
    }

    /// Files an agent read or wrote: finds the `.claude/skills` of the
    /// folders above them (below the project root) and activates skills
    /// whose `paths` match. Returns the folders with new skills, each with
    /// the names they are listed under.
    pub fn touched(&self, files: &[PathBuf]) -> Vec<Discovered> {
        let conditional: Vec<Skill> = self
            .base
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|s| !s.paths.is_empty())
            .cloned()
            .collect();
        let refs: Vec<&Skill> = conditional.iter().collect();
        let mut found_dirs = Vec::new();
        let mut changed = false;
        {
            let mut found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
            for file in files {
                let (dirs, c) = found.touch(file, &refs);
                found_dirs.extend(dirs);
                changed |= c;
            }
        }
        if !changed {
            return Vec::new();
        }
        self.rebuild();
        let names: HashMap<usize, String> = {
            let base = self.base.read().unwrap_or_else(PoisonError::into_inner);
            let found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
            scoped::named(&base, &found.scoped)
                .into_iter()
                .filter(|(_, s)| s.is_listed())
                .map(|(i, s)| (i, s.name))
                .collect()
        };
        found_dirs
            .into_iter()
            .map(|(dir, added)| Discovered { dir, names: added.iter().filter_map(|i| names.get(i).cloned()).collect() })
            .filter(|d| !d.names.is_empty())
            .collect()
    }

    fn snapshot(&self) -> Arc<Entries> {
        Arc::clone(&self.entries.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn skills(&self) -> Vec<Arc<Skill>> {
        self.snapshot().skills.clone()
    }

    /// The skills the model may see and call, in precedence order.
    pub fn listed(&self) -> Vec<Arc<Skill>> {
        self.snapshot().skills.iter().filter(|s| s.is_listed()).cloned().collect()
    }

    /// By exact name, or a plugin item by its bare name when only one plugin has it.
    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        let entries = self.snapshot();
        if let Some(&i) = entries.index.get(name) {
            return Some(Arc::clone(&entries.skills[i]));
        }
        if name.contains(':') {
            return None;
        }
        let suffix = format!(":{name}");
        let mut plugin_items = entries.skills.iter().filter(|s| {
            matches!(s.origin, Origin::PluginSkill { .. } | Origin::PluginCommand { .. }) && s.name.ends_with(&suffix)
        });
        match (plugin_items.next(), plugin_items.next()) {
            (Some(only), None) => Some(Arc::clone(only)),
            _ => None,
        }
    }

    /// The listing text for the session context (`None` with nothing to list).
    pub fn listing(&self, budget: &ListingBudget) -> Option<String> {
        let listed = self.listed();
        (!listed.is_empty()).then(|| listing::message(&listing::entries(&listed, budget)))
    }

    /// Expands `/name args` typed by the user. `None` when the input is not a
    /// slash command of a user-invocable skill (the host then treats it as
    /// plain text or its own command).
    pub async fn expand(&self, input: &str, ctx: &ExpandContext<'_>) -> Option<Result<Expansion, ExpandError>> {
        let (name, args) = parse_slash(input)?;
        let skill = self.get(name).filter(|s| s.user_invocable)?;
        Some(expand::render(&skill, args, ctx).await)
    }

    /// Loads a skill the model asked for (the Skill tool's path).
    pub async fn invoke(&self, name: &str, args: &str, ctx: &ExpandContext<'_>) -> Result<Expansion, InvokeError> {
        let Some(skill) = self.get(name) else {
            return Err(InvokeError::Unknown { name: name.to_owned(), suggestion: self.suggest(name) });
        };
        if skill.disable_model_invocation {
            return Err(InvokeError::UserOnly(name.to_owned()));
        }
        Ok(expand::render(&skill, args, ctx).await?)
    }

    /// A listed name within two edits of `name`.
    fn suggest(&self, name: &str) -> Option<String> {
        self.listed()
            .iter()
            .map(|s| (edit_distance(&s.name, name), &s.name))
            .filter(|(d, _)| *d <= 2)
            .min_by_key(|(d, _)| *d)
            .map(|(_, n)| n.clone())
    }
}

/// `/name args` → `(name, args)`.
pub fn parse_slash(input: &str) -> Option<(&str, &str)> {
    let rest = input.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then(|| (name, rest[end..].trim()))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + usize::from(ca != cb));
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slash_commands() {
        assert_eq!(parse_slash("/deploy prod eu "), Some(("deploy", "prod eu")));
        assert_eq!(parse_slash("  /design"), Some(("design", "")));
        assert_eq!(parse_slash("deploy"), None);
        assert_eq!(parse_slash("/ x"), None);
        assert_eq!(edit_distance("deploy", "deplyo"), 2);
    }
}
