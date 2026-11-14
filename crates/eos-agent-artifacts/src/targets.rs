//! Which artifact a session works on, and the contract each one is pinned
//! to: Claude Code's `artifactRefs` (its `eJo` sets, `tJo` reads). The
//! newest artifact the session published, created or opened for writing is
//! the target; the `artifact-capabilities` skill is written for the target's
//! contract when it has a pinned one.
//!
//! One table per process, by session (a top-level agent and its subagents
//! share one, as Claude Code's subagents share the session's).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use eos_agent_core::Extensions;

/// The session an agent belongs to (its tree's top-level agent). Hosts put
/// it in every agent's extensions; without it an agent is its own session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSession(pub String);

/// The key of the session `extensions` belong to (`agent` without one).
pub(crate) fn session_of(extensions: &Extensions, agent: &str) -> String {
    extensions.get::<ArtifactSession>().map_or_else(|| agent.to_owned(), |s| s.0.clone())
}

const SESSIONS: usize = 256;
const REFS_PER_SESSION: usize = 64;
const NO_PIN_SLUGS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Ref {
    slug: String,
    pin: Option<String>,
}

/// Claude Code's `getArtifactContractTarget()`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) slug: Option<String>,
    pub(crate) pins: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub(crate) struct ContractTargets {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    sessions: HashMap<String, Vec<Ref>>,
    order: VecDeque<String>,
    /// Artifacts whose read-back named no pin (Claude Code asks once).
    unpinned: HashSet<String>,
}

impl ContractTargets {
    /// Claude Code's `setArtifactContractTarget(slug, contract?, {cachePinOnly})`:
    /// the artifact becomes the target (a valid `contract` its pin, else it
    /// keeps the one it had); with `cache_pin_only`, only the pin is noted.
    pub(crate) fn set(&self, session: &str, slug: &str, contract: Option<&str>, cache_pin_only: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let refs = inner.session(session);
        let at = refs.iter().position(|r| r.slug == slug);
        let pin = valid_pin(contract).or_else(|| at.and_then(|i| refs[i].pin.clone()));
        if cache_pin_only {
            match at {
                Some(i) => refs[i].pin = pin,
                None if pin.is_some() && !refs.is_empty() => refs.push(Ref { slug: slug.to_owned(), pin }),
                None => {}
            }
        } else {
            if let Some(i) = at {
                refs.remove(i);
            }
            refs.insert(0, Ref { slug: slug.to_owned(), pin });
        }
        refs.truncate(REFS_PER_SESSION);
    }

    pub(crate) fn get(&self, session: &str) -> Target {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(refs) = inner.sessions.get(session) else { return Target::default() };
        Target {
            slug: refs.first().map(|r| r.slug.clone()),
            pins: refs.iter().filter_map(|r| Some((r.slug.clone(), r.pin.clone()?))).collect(),
        }
    }

    pub(crate) fn is_unpinned(&self, slug: &str) -> bool {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).unpinned.contains(slug)
    }

    pub(crate) fn mark_unpinned(&self, slug: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.unpinned.len() >= NO_PIN_SLUGS {
            inner.unpinned.clear();
        }
        inner.unpinned.insert(slug.to_owned());
    }
}

impl Inner {
    fn session(&mut self, session: &str) -> &mut Vec<Ref> {
        if !self.sessions.contains_key(session) {
            while self.order.len() >= SESSIONS {
                if let Some(old) = self.order.pop_front() {
                    self.sessions.remove(&old);
                }
            }
            self.order.push_back(session.to_owned());
        }
        self.sessions.entry(session.to_owned()).or_default()
    }
}

/// Claude Code's `u2`: a contract version that can pin (`0.0.0` is none).
pub(crate) fn valid_pin(contract: Option<&str>) -> Option<String> {
    contract.filter(|v| *v != "0.0.0" && crate::capabilities::valid_version(v)).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_artifact_is_the_target_and_keeps_its_pin() {
        let t = ContractTargets::default();
        assert_eq!(t.get("s"), Target::default());
        t.set("s", "a", Some("0.2.90"), false);
        t.set("s", "b", None, false);
        let got = t.get("s");
        assert_eq!(got.slug.as_deref(), Some("b"));
        assert_eq!(got.pins.get("a").map(String::as_str), Some("0.2.90"));
        // Back to `a` without a contract: it keeps its pin.
        t.set("s", "a", None, false);
        assert_eq!(t.get("s").slug.as_deref(), Some("a"));
        assert_eq!(t.get("s").pins.len(), 1);
        // Only the pin: the target stays; an unknown artifact is appended.
        t.set("s", "c", Some("0.2.91"), true);
        t.set("s", "b", Some("0.0.0"), true);
        let got = t.get("s");
        assert_eq!(got.slug.as_deref(), Some("a"));
        assert_eq!(got.pins.get("c").map(String::as_str), Some("0.2.91"));
        assert!(!got.pins.contains_key("b"), "0.0.0 pins nothing");
        // A pin-only note on an empty session is dropped.
        t.set("other", "x", Some("0.2.91"), true);
        assert_eq!(t.get("other"), Target::default());
        assert!(valid_pin(Some("1.2")).is_none());
    }
}
