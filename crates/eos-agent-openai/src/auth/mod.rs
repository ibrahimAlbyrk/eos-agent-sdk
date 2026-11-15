//! A ChatGPT plan sign-in ("Sign in with ChatGPT"): the stored record, the
//! access token it hands out, and its renewal. Refresh tokens rotate (each
//! renewal spends the old one), so renewals by every process using a store
//! are serialized by a lock in the store directory, and the record is read
//! again once the lock is held.

mod login;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{BoxFuture, Credential, ModelError, SecretStore, TokenSource};
use eos_agent_oauth::{DirLock, FileSecrets, now_ms, post_form};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

pub use login::{BrowserOpener, LoginError, PrintOpener, sign_in};

/// The record's file in a store directory.
const RECORD: &str = "siwc.json";
/// This host's id (`ext_agent_host_id`), kept apart so signing out keeps it.
const HOST_ID: &str = "host-id";
const LOCK: &str = ".oauth_refresh.lock";
const LOCK_STALE: Duration = Duration::from_secs(60);
const LOCK_WAIT: Duration = Duration::from_secs(45);
const LOCK_POLL: Duration = Duration::from_millis(250);
/// A renewal runs under the lock every agent on the sign-in waits for, so it
/// may never hang: past this it fails and the next request tries again.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(30);
const RENEWAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Renew this long before the access token expires.
const REFRESH_MARGIN_MS: u64 = 5 * 60 * 1000;
/// The scope that lets a token spend the ChatGPT plan.
pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
/// Refresh answers that mean the refresh token is spent for good.
const DEAD_REFRESH: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];

/// Where the sign-in endpoints are.
#[derive(Debug, Clone)]
pub struct SignInConfig {
    pub issuer: String,
    pub authorize_url: String,
    pub token_url: String,
    /// The issuer's signing keys (`None`: from its OpenID configuration).
    pub jwks_url: Option<String>,
    /// The API the tokens are for.
    pub resource: String,
    /// `agent_name_hint` on a first sign-in: the app's name as the user sees it.
    pub app_name: String,
    /// The loopback port the browser returns to (another free port when taken).
    pub callback_port: u16,
    /// How long the sign-in waits for the browser.
    pub timeout: Duration,
}

impl Default for SignInConfig {
    fn default() -> Self {
        Self {
            issuer: "https://auth.openai.com".into(),
            authorize_url: "https://auth.openai.com/api/accounts/authorize".into(),
            token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
            jwks_url: None,
            resource: "https://api.openai.com/v1".into(),
            app_name: "Eos".into(),
            callback_port: 1455,
            timeout: Duration::from_secs(600),
        }
    }
}

/// One store directory's sign-in, as saved (mode 0600).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) email: Option<String>,
    pub(crate) issuer: String,
    pub(crate) subject: String,
    /// The client id issued at registration (never `dynamic_agent_client`).
    pub(crate) client_id: String,
    pub(crate) ext_agent_host_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<String>,
    /// Unix ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<u64>,
    /// Unix ms before which the server prefers no renewal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) earliest_refresh_at: Option<u64>,
    #[serde(default)]
    pub(crate) scopes: Vec<String>,
    /// Unix ms of the last token answer.
    #[serde(default)]
    pub(crate) saved_at: u64,
}

impl Record {
    fn fresh(&self, now: u64) -> bool {
        self.access_token.is_some() && self.expires_at.is_none_or(|exp| exp.saturating_sub(now) > REFRESH_MARGIN_MS)
    }

    /// Takes a token endpoint's answer: tokens, expiry, scopes together.
    pub(crate) fn apply(&mut self, answer: &serde_json::Value) {
        let s = |k: &str| answer[k].as_str().filter(|v| !v.is_empty()).map(str::to_owned);
        let now = now_ms();
        if let Some(access) = s("access_token") {
            self.access_token = Some(access);
        }
        if let Some(refresh) = s("refresh_token") {
            self.refresh_token = Some(refresh);
        }
        if let Some(id) = s("id_token") {
            self.id_token = Some(id);
        }
        let expires_in = answer["expires_in"].as_u64().unwrap_or(3600);
        self.expires_at = Some(now + expires_in * 1000);
        self.earliest_refresh_at =
            answer["earliest_refresh_at"].as_u64().map(|at| if at < 1 << 40 { at * 1000 } else { at });
        if let Some(scope) = s("scope") {
            self.scopes = scope.split_whitespace().map(str::to_owned).collect();
        }
        self.saved_at = now;
    }

    pub(crate) fn can_use_plan(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }
}

/// Who a store is signed in as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub email: Option<String>,
    /// The verified ID token's `sub`.
    pub subject: String,
    /// The client id OpenAI issued to this app for the account.
    pub client_id: String,
    /// Whether the grant lets the tokens spend the ChatGPT plan.
    pub plan_usage: bool,
}

/// The access token of a store directory's sign-in, renewed under the store's lock.
pub struct ChatGptLogin {
    dir: PathBuf,
    secrets: FileSecrets,
    config: SignInConfig,
    http: reqwest::Client,
    /// The token handed out last and when it expires.
    cached: Mutex<Option<(String, Option<u64>)>>,
}

impl std::fmt::Debug for ChatGptLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptLogin").field("dir", &self.dir).finish_non_exhaustive()
    }
}

impl ChatGptLogin {
    pub fn new(store_dir: impl Into<PathBuf>) -> Self {
        Self::with_config(store_dir, SignInConfig::default())
    }

    pub fn with_config(store_dir: impl Into<PathBuf>, config: SignInConfig) -> Self {
        let dir = store_dir.into();
        Self {
            secrets: FileSecrets::new(dir.join(RECORD)),
            dir,
            config,
            http: reqwest::Client::builder()
                .connect_timeout(RENEWAL_CONNECT_TIMEOUT)
                .timeout(RENEWAL_TIMEOUT)
                .build()
                .unwrap_or_default(),
            cached: Mutex::new(None),
        }
    }

    /// Whether `store_dir` holds a sign-in.
    pub fn exists(store_dir: &Path) -> bool {
        store_dir.join(RECORD).is_file()
    }

    pub fn store_dir(&self) -> &Path {
        &self.dir
    }

    /// Who the store is signed in as (`None`: not signed in).
    pub async fn account(&self) -> Option<Account> {
        let record = read(&self.secrets).await.ok()??;
        Some(Account {
            email: record.email.clone(),
            subject: record.subject.clone(),
            client_id: record.client_id.clone(),
            plan_usage: record.can_use_plan() && record.refresh_token.is_some(),
        })
    }

    /// Signs the store out: its sign-in record is removed (the host id and
    /// anything else in the directory stay). The tokens are forgotten here,
    /// not revoked at OpenAI. False when it was not signed in.
    pub async fn sign_out(&self) -> std::io::Result<bool> {
        let mut cached = self.cached.lock().await;
        *cached = None;
        if !Self::exists(&self.dir) {
            return Ok(false);
        }
        // Under the lock renewals take, so none writes the record back.
        let _held = DirLock::wait_for(&self.dir.join(LOCK), LOCK_STALE, LOCK_POLL, LOCK_WAIT)
            .await?
            .ok_or_else(|| std::io::Error::other("another process holds the sign-in lock"))?;
        match tokio::fs::remove_file(self.dir.join(RECORD)).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn shared(self) -> Arc<dyn TokenSource> {
        Arc::new(self)
    }

    async fn current(&self) -> Result<Record, ModelError> {
        let record =
            read(&self.secrets).await.map_err(|e| ModelError::Auth(format!("reading the ChatGPT sign-in: {e}")))?;
        let record = record.ok_or_else(not_signed_in)?;
        if !record.can_use_plan() {
            return Err(ModelError::Auth(
                "ChatGPT plan use is not enabled for this sign-in; sign in again with `eos-agent login chatgpt` and \
                 allow it"
                    .into(),
            ));
        }
        Ok(record)
    }

    /// Renews under the store's lock. `rejected` forces a renewal while the
    /// stored token is still the one the server refused.
    async fn renew(&self, rejected: Option<&str>) -> Result<Record, ModelError> {
        let lock = self.dir.join(LOCK);
        let _held = DirLock::wait_for(&lock, LOCK_STALE, LOCK_POLL, LOCK_WAIT)
            .await
            .map_err(|e| ModelError::Auth(format!("ChatGPT sign-in lock: {e}")))?
            .ok_or_else(|| ModelError::Transient("another process is renewing the ChatGPT sign-in".into()))?;
        let mut record = self.current().await?;
        let now = now_ms();
        let refused = rejected.is_some() && record.access_token.as_deref() == rejected;
        if record.fresh(now) && !refused {
            return Ok(record); // another process renewed it while we waited
        }
        let refresh = record.refresh_token.clone().ok_or_else(not_signed_in)?;
        let pairs = [
            ("grant_type", "refresh_token"),
            ("client_id", record.client_id.as_str()),
            ("refresh_token", refresh.as_str()),
            ("resource", self.config.resource.as_str()),
        ];
        let answer = post_form(&self.http, &self.config.token_url, &pairs)
            .await
            .map_err(|e| ModelError::Transient(format!("renewing the ChatGPT sign-in: {e}")))?;
        let body = answer.body.clone().unwrap_or_default();
        if answer.status == 200 && body["access_token"].is_string() {
            record.apply(&body);
            write(&self.secrets, &record).await?;
            return Ok(record);
        }
        let code = answer.error_code().unwrap_or_default().to_owned();
        if DEAD_REFRESH.contains(&code.as_str()) {
            // Spent for good: keep the registration (client id, host id, ID
            // token for the next sign-in's hint), drop the tokens.
            record.access_token = None;
            record.refresh_token = None;
            record.expires_at = None;
            write(&self.secrets, &record).await?;
            return Err(ModelError::Auth(format!(
                "the ChatGPT sign-in has ended ({code}); sign in again with `eos-agent login chatgpt`"
            )));
        }
        if answer.status >= 500 || answer.status == 429 {
            return Err(ModelError::Transient(format!("renewing the ChatGPT sign-in: HTTP {}", answer.status)));
        }
        Err(ModelError::Auth(format!("ChatGPT sign-in renewal refused (HTTP {}, {code})", answer.status)))
    }
}

fn not_signed_in() -> ModelError {
    ModelError::Auth("not signed in to ChatGPT; run `eos-agent login chatgpt`".into())
}

pub(crate) async fn read(secrets: &FileSecrets) -> std::io::Result<Option<Record>> {
    let Some(text) = secrets.read().await? else { return Ok(None) };
    serde_json::from_str(&text).map(Some).map_err(std::io::Error::other)
}

pub(crate) async fn write(secrets: &FileSecrets, record: &Record) -> Result<(), ModelError> {
    let text = serde_json::to_string_pretty(record).map_err(|e| ModelError::Auth(e.to_string()))?;
    secrets.write(&text).await.map_err(|e| ModelError::Auth(format!("saving the ChatGPT sign-in: {e}")))
}

impl TokenSource for ChatGptLogin {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        Box::pin(async move {
            let mut cached = self.cached.lock().await;
            let now = now_ms();
            if let Some((token, exp)) = cached.as_ref()
                && exp.is_none_or(|exp| exp.saturating_sub(now) > REFRESH_MARGIN_MS)
            {
                return Ok(Credential::Bearer(token.clone()));
            }
            let mut record = self.current().await?;
            if !record.fresh(now) {
                record = self.renew(None).await?;
            }
            let token = record.access_token.clone().ok_or_else(not_signed_in)?;
            *cached = Some((token.clone(), record.expires_at));
            Ok(Credential::Bearer(token))
        })
    }

    fn refused<'a>(&'a self, rejected: &'a Credential) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Credential::Bearer(token) = rejected else { return false };
            let mut cached = self.cached.lock().await;
            match self.renew(Some(token)).await {
                Ok(record) => {
                    let fresh = record.access_token.clone();
                    let changed = fresh.as_deref().is_some_and(|t| t != token);
                    *cached = fresh.map(|t| (t, record.expires_at));
                    changed
                }
                Err(_) => {
                    *cached = None;
                    false
                }
            }
        })
    }
}

/// This host's stable id in `dir`, made on first use.
pub(crate) async fn host_id(dir: &Path) -> std::io::Result<String> {
    let path = dir.join(HOST_ID);
    if let Ok(id) = tokio::fs::read_to_string(&path).await
        && !id.trim().is_empty()
    {
        return Ok(id.trim().to_owned());
    }
    tokio::fs::create_dir_all(dir).await?;
    let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
    tokio::fs::write(&path, &id).await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_replace_tokens_together() {
        let mut r = Record { refresh_token: Some("r0".into()), ..Record::default() };
        r.apply(&serde_json::json!({"access_token":"a1","expires_in":3600,"scope":"openid chatgpt.tokens.use.direct"}));
        assert_eq!(r.refresh_token.as_deref(), Some("r0"), "kept when the answer has none");
        assert!(r.fresh(now_ms()) && r.can_use_plan());
        r.apply(&serde_json::json!({"access_token":"a2","refresh_token":"r1","expires_in":60}));
        assert_eq!((r.access_token.as_deref(), r.refresh_token.as_deref()), (Some("a2"), Some("r1")));
        assert!(!r.fresh(now_ms()), "inside the renewal margin");
    }
}
