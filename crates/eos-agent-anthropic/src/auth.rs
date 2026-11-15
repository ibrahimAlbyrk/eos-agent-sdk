//! Credentials: an API key, a fixed bearer token, or a Claude sign-in that is
//! read from where Claude Code keeps it and renewed when it lapses.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{BoxFuture, ModelError};
pub use eos_agent_core::{Credential, StaticCredential, TokenSource};
use serde_json::Value;
use tokio::sync::Mutex;

mod sign_in;

pub use sign_in::{ClaudeAccount, SignIn, SignInConfig, SignInError};

use eos_agent_oauth::{DirLock, form_encode, now_ms};
pub use eos_agent_oauth::{FallbackSecrets, FileSecrets, KeychainSecrets, MemorySecrets, SecretStore};

/// `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN`.
pub fn from_env() -> Option<Arc<dyn TokenSource>> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if let Some(key) = var("ANTHROPIC_API_KEY") {
        return Some(Arc::new(StaticCredential(Credential::ApiKey(key))));
    }
    var("ANTHROPIC_AUTH_TOKEN")
        .or_else(|| var("CLAUDE_CODE_OAUTH_TOKEN"))
        .map(|t| Arc::new(StaticCredential(Credential::Bearer(t))) as Arc<dyn TokenSource>)
}

// Claude's OAuth client, as Claude Code registers it.
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Renew this long before the access token expires (Claude Code's margin).
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
// Claude Code's refresh lock: a directory in the store dir. A holder touches it
// while working; one untouched this long belonged to a process that died.
const LOCK_NAME: &str = ".oauth_refresh.lock";
const LOCK_STALE: Duration = Duration::from_secs(60);
const LOCK_WAIT: Duration = Duration::from_secs(45);
const LOCK_POLL: Duration = Duration::from_millis(250);
/// A renewal runs under the lock every agent on the sign-in waits for, so it
/// may never hang: past this it fails and the next request tries again.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(30);
const RENEWAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A Claude sign-in shared with Claude Code (and Eos): the same secret item,
/// the same refresh lock. Refresh tokens are single-use, so renewals across
/// every process using this sign-in are serialized by that lock, and the
/// stored token is re-read once the lock is held.
pub struct ClaudeLogin {
    secrets: Arc<dyn SecretStore>,
    lock_dir: PathBuf,
    token_url: String,
    http: reqwest::Client,
    cached: Mutex<Option<Bundle>>,
}

impl std::fmt::Debug for ClaudeLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeLogin").field("lock_dir", &self.lock_dir).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
struct Bundle {
    access: String,
    refresh: Option<String>,
    expires_at_ms: Option<u64>,
}

impl Bundle {
    fn fresh(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_none_or(|exp| exp.saturating_sub(now_ms) > REFRESH_MARGIN.as_millis() as u64)
    }
}

impl ClaudeLogin {
    /// The sign-in in macOS Keychain for `store_dir` (Claude Code's
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR`), or the default one in `~/.claude`;
    /// as in Claude Code, the plaintext `.credentials.json` there when the
    /// Keychain item is empty.
    pub fn keychain(store_dir: Option<&Path>) -> Self {
        let lock_parent = store_dir.map_or_else(|| home().join(".claude"), Path::to_path_buf);
        let keychain = Arc::new(KeychainSecrets::for_claude_store(store_dir));
        let secrets = Arc::new(FallbackSecrets::new(keychain, lock_parent.join(".credentials.json")));
        Self::new(secrets, lock_parent)
    }

    pub fn new(secrets: Arc<dyn SecretStore>, lock_parent: PathBuf) -> Self {
        Self {
            secrets,
            lock_dir: lock_parent.join(LOCK_NAME),
            token_url: TOKEN_URL.into(),
            http: renewal_client(RENEWAL_TIMEOUT),
            cached: Mutex::new(None),
        }
    }

    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.token_url = url.into();
        self
    }

    /// How long one renewal (or sign-in exchange) may take before it fails
    /// (default 30 s).
    pub fn with_renewal_timeout(mut self, timeout: Duration) -> Self {
        self.http = renewal_client(timeout);
        self
    }

    async fn read(&self) -> Result<(Value, Bundle), ModelError> {
        let raw = self.secrets.read().await.map_err(|e| ModelError::Auth(format!("reading the sign-in: {e}")))?;
        let raw = raw.ok_or_else(|| ModelError::Auth("not signed in to Claude".into()))?;
        let doc: Value =
            serde_json::from_str(&raw).map_err(|e| ModelError::Auth(format!("unreadable sign-in: {e}")))?;
        let o = &doc["claudeAiOauth"];
        let access = o["accessToken"].as_str().ok_or_else(|| ModelError::Auth("sign-in has no access token".into()))?;
        let bundle = Bundle {
            access: access.to_owned(),
            refresh: o["refreshToken"].as_str().map(str::to_owned),
            expires_at_ms: o["expiresAt"].as_u64(),
        };
        Ok((doc, bundle))
    }

    /// Renews under the shared lock. `rejected` forces a renewal while the
    /// stored token is still the one the server refused.
    async fn renew(&self, rejected: Option<&str>) -> Result<Bundle, ModelError> {
        let _lock = DirLock::wait_for(&self.lock_dir, LOCK_STALE, LOCK_POLL, LOCK_WAIT)
            .await
            .map_err(|e| ModelError::Auth(format!("sign-in lock: {e}")))?
            .ok_or_else(|| ModelError::Transient("another process is renewing the sign-in".into()))?;
        let (mut doc, current) = self.read().await?;
        let still_needed = !current.fresh(now_ms()) || rejected == Some(current.access.as_str());
        if !still_needed {
            return Ok(current); // another process renewed it while we waited
        }
        let refresh = current.refresh.clone().ok_or_else(|| ModelError::Auth("sign-in cannot be renewed".into()))?;
        let res = self
            .http
            .post(&self.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form_encode(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", CLIENT_ID),
            ]))
            .send()
            .await
            .map_err(|e| ModelError::Transient(format!("renewing the sign-in: {e}")))?;
        let status = res.status();
        let text = res.text().await.map_err(|e| ModelError::Transient(format!("renewing the sign-in: {e}")))?;
        let body: Value =
            serde_json::from_str(&text).map_err(|e| ModelError::Auth(format!("renewal answer unreadable: {e}")))?;
        if !status.is_success() {
            return Err(ModelError::Auth(format!("sign-in renewal refused ({status}); sign in again")));
        }
        let access = body["access_token"].as_str().ok_or_else(|| ModelError::Auth("renewal gave no token".into()))?;
        let o = &mut doc["claudeAiOauth"];
        o["accessToken"] = Value::from(access);
        if let Some(r) = body["refresh_token"].as_str() {
            o["refreshToken"] = Value::from(r);
        }
        if let Some(secs) = body["expires_in"].as_u64() {
            o["expiresAt"] = Value::from(now_ms() + secs * 1000);
        }
        self.secrets.write(&doc.to_string()).await.map_err(|e| ModelError::Auth(format!("saving the renewal: {e}")))?;
        Ok(self.read().await?.1)
    }
}

impl TokenSource for ClaudeLogin {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        Box::pin(async move {
            let mut cached = self.cached.lock().await;
            let now = now_ms();
            if let Some(b) = cached.as_ref().filter(|b| b.fresh(now)) {
                return Ok(Credential::Bearer(b.access.clone()));
            }
            let (_, mut bundle) = self.read().await?;
            if !bundle.fresh(now) {
                bundle = self.renew(None).await?;
            }
            let access = bundle.access.clone();
            *cached = Some(bundle);
            Ok(Credential::Bearer(access))
        })
    }

    fn refused<'a>(&'a self, rejected: &'a Credential) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Credential::Bearer(token) = rejected else { return false };
            let mut cached = self.cached.lock().await;
            match self.renew(Some(token)).await {
                Ok(b) => {
                    let changed = &b.access != token;
                    *cached = Some(b);
                    changed
                }
                Err(_) => false,
            }
        })
    }
}

fn renewal_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(RENEWAL_CONNECT_TIMEOUT.min(timeout))
        .timeout(timeout)
        .build()
        .unwrap_or_default()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}
