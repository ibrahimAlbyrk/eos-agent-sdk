//! MCP sign-ins where Claude Code keeps them: entries of its credentials JSON
//! (`mcpOAuth`, `mcpOAuthClientConfig`), keyed per server config, changed
//! only under its storage lock so neither program loses the other's writes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::SecretStore;
use eos_agent_oauth::{DirLock, MemorySecrets, lockfile_path};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Where MCP sign-ins are kept: a secret document (Claude Code's
/// credentials item, so its logins are shared both ways) and the folder
/// holding Claude Code's locks for it (`~/.claude`, or the store dir).
#[derive(Clone)]
pub struct McpCredentials {
    secrets: Arc<dyn SecretStore>,
    lock_dir: PathBuf,
    pub(crate) needs_auth: super::needs_auth::NeedsAuthCache,
}

impl std::fmt::Debug for McpCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredentials").field("lock_dir", &self.lock_dir).finish_non_exhaustive()
    }
}

impl McpCredentials {
    pub fn new(secrets: Arc<dyn SecretStore>, lock_dir: impl Into<PathBuf>) -> Self {
        let lock_dir = lock_dir.into();
        let needs_auth = super::needs_auth::NeedsAuthCache::file(&lock_dir);
        Self { secrets, lock_dir, needs_auth }
    }

    /// Kept in memory only: sign-ins last as long as the process.
    pub fn in_memory() -> Self {
        let dir = std::env::temp_dir().join(format!("eos-mcp-oauth-{}", std::process::id()));
        let needs_auth = super::needs_auth::NeedsAuthCache::default();
        Self { secrets: Arc::new(MemorySecrets::default()), lock_dir: dir, needs_auth }
    }

    /// Keeps the needs-auth cache in Claude Code's config directory
    /// (`CLAUDE_CONFIG_DIR` or `~/.claude`) when the store lives elsewhere.
    #[must_use]
    pub fn with_config_dir(mut self, dir: &Path) -> Self {
        self.needs_auth = super::needs_auth::NeedsAuthCache::file(dir);
        self
    }

    pub fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }

    /// The whole document (`{}` when there is none yet).
    pub(crate) async fn read(&self) -> Result<Value, String> {
        let text = self.secrets.read().await.map_err(|e| format!("reading the credential store: {e}"))?;
        match text.as_deref().map(str::trim) {
            None | Some("") => Ok(Value::Object(Map::new())),
            Some(t) => serde_json::from_str(t).map_err(|e| format!("the credential store is not JSON: {e}")),
        }
    }

    pub(crate) async fn entry(&self, key: &str) -> Result<Entry, String> {
        Ok(Entry::from_doc(&self.read().await?, key))
    }

    /// Changes the document under Claude Code's storage lock, re-read once the
    /// lock is held. `change` returns false to leave it as it was.
    pub(crate) async fn mutate(&self, change: impl FnOnce(&mut Map<String, Value>) -> bool) -> Result<(), String> {
        let target = self.lock_dir.join(".storage-write");
        let _lock = DirLock::take(&lockfile_path(&target), Duration::from_secs(15), 10, |n| {
            Duration::from_millis(50 << n.min(4))
        })
        .await
        .map_err(|e| format!("credential store lock: {e}"))?
        .ok_or("the credential store is locked by another process")?;
        let mut doc = self.read().await?;
        let Value::Object(root) = &mut doc else { return Err("the credential store is not a JSON object".into()) };
        if !change(root) {
            return Ok(());
        }
        let text = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
        self.secrets.write(&text).await.map_err(|e| format!("saving to the credential store: {e}"))
    }

    /// Changes one server's `mcpOAuth` entry (created when missing).
    pub(crate) async fn mutate_entry(
        &self,
        key: &str,
        change: impl FnOnce(&mut Map<String, Value>) -> bool,
    ) -> Result<(), String> {
        self.mutate(|root| {
            let all = root.entry("mcpOAuth").or_insert_with(|| Value::Object(Map::new()));
            if !all.is_object() {
                *all = Value::Object(Map::new());
            }
            let Some(all) = all.as_object_mut() else { return false };
            let slot = all.entry(key).or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            slot.as_object_mut().is_some_and(change)
        })
        .await
    }

    /// Claude Code's per-server refresh lock target (`mcp-refresh-<key>.lock`).
    pub(crate) fn refresh_lock(&self, key: &str) -> PathBuf {
        let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        self.lock_dir.join(format!("mcp-refresh-{safe}.lock"))
    }
}

/// One server's stored sign-in, as Claude Code writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) access_token: Option<String>,
    pub(crate) refresh_token: Option<String>,
    /// Unix ms.
    pub(crate) expires_at: Option<u64>,
    pub(crate) scope: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
    pub(crate) redirect_uri: Option<String>,
    pub(crate) step_up_scope: Option<String>,
    /// `discoveryState.authorizationServerUrl`.
    pub(crate) auth_server_url: Option<String>,
    /// `discoveryState.resourceMetadataUrl`.
    pub(crate) resource_metadata_url: Option<String>,
    /// `mcpOAuthClientConfig.<key>.clientSecret`: the secret of a configured client.
    pub(crate) configured_secret: Option<String>,
    /// The entry has a `discoveryState`.
    pub(crate) has_discovery: bool,
    /// `discoveryState.oauthMetadataFound`.
    pub(crate) metadata_found: Option<bool>,
    /// `discoveryState.tokenlessRejectedAt` (unix ms): when the server last
    /// turned away a request that carried no token.
    pub(crate) tokenless_rejected_at: Option<u64>,
    /// `discoveryState.registrationNotOfferedAt` (unix ms).
    pub(crate) registration_not_offered_at: Option<u64>,
    /// `discoveryState.xaaPinned`: the authorization server an XAA sign-in used.
    pub(crate) xaa_pinned: bool,
}

impl Entry {
    fn from_doc(doc: &Value, key: &str) -> Self {
        let e = &doc["mcpOAuth"][key];
        let s = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let state = &e["discoveryState"];
        Self {
            access_token: s(&e["accessToken"]),
            refresh_token: s(&e["refreshToken"]),
            expires_at: e["expiresAt"].as_f64().filter(|ms| *ms > 0.0).map(|ms| ms as u64),
            scope: s(&e["scope"]),
            client_id: s(&e["clientId"]),
            client_secret: s(&e["clientSecret"]),
            redirect_uri: s(&e["redirectUri"]),
            step_up_scope: s(&e["stepUpScope"]),
            auth_server_url: s(&state["authorizationServerUrl"]),
            resource_metadata_url: s(&state["resourceMetadataUrl"]),
            configured_secret: s(&doc["mcpOAuthClientConfig"][key]["clientSecret"]),
            has_discovery: state.is_object(),
            metadata_found: state["oauthMetadataFound"].as_bool(),
            tokenless_rejected_at: state["tokenlessRejectedAt"].as_f64().map(|ms| ms as u64),
            registration_not_offered_at: state["registrationNotOfferedAt"].as_f64().map(|ms| ms as u64),
            xaa_pinned: state["xaaPinned"] == Value::Bool(true),
        }
    }

    /// Claude Code's `Kin`: no tokens, but discovery found the authorization
    /// server's metadata.
    pub(crate) fn discovered_without_tokens(&self) -> bool {
        self.access_token.is_none() && self.refresh_token.is_none() && self.metadata_found == Some(true)
    }

    /// The authorization server an XAA sign-in pinned (`FZ`).
    pub(crate) fn xaa_pin(&self) -> Option<&str> {
        self.auth_server_url.as_deref().filter(|_| self.xaa_pinned)
    }

    /// Seconds the access token has left (`None`: no expiry recorded).
    pub(crate) fn expires_in(&self, now_ms: u64) -> Option<i64> {
        self.expires_at
            .map(|at| (i128::from(at) - i128::from(now_ms)).clamp(i64::MIN.into(), i64::MAX.into()) as i64 / 1000)
    }
}

/// Claude Code's key for a server's sign-in: its name and a hash of what it
/// reaches, `<name>|<first 16 hex of sha256(JSON {type, url, headers})>`,
/// the type and the headers' order as the config wrote them.
pub(crate) fn server_key(
    name: &str,
    kind: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    order: &[String],
) -> String {
    let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let mut names: Vec<&String> = order.iter().filter(|n| headers.contains_key(*n)).collect();
    names.dedup();
    names.extend(headers.keys().filter(|k| !order.contains(k)));
    let fields: Vec<String> = names.iter().map(|n| format!("{}:{}", quoted(n), quoted(&headers[*n]))).collect();
    let json = format!(r#"{{"type":{},"url":{},"headers":{{{}}}}}"#, quoted(kind), quoted(url), fields.join(","));
    let hash = hex::encode(Sha256::digest(json.as_bytes()));
    format!("{name}|{}", &hash[..16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_hash_what_the_server_reaches_like_claude_code() {
        // Node: sha256(JSON.stringify({type:"http",url:"https://x.dev/mcp",headers:{A:"1",b:"2"}})).slice(0,16)
        let headers = BTreeMap::from([("A".to_owned(), "1".to_owned()), ("b".to_owned(), "2".to_owned())]);
        assert_eq!(server_key("x", "http", "https://x.dev/mcp", &headers, &[]), "x|b3d0a1dc8348c5ae");
        // Written as {b, A}: Node hashes that order.
        let reordered = server_key("x", "http", "https://x.dev/mcp", &headers, &["b".into(), "A".into()]);
        let json = r#"{"type":"http","url":"https://x.dev/mcp","headers":{"b":"2","A":"1"}}"#;
        assert_eq!(reordered, format!("x|{}", &hex::encode(Sha256::digest(json.as_bytes()))[..16]));
        let streamable = server_key("x", "streamable-http", "https://x.dev/mcp", &BTreeMap::new(), &[]);
        let json = r#"{"type":"streamable-http","url":"https://x.dev/mcp","headers":{}}"#;
        assert_eq!(streamable, format!("x|{}", &hex::encode(Sha256::digest(json.as_bytes()))[..16]));
    }

    #[tokio::test]
    async fn entries_are_patched_in_place() {
        let creds = McpCredentials::in_memory();
        creds
            .mutate(|root| {
                root.insert("claudeAiOauth".into(), serde_json::json!({"accessToken": "keep"}));
                true
            })
            .await
            .expect("write");
        creds
            .mutate_entry("s|1", |e| {
                e.insert("accessToken".into(), "a".into());
                e.insert("expiresAt".into(), 5000.into());
                true
            })
            .await
            .expect("write");
        let doc = creds.read().await.expect("read");
        assert_eq!(doc["claudeAiOauth"]["accessToken"], "keep");
        let entry = creds.entry("s|1").await.expect("read");
        assert_eq!((entry.access_token.as_deref(), entry.expires_in(1000)), (Some("a"), Some(4)));
        creds.mutate(|root| root.remove("mcpOAuth").is_some()).await.expect("remove");
        assert_eq!(creds.entry("s|1").await.expect("read"), Entry::default());
    }
}
