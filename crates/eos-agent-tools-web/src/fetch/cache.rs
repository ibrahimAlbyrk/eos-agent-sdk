//! The page cache every agent shares: least recently used out first, bounded
//! by total bytes and entry count, entries expire a fixed time after storing.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Claude Code's defaults: 15 minutes, 50 MiB.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_CACHE_BYTES: usize = 50 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const PURGE_EVERY: Duration = Duration::from_secs(60);

/// A fetched page, after conversion to text.
#[derive(Debug)]
pub(crate) struct Page {
    pub content: String,
    pub content_type: String,
    /// Where a binary body was written, and its size.
    pub saved: Option<(PathBuf, u64)>,
}

#[derive(Debug)]
pub struct FetchCache {
    ttl: Duration,
    max_bytes: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    map: HashMap<Arc<str>, Slot>,
    /// Use tick → key, oldest first.
    order: BTreeMap<u64, Arc<str>>,
    tick: u64,
    bytes: usize,
    last_purge: Instant,
}

#[derive(Debug)]
struct Slot {
    page: Arc<Page>,
    stored: Instant,
    size: usize,
    tick: u64,
}

impl Default for FetchCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_TTL, DEFAULT_CACHE_BYTES)
    }
}

impl FetchCache {
    pub fn new(ttl: Duration, max_bytes: usize) -> Self {
        let inner =
            Inner { map: HashMap::new(), order: BTreeMap::new(), tick: 0, bytes: 0, last_purge: Instant::now() };
        Self { ttl, max_bytes: max_bytes.max(1), inner: Mutex::new(inner) }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Total bytes held (for tests and metrics).
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.map.clear();
        inner.order.clear();
        inner.bytes = 0;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The page, marked as recently used.
    pub(crate) fn get(&self, url: &str) -> Option<Arc<Page>> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        let (key, slot) = inner.map.get_key_value(url)?;
        if slot.stored.elapsed() >= self.ttl {
            let key = Arc::clone(key);
            inner.remove(&key);
            return None;
        }
        let key = Arc::clone(key);
        inner.tick += 1;
        let tick = inner.tick;
        let slot = inner.map.get_mut(&key)?;
        inner.order.remove(&slot.tick);
        slot.tick = tick;
        inner.order.insert(tick, key);
        Some(Arc::clone(&slot.page))
    }

    /// The page without touching its recency.
    pub(crate) fn peek(&self, url: &str) -> Option<Arc<Page>> {
        let inner = self.lock();
        inner.map.get(url).filter(|s| s.stored.elapsed() < self.ttl).map(|s| Arc::clone(&s.page))
    }

    pub(crate) fn insert(&self, url: &str, page: Arc<Page>, size: usize) {
        let size = size.max(1);
        if size > self.max_bytes {
            return;
        }
        let mut inner = self.lock();
        if inner.last_purge.elapsed() >= PURGE_EVERY {
            inner.purge(self.ttl);
        }
        inner.remove(url);
        while inner.bytes + size > self.max_bytes || inner.map.len() >= MAX_ENTRIES {
            let Some((_, oldest)) = inner.order.pop_first() else { break };
            if let Some(slot) = inner.map.remove(&oldest) {
                inner.bytes -= slot.size;
            }
        }
        inner.tick += 1;
        let tick = inner.tick;
        let key: Arc<str> = url.into();
        inner.order.insert(tick, Arc::clone(&key));
        inner.map.insert(key, Slot { page, stored: Instant::now(), size, tick });
        inner.bytes += size;
    }
}

impl Inner {
    fn remove(&mut self, url: &str) {
        if let Some(slot) = self.map.remove(url) {
            self.order.remove(&slot.tick);
            self.bytes -= slot.size;
        }
    }

    fn purge(&mut self, ttl: Duration) {
        let stale: Vec<Arc<str>> =
            self.map.iter().filter(|(_, s)| s.stored.elapsed() >= ttl).map(|(k, _)| Arc::clone(k)).collect();
        for key in stale {
            self.remove(&key);
        }
        self.last_purge = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str) -> Arc<Page> {
        Arc::new(Page { content: text.into(), content_type: "text/plain".into(), saved: None })
    }

    #[test]
    fn evicts_least_recently_used_by_bytes() {
        let cache = FetchCache::new(Duration::from_secs(60), 10);
        cache.insert("a", page("aaaa"), 4);
        cache.insert("b", page("bbbb"), 4);
        assert!(cache.get("a").is_some()); // a is now newer than b
        cache.insert("c", page("cccc"), 4);
        assert!(cache.peek("b").is_none());
        assert!(cache.peek("a").is_some());
        assert!(cache.peek("c").is_some());
        assert_eq!(cache.bytes(), 8);
        cache.insert("huge", page("x"), 11);
        assert!(cache.peek("huge").is_none());
    }

    #[test]
    fn entries_expire() {
        let cache = FetchCache::new(Duration::ZERO, 100);
        cache.insert("a", page("a"), 1);
        assert!(cache.get("a").is_none());
        assert_eq!(cache.bytes(), 0);
    }
}
