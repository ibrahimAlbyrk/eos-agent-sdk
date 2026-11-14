//! Claude Code's needs-auth cache (`~/.claude/mcp-needs-auth-cache.json`):
//! a remote server that asked for a sign-in is not connected again for 15
//! minutes, in any process; a sign-in or a connection that works clears it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};

use super::now_ms;

/// How long a server stays known to need a sign-in.
const TTL_MS: i64 = 15 * 60 * 1000;
/// Clock skew Claude Code allows for an entry written "in the future".
const SKEW_MS: i64 = 60 * 1000;

#[derive(Debug, Clone)]
pub(crate) enum NeedsAuthCache {
    File(PathBuf),
    /// For credentials kept in memory: the cache too.
    Memory(Arc<Mutex<HashMap<String, u64>>>),
}

impl Default for NeedsAuthCache {
    fn default() -> Self {
        Self::Memory(Arc::default())
    }
}

impl NeedsAuthCache {
    pub(crate) fn file(config_dir: &Path) -> Self {
        Self::File(config_dir.join("mcp-needs-auth-cache.json"))
    }

    /// Whether `server` asked for a sign-in in the last 15 minutes.
    pub(crate) async fn fresh(&self, server: &str) -> bool {
        let stamp = match self {
            Self::Memory(map) => map.lock().unwrap_or_else(PoisonError::into_inner).get(server).copied(),
            Self::File(path) => read(path).await.get(server).and_then(|e| e.get("timestamp")).and_then(Value::as_u64),
        };
        let Some(stamp) = stamp else { return false };
        let age = i64::try_from(now_ms()).unwrap_or(i64::MAX) - i64::try_from(stamp).unwrap_or(i64::MAX);
        age > -SKEW_MS && age < TTL_MS
    }

    pub(crate) async fn mark(&self, server: &str) {
        self.change(server, Some(now_ms())).await;
    }

    pub(crate) async fn clear(&self, server: &str) {
        self.change(server, None).await;
    }

    async fn change(&self, server: &str, stamp: Option<u64>) {
        match self {
            Self::Memory(map) => {
                let mut map = map.lock().unwrap_or_else(PoisonError::into_inner);
                match stamp {
                    Some(t) => map.insert(server.to_owned(), t),
                    None => map.remove(server),
                };
            }
            Self::File(path) => {
                let mut doc = read(path).await;
                let changed = match stamp {
                    Some(t) => {
                        doc.insert(server.to_owned(), json!({ "timestamp": t }));
                        true
                    }
                    None => doc.remove(server).is_some(),
                };
                if changed {
                    if let Some(dir) = path.parent() {
                        let _ = tokio::fs::create_dir_all(dir).await;
                    }
                    let _ = tokio::fs::write(path, Value::Object(doc).to_string()).await;
                }
            }
        }
    }
}

impl super::ServerAuth {
    /// The server asked for a sign-in within the last 15 minutes (in any process).
    pub(crate) async fn known_to_need_auth(&self) -> bool {
        self.ctx.credentials.needs_auth.fresh(&self.name).await
    }

    /// Notes that the server asks for a sign-in.
    pub(crate) fn note_needs_auth(&self) {
        let (cache, name) = (self.ctx.credentials.needs_auth.clone(), self.name.clone());
        tokio::spawn(async move { cache.mark(&name).await });
    }

    /// Notes that the server took a sign-in (or needed none).
    pub(crate) fn note_connected(&self) {
        self.forget_tokenless_rejection();
        let (cache, name) = (self.ctx.credentials.needs_auth.clone(), self.name.clone());
        tokio::spawn(async move { cache.clear(&name).await });
    }
}

async fn read(path: &Path) -> Map<String, Value> {
    let Ok(text) = tokio::fs::read_to_string(path).await else { return Map::new() };
    match serde_json::from_str(&text) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn entries_last_fifteen_minutes() {
        let dir = std::env::temp_dir().join(format!("eos-needs-auth-{}", std::process::id()));
        let cache = NeedsAuthCache::file(&dir);
        assert!(!cache.fresh("docs").await);
        cache.mark("docs").await;
        assert!(cache.fresh("docs").await);
        let old = now_ms() - 16 * 60 * 1000;
        tokio::fs::write(dir.join("mcp-needs-auth-cache.json"), json!({"docs": {"timestamp": old}}).to_string())
            .await
            .expect("write");
        assert!(!cache.fresh("docs").await, "stale after 15 minutes");
        cache.mark("docs").await;
        cache.clear("docs").await;
        assert!(!cache.fresh("docs").await);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
