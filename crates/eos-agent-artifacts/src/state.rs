//! What one agent has seen of its artifacts, kept in its extensions: the
//! version it last read or published (an update is pinned to it) and which
//! local file published which artifact (republishing a file updates it).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

const CAPACITY: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Known {
    pub version: String,
    pub title: Option<String>,
    /// Made from an Artifact type: its page is the type's, its files are data.
    pub typed: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ArtifactState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    known: HashMap<String, Known>,
    by_path: HashMap<PathBuf, String>,
    order: VecDeque<String>,
}

impl ArtifactState {
    pub(crate) fn get(&self, slug: &str) -> Option<Known> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).known.get(slug).cloned()
    }

    pub(crate) fn record(&self, slug: &str, known: Known) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.known.insert(slug.to_owned(), known).is_some() {
            inner.order.retain(|s| s != slug);
        }
        inner.order.push_back(slug.to_owned());
        while inner.order.len() > CAPACITY {
            if let Some(old) = inner.order.pop_front() {
                inner.known.remove(&old);
                inner.by_path.retain(|_, s| *s != old);
            }
        }
    }

    pub(crate) fn slug_for_path(&self, path: &Path) -> Option<String> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).by_path.get(path).cloned()
    }

    pub(crate) fn bind_path(&self, path: &Path, slug: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.known.contains_key(slug) {
            inner.by_path.insert(path.to_owned(), slug.to_owned());
        }
    }

    pub(crate) fn forget(&self, slug: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.known.remove(slug);
        inner.order.retain(|s| s != slug);
        inner.by_path.retain(|_, s| s != slug);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_and_paths_follow_their_artifact() {
        let state = ArtifactState::default();
        let known = Known { version: "v1".into(), title: None, typed: false };
        state.record("a", known.clone());
        state.bind_path(Path::new("/p/a.html"), "a");
        for i in 0..CAPACITY {
            state.record(&format!("s{i}"), known.clone());
        }
        assert!(state.get("a").is_none());
        assert!(state.slug_for_path(Path::new("/p/a.html")).is_none());
        state.forget("s1");
        assert!(state.get("s1").is_none());
    }
}
