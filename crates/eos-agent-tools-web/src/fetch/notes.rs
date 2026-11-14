//! Short notes that spare a refetch: a URL that failed moments ago, or the
//! same fetch an agent already holds the answer to.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use super::cache::Page;

const FAILURE_TTL: Duration = Duration::from_secs(300);
const MAX_FAILURES: usize = 1024;
const MAX_TOLD: usize = 64;
const MAX_HISTORY: usize = 64;

/// URLs whose fetch failed recently, shared by every agent. Each agent is
/// told once; asking again fetches for real.
#[derive(Debug, Default)]
pub(crate) struct FailureLog {
    map: Mutex<HashMap<String, Failure>>,
}

#[derive(Debug)]
struct Failure {
    kind: &'static str,
    at: Instant,
    told: Vec<Arc<str>>,
}

impl FailureLog {
    pub(crate) fn record(&self, url: &str, kind: &'static str) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if map.len() >= MAX_FAILURES {
            map.retain(|_, f| f.at.elapsed() < FAILURE_TTL);
            if map.len() >= MAX_FAILURES {
                let oldest = map.iter().min_by_key(|(_, f)| f.at).map(|(k, _)| k.clone());
                if let Some(oldest) = oldest {
                    map.remove(&oldest);
                }
            }
        }
        map.insert(url.to_owned(), Failure { kind, at: Instant::now(), told: Vec::new() });
    }

    pub(crate) fn clear(&self, url: &str) {
        self.map.lock().unwrap_or_else(PoisonError::into_inner).remove(url);
    }

    /// `(kind, seconds ago)` when `agent` has not heard of this failure yet.
    pub(crate) fn tell(&self, url: &str, agent: &str) -> Option<(&'static str, u64)> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let failure = map.get_mut(url)?;
        if failure.at.elapsed() >= FAILURE_TTL {
            map.remove(url);
            return None;
        }
        if failure.told.iter().any(|a| &**a == agent) || failure.told.len() >= MAX_TOLD {
            return None;
        }
        failure.told.push(agent.into());
        Some((failure.kind, failure.at.elapsed().as_secs()))
    }
}

/// One agent's recent fetches (kept in its extensions).
#[derive(Debug, Default)]
pub(crate) struct FetchHistory {
    records: Mutex<VecDeque<Record>>,
}

#[derive(Debug)]
struct Record {
    url: String,
    key: u64,
    tool_use_id: String,
    at: Instant,
    /// The cache entry the answer came from: once it is evicted or replaced,
    /// a refetch could return something new, so no note is given.
    page: Weak<Page>,
    noted: bool,
}

/// What an earlier identical fetch left behind.
#[derive(Debug)]
pub(crate) struct Earlier {
    pub tool_use_id: String,
    pub seconds_ago: u64,
}

pub(crate) fn call_key(prompt: &str, offset: usize) -> u64 {
    let mut h = DefaultHasher::new();
    (offset, prompt).hash(&mut h);
    h.finish()
}

impl FetchHistory {
    /// The earlier identical fetch, if its page is still the cached one and
    /// the agent was not already told about it.
    pub(crate) fn repeat(&self, url: &str, key: u64, cached: Option<&Arc<Page>>) -> Option<Earlier> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let record = records.iter_mut().find(|r| r.key == key && r.url == url)?;
        let same_page = cached.is_some_and(|page| record.page.upgrade().is_some_and(|p| Arc::ptr_eq(&p, page)));
        if record.noted || !same_page {
            return None;
        }
        record.noted = true;
        Some(Earlier { tool_use_id: record.tool_use_id.clone(), seconds_ago: record.at.elapsed().as_secs() })
    }

    pub(crate) fn remember(&self, url: &str, key: u64, tool_use_id: &str, page: &Arc<Page>) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.retain(|r| !(r.key == key && r.url == url));
        if records.len() >= MAX_HISTORY {
            records.pop_front();
        }
        records.push_back(Record {
            url: url.to_owned(),
            key,
            tool_use_id: tool_use_id.to_owned(),
            at: Instant::now(),
            page: Arc::downgrade(page),
            noted: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_told_once_per_agent() {
        let log = FailureLog::default();
        log.record("https://x.dev", "url_not_found");
        assert_eq!(log.tell("https://x.dev", "a1").map(|t| t.0), Some("url_not_found"));
        assert!(log.tell("https://x.dev", "a1").is_none());
        assert!(log.tell("https://x.dev", "a2").is_some());
        log.clear("https://x.dev");
        assert!(log.tell("https://x.dev", "a3").is_none());
    }
}
