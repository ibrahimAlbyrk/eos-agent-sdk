//! One server's sign-in: the token every request carries, renewed before it
//! lapses or when the server refuses it (one renewal at a time across every
//! process, under Claude Code's refresh lock), and what the server said when
//! it turned a request away.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};

use super::discovery::{self, Challenge, OAuthHttp};
use super::flow::ActiveFlow;
pub(crate) use super::token::{Client, EndpointError, SavedBy, basic_auth, random_u16, set_or_remove, union};
use eos_agent_oauth::{DirLock, lockfile_path};

use super::store::{Entry, server_key};
use super::{AuthContext, now_ms};
use crate::config::{McpServerConfig, McpTransport, OAuthConfig};

/// Renew this close to expiry (Claude Code: 300 s).
const REFRESH_MARGIN_SECS: i64 = 300;
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(60);
const REFRESH_LOCK_ATTEMPTS: u32 = 5;
const REFRESH_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub(crate) struct Tokens {
    pub(crate) access: String,
    pub(super) refresh: Option<String>,
    pub(super) expires_at: Option<u64>,
    pub(super) scope: Option<String>,
}

impl Tokens {
    fn fresh(&self, now: u64) -> bool {
        self.expires_at.is_none_or(|at| at > now + (REFRESH_MARGIN_SECS as u64) * 1000)
    }
}

/// Why the server turned a request away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// 401: no usable token (`token` is the one sent, if any).
    SignIn { token: Option<String> },
    /// 403 `insufficient_scope`: the token lacks `scope`.
    StepUp { token: Option<String>, scope: String },
}

impl Rejection {
    pub(crate) fn token(&self) -> Option<&str> {
        match self {
            Self::SignIn { token } | Self::StepUp { token, .. } => token.as_deref(),
        }
    }
}

pub(crate) struct ServerAuth {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(super) oauth: OAuthConfig,
    pub(crate) key: String,
    pub(super) ctx: Arc<AuthContext>,
    pub(super) http: OAuthHttp,
    cached: Mutex<Option<Tokens>>,
    refreshing: tokio::sync::Mutex<()>,
    rejection: Mutex<Option<Rejection>>,
    /// The authorization server's metadata, once known.
    pub(super) metadata: Mutex<Option<(String, Map<String, Value>)>>,
    /// `resource_metadata` from the server's last challenge.
    pub(super) advertised: Mutex<Option<String>>,
    pub(super) flow: Mutex<Option<Arc<ActiveFlow>>>,
    /// The config sends headers of its own (Claude Code's `wte`).
    pub(super) has_headers: bool,
}

impl std::fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerAuth").field("server", &self.name).field("key", &self.key).finish_non_exhaustive()
    }
}

pub(super) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ServerAuth {
    /// OAuth for a remote server whose config sends no `Authorization` header
    /// of its own (Claude Code's rule); `None` for the others.
    pub(crate) fn for_server(config: &McpServerConfig, ctx: &Arc<AuthContext>) -> Option<Arc<Self>> {
        let (kind, url, headers, oauth) = match &config.transport {
            McpTransport::Http { url, headers, oauth } => ("http", url, headers, oauth),
            McpTransport::Sse { url, headers, oauth } => ("sse", url, headers, oauth),
            McpTransport::Stdio { .. } => return None,
        };
        if headers.keys().any(|k| k.eq_ignore_ascii_case("authorization")) {
            return None;
        }
        Some(Arc::new(Self {
            name: config.name.clone(),
            url: url.clone(),
            oauth: oauth.clone().unwrap_or_default(),
            key: server_key(
                &config.name,
                config.key_hints.type_name.as_deref().unwrap_or(kind),
                url,
                headers,
                &config.key_hints.header_order,
            ),
            http: OAuthHttp::new(ctx.http.clone(), url, headers),
            ctx: Arc::clone(ctx),
            cached: Mutex::default(),
            refreshing: tokio::sync::Mutex::default(),
            rejection: Mutex::default(),
            metadata: Mutex::default(),
            advertised: Mutex::default(),
            flow: Mutex::default(),
            has_headers: !headers.is_empty(),
        }))
    }

    /// The access token for the next request, renewed first when it is
    /// about to lapse. `None`: not signed in (the request goes without).
    pub(crate) async fn bearer(&self) -> Option<String> {
        let now = now_ms();
        if let Some(t) = lock(&self.cached).as_ref().filter(|t| t.fresh(now)) {
            return Some(t.access.clone());
        }
        let entry = self.ctx.credentials.entry(&self.key).await.ok()?;
        self.usable(entry).await.map(|t| t.access)
    }

    /// The stored tokens, renewed when close to expiry; cached for the next request.
    async fn usable(&self, entry: Entry) -> Option<Tokens> {
        let now = now_ms();
        // XAA without a refresh token: Claude Code exchanges the cached ID
        // token again once the access token is missing or about to lapse.
        let lapsing = entry.expires_in(now).is_some_and(|s| s <= REFRESH_MARGIN_SECS);
        if self.oauth.xaa
            && self.ctx.xaa_enabled()
            && entry.refresh_token.is_none()
            && (entry.access_token.is_none() || lapsing)
        {
            let _one = self.refreshing.lock().await;
            if let Some(tokens) = self.xaa_silent().await {
                return Some(tokens);
            }
        }
        let tokens = Tokens {
            access: entry.access_token?,
            refresh: entry.refresh_token,
            expires_at: entry.expires_at,
            scope: entry.scope,
        };
        let left = entry.expires_at.map(|at| i128::from(at) - i128::from(now));
        if left.is_some_and(|ms| ms <= 0) && tokens.refresh.is_none() {
            *lock(&self.cached) = None;
            return None;
        }
        if !tokens.fresh(now)
            && tokens.refresh.is_some()
            && let Some(renewed) = self.refresh(None).await
        {
            return Some(renewed);
        }
        *lock(&self.cached) = Some(tokens.clone());
        Some(tokens)
    }

    /// The server refused `sent` (or a request without a token) with a 401.
    /// True when a token worth one retry is available now.
    pub(crate) async fn unauthorized(&self, sent: Option<&str>, header: Option<&str>) -> bool {
        let challenge = header.and_then(discovery::parse_challenge).unwrap_or_default();
        if let Some(url) = challenge.resource_metadata {
            *lock(&self.advertised) = Some(url);
        }
        let retry = match sent {
            // Signed in elsewhere (Claude Code, another process) since?
            None => match self.ctx.credentials.entry(&self.key).await {
                Ok(entry) => self.usable(entry).await.is_some(),
                Err(_) => false,
            },
            Some(token) => self.refresh(Some(token)).await.is_some_and(|t| t.access != token),
        };
        if !retry {
            self.discover_on_connect(challenge.scope.as_deref()).await;
            if sent.is_none() {
                self.note_tokenless_rejection().await;
            }
            *lock(&self.rejection) = Some(Rejection::SignIn { token: sent.map(str::to_owned) });
        }
        retry
    }

    /// A 403. With `insufficient_scope` and a scope, the scope the next
    /// sign-in asks for is remembered (`stepUpScope`: what the token had plus
    /// what the server asks for, with `offline_access` when offered), and the
    /// rejection names the scope the server asked for, as Claude Code's
    /// pending step-up does.
    pub(crate) async fn forbidden(&self, sent: Option<&str>, header: &str) {
        let Some(Challenge { error: Some(error), scope, .. }) = discovery::parse_challenge(header) else { return };
        if error != "insufficient_scope" {
            return;
        }
        let had = lock(&self.cached).as_ref().and_then(|t| t.scope.clone());
        let mut wanted = union(&[had.as_deref(), scope.as_deref()]);
        let offers_offline = lock(&self.metadata).as_ref().is_some_and(|(_, m)| {
            m.get("scopes_supported").and_then(Value::as_array).is_some_and(|a| a.iter().any(|s| s == "offline_access"))
        });
        if offers_offline && !wanted.is_empty() {
            wanted = union(&[Some(wanted.as_str()), Some("offline_access")]);
        }
        if !wanted.is_empty() {
            let scope = wanted.clone();
            let _ = self
                .ctx
                .credentials
                .mutate_entry(&self.key, |e| {
                    if e.get("accessToken").is_none() {
                        return false;
                    }
                    e.insert("stepUpScope".into(), Value::String(scope));
                    true
                })
                .await;
        }
        let asked = scope.filter(|s| !s.trim().is_empty()).unwrap_or(wanted);
        *lock(&self.rejection) = Some(Rejection::StepUp { token: sent.map(str::to_owned), scope: asked });
    }

    /// Puts Claude Code's step-up question to the host.
    pub(crate) async fn ask_step_up(&self, request: &super::StepUpRequest) -> super::StepUpAnswer {
        self.ctx.prompter.ask_step_up(request).await
    }

    /// What the server last turned away, taken once.
    pub(crate) fn take_rejection(&self) -> Option<Rejection> {
        lock(&self.rejection).take()
    }

    /// Whether the store holds a sign-in other than `rejected` (made by an
    /// authorization here, by Claude Code, or by another process).
    pub(crate) async fn has_new_sign_in(&self, rejected: Option<&str>) -> bool {
        let Ok(entry) = self.ctx.credentials.entry(&self.key).await else { return false };
        entry.access_token.is_some_and(|access| Some(access.as_str()) != rejected)
    }

    pub(super) fn set_cached(&self, tokens: Option<Tokens>) {
        *lock(&self.cached) = tokens;
    }

    /// Renews the sign-in: once per process at a time, then once across
    /// processes under Claude Code's lock, re-reading the store first (a
    /// refresh token works once, so whoever got there first wins).
    /// `rejected` is the access token the server refused, if any.
    pub(super) async fn refresh(&self, rejected: Option<&str>) -> Option<Tokens> {
        let _one = self.refreshing.lock().await;
        let now = now_ms();
        if let Some(t) = lock(&self.cached).as_ref()
            && t.fresh(now)
            && rejected.is_some_and(|r| r != t.access)
        {
            return Some(t.clone());
        }
        let target = self.ctx.credentials.refresh_lock(&self.key);
        let held = DirLock::take(&lockfile_path(&target), REFRESH_LOCK_STALE, REFRESH_LOCK_ATTEMPTS, |_| {
            Duration::from_millis(1000 + u64::from(random_u16() % 1000))
        })
        .await
        .ok()
        .flatten()?;
        let entry = self.ctx.credentials.entry(&self.key).await.ok()?;
        if let Some(access) = &entry.access_token
            && Some(access.as_str()) != rejected
            && entry.expires_in(now_ms()).is_none_or(|s| s > REFRESH_MARGIN_SECS)
        {
            let tokens = Tokens {
                access: access.clone(),
                refresh: entry.refresh_token.clone(),
                expires_at: entry.expires_at,
                scope: entry.scope.clone(),
            };
            self.set_cached(Some(tokens.clone()));
            return Some(tokens);
        }
        let refresh_token = entry.refresh_token.clone()?;
        let renewed = self.do_refresh(&entry, &refresh_token).await;
        drop(held);
        renewed
    }

    async fn do_refresh(&self, entry: &Entry, refresh_token: &str) -> Option<Tokens> {
        for attempt in 1..=REFRESH_ATTEMPTS {
            let (auth_server, metadata) = self.known_metadata(entry).await?;
            let client = self.stored_client(entry)?;
            let resource = reqwest::Url::parse(&self.url).map(|u| u.to_string()).unwrap_or_else(|_| self.url.clone());
            let result = self
                .token_request(
                    &auth_server,
                    Some(&metadata),
                    &client,
                    &[("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
                    Some(&resource),
                )
                .await;
            match result {
                Ok(mut tokens) => {
                    tokens.refresh = tokens.refresh.or_else(|| Some(refresh_token.to_owned()));
                    self.save_tokens(&tokens, SavedBy::Refresh).await;
                    return Some(tokens);
                }
                Err(EndpointError::OAuth { code, .. }) if code == "invalid_grant" => {
                    if let Some(won) = self.concurrent_winner().await {
                        return Some(won);
                    }
                    self.invalidate_tokens().await;
                    return None;
                }
                Err(EndpointError::OAuth { code, .. }) if code == "invalid_client" || code == "unauthorized_client" => {
                    if let Some(won) = self.concurrent_winner().await {
                        return Some(won);
                    }
                    let now = self.ctx.credentials.entry(&self.key).await.ok()?;
                    if now.client_id.is_some() && now.client_id.as_deref() != Some(client.id.as_str()) {
                        return None; // another process registered a new client
                    }
                    self.invalidate_all().await;
                    return None;
                }
                Err(EndpointError::Network(_)) if attempt < REFRESH_ATTEMPTS => {
                    tokio::time::sleep(Duration::from_millis(1000 << (attempt - 1))).await;
                }
                Err(_) => return None,
            }
        }
        None
    }

    /// Fresh tokens another process saved while this one's refresh failed.
    async fn concurrent_winner(&self) -> Option<Tokens> {
        let entry = self.ctx.credentials.entry(&self.key).await.ok()?;
        let fresh = entry.expires_in(now_ms()).is_none_or(|s| s > REFRESH_MARGIN_SECS);
        let tokens = Tokens {
            access: entry.access_token.filter(|_| fresh)?,
            refresh: entry.refresh_token,
            expires_at: entry.expires_at,
            scope: entry.scope,
        };
        self.set_cached(Some(tokens.clone()));
        Some(tokens)
    }

    /// The authorization server and its metadata: known already, from the
    /// stored discovery state, or discovered again.
    pub(super) async fn known_metadata(&self, entry: &Entry) -> Option<(String, Map<String, Value>)> {
        if let Some(known) = lock(&self.metadata).clone() {
            return Some(known);
        }
        let found = if let Some(url) = &self.oauth.auth_server_metadata_url {
            discovery::configured_metadata(&self.http, url).await.ok().map(|m| {
                let issuer = m.get("issuer").and_then(Value::as_str).unwrap_or(url).to_owned();
                (issuer, m)
            })
        } else if let Some(auth_server) = &entry.auth_server_url {
            discovery::auth_server_metadata(&self.http, auth_server)
                .await
                .ok()
                .flatten()
                .map(|m| (auth_server.clone(), m))
        } else {
            let advertised = entry.resource_metadata_url.clone().or_else(|| lock(&self.advertised).clone());
            let info = discovery::server_info(&self.http, &self.url, advertised.as_deref()).await.ok()?;
            info.metadata.map(|m| (info.auth_server_url, m))
        };
        if let Some(found) = &found {
            *lock(&self.metadata) = Some(found.clone());
        }
        found
    }

    /// The registered client, or the configured one.
    pub(super) fn stored_client(&self, entry: &Entry) -> Option<Client> {
        if let Some(id) = &entry.client_id {
            let secret = entry.client_secret.clone().or_else(|| {
                (self.oauth.client_id.as_deref() == Some(id.as_str()))
                    .then(|| entry.configured_secret.clone())
                    .flatten()
            });
            return Some(Client { id: id.clone(), secret, auth_method: None });
        }
        let id = self.oauth.client_id.clone()?;
        Some(Client { id, secret: entry.configured_secret.clone(), auth_method: None })
    }
}
