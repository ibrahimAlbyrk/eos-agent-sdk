//! The skill listing the model reads, under Claude Code's budgets, and the
//! reminder that announces skills added later.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use eos_agent_core::{BoxFuture, Extensions, FileStates, ReminderSource};

use crate::catalog::SkillCatalog;
use crate::skill::{Origin, Skill};

pub const LISTING_HEADER: &str = "The following skills are available for use with the Skill tool:";

/// How much of the context the listing may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingBudget {
    pub total_chars: usize,
    /// Longer descriptions are cut with `…`.
    pub max_description_chars: usize,
}

impl Default for ListingBudget {
    /// 1% of a 200k-token window at 4 chars per token; 1536 per description.
    fn default() -> Self {
        Self::for_context_window(200_000)
    }
}

impl ListingBudget {
    pub fn for_context_window(tokens: u64) -> Self {
        let total = (tokens.saturating_mul(4) / 100).max(1);
        Self { total_chars: usize::try_from(total).unwrap_or(usize::MAX), max_description_chars: 1536 }
    }
}

/// `header\n\nentries`, the text of the listing message.
pub(crate) fn message(entries: &str) -> String {
    format!("{LISTING_HEADER}\n\n{entries}")
}

/// One line per skill. Over budget, bundled skills keep their descriptions
/// and the rest keep theirs in order while room lasts; the others are listed
/// by name only.
pub(crate) fn entries(skills: &[Arc<Skill>], budget: &ListingBudget) -> String {
    let lines: Vec<(String, String)> = skills
        .iter()
        .map(|s| {
            let name_only = format!("- {}", s.name);
            let full = format!("{name_only}: {}", truncate(&s.listing_description(), budget.max_description_chars));
            (name_only, full)
        })
        .collect();
    let len = |s: &String| s.chars().count();
    let separators = lines.len().saturating_sub(1);
    let full_total: usize = lines.iter().map(|(_, f)| len(f)).sum::<usize>() + separators;
    if full_total <= budget.total_chars {
        return lines.into_iter().map(|(_, f)| f).collect::<Vec<_>>().join("\n");
    }
    let fixed: Vec<bool> = skills.iter().map(|s| s.origin == Origin::Bundled).collect();
    let floor: usize =
        lines.iter().zip(&fixed).map(|((n, f), &fixed)| if fixed { len(f) } else { len(n) }).sum::<usize>()
            + separators;
    let mut room = budget.total_chars as i64 - floor as i64;
    let mut keep = fixed.clone();
    for (i, (name_only, full)) in lines.iter().enumerate() {
        if fixed[i] {
            continue;
        }
        let extra = (len(full) - len(name_only)) as i64;
        if extra <= room {
            keep[i] = true;
            room -= extra;
        }
    }
    lines.into_iter().zip(keep).map(|((n, f), keep)| if keep { f } else { n }).collect::<Vec<_>>().join("\n")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// Announces skills the agent has not been told about yet, before each
/// request. The first listing usually goes out with the session context
/// ([`SkillCatalog::listing`]); then only additions follow.
#[derive(Debug)]
pub struct SkillListingReminder {
    catalog: Arc<SkillCatalog>,
    budget: ListingBudget,
    initial_in_context: bool,
}

/// Digests of the entries an agent was shown.
#[derive(Debug, Default)]
struct Shown(Mutex<Option<HashSet<u64>>>);

/// The files of the agent's file record already looked at for skills.
#[derive(Debug, Default)]
struct Looked(Mutex<HashSet<PathBuf>>);

impl SkillListingReminder {
    /// `initial_in_context`: the full listing was already sent with the
    /// session context, so the first call only records what it held.
    pub fn new(catalog: Arc<SkillCatalog>, budget: ListingBudget, initial_in_context: bool) -> Self {
        Self { catalog, budget, initial_in_context }
    }

    /// Skills found from the files the agent read or wrote since the last
    /// request: one notice per folder whose own skills were found.
    fn discovered(&self, extensions: &Extensions) -> Vec<String> {
        let Some(files) = extensions.get::<FileStates>() else { return Vec::new() };
        let current: HashSet<PathBuf> = files.snapshot().into_iter().map(|(p, _)| p).collect();
        let looked = extensions.get_or_default::<Looked>();
        let mut looked = looked.0.lock().unwrap_or_else(PoisonError::into_inner);
        let new: Vec<PathBuf> = current.iter().filter(|p| !looked.contains(*p)).cloned().collect();
        // Only current entries are kept, so the set never outgrows the record.
        *looked = current;
        drop(looked);
        if new.is_empty() {
            return Vec::new();
        }
        self.catalog
            .touched(&new)
            .into_iter()
            .map(|d| {
                let names: Vec<String> = d.names.iter().map(|n| format!("- {n}")).collect();
                format!(
                    "<system-reminder>\nNew skills discovered in {}, now available via the Skill tool:\n{}\n</system-reminder>",
                    d.dir,
                    names.join("\n")
                )
            })
            .collect()
    }

    fn pending(&self, extensions: &Extensions) -> Option<String> {
        let listed = self.catalog.listed();
        let shown = extensions.get_or_default::<Shown>();
        let mut shown = shown.0.lock().unwrap_or_else(PoisonError::into_inner);
        let current: HashSet<u64> = listed.iter().map(|s| s.digest).collect();
        let new: Vec<Arc<Skill>> = match shown.as_ref() {
            None if self.initial_in_context => Vec::new(),
            None => listed,
            Some(seen) => listed.into_iter().filter(|s| !seen.contains(&s.digest)).collect(),
        };
        // Only current entries are kept, so the set never outgrows the catalog.
        *shown = Some(current);
        (!new.is_empty())
            .then(|| format!("<system-reminder>\n{}\n</system-reminder>", message(&entries(&new, &self.budget))))
    }
}

impl ReminderSource for SkillListingReminder {
    fn collect<'a>(&'a self, extensions: &'a Extensions) -> BoxFuture<'a, Vec<String>> {
        let mut notes = self.discovered(extensions);
        notes.extend(self.pending(extensions));
        Box::pin(async move { notes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, desc: &str, origin: Origin) -> Arc<Skill> {
        let mut s = Skill::from_markdown(name, &format!("---\ndescription: {desc}\n---\n"), origin, None);
        s.digest = s.compute_digest();
        Arc::new(s)
    }

    #[test]
    fn fits_or_collapses_by_order() {
        let skills = vec![
            skill("a", &"x".repeat(30), Origin::UserSkill),
            skill("b", &"y".repeat(30), Origin::UserSkill),
            skill("c", &"z".repeat(30), Origin::Bundled),
        ];
        let roomy = ListingBudget { total_chars: 1000, max_description_chars: 1536 };
        assert_eq!(entries(&skills, &roomy).lines().count(), 3);
        assert!(entries(&skills, &roomy).contains("- b: yyy"));
        // Room for the bundled one plus one more description.
        let tight = ListingBudget { total_chars: 85, max_description_chars: 1536 };
        assert_eq!(entries(&skills, &tight), format!("- a: {}\n- b\n- c: {}", "x".repeat(30), "z".repeat(30)));
    }

    #[test]
    fn long_descriptions_are_cut() {
        let s = vec![skill("a", &"d".repeat(50), Origin::UserSkill)];
        let budget = ListingBudget { total_chars: 8000, max_description_chars: 10 };
        assert_eq!(entries(&s, &budget), format!("- a: {}\u{2026}", "d".repeat(9)));
        assert_eq!(ListingBudget::default().total_chars, 8000);
    }
}
