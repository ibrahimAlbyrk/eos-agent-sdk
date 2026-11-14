//! What Claude Code does when a server answers 401 before anyone signed in
//! (the MCP SDK's `auth()` with a provider that cannot redirect): discovery
//! runs and is kept in the entry's `discoveryState` (with a client registered
//! when the authorization server has no metadata); a request without a
//! token that was turned away is noted (`tokenlessRejectedAt`); and while
//! that note (or one saying the server offers no client registration) is
//! recent, a new process does not connect at start.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use reqwest::Url;
use serde_json::{Map, Value};

use super::discovery;
use super::now_ms;
use super::provider::{ServerAuth, lock, set_or_remove};

/// Claude Code skips connecting this long after a tokenless request was refused.
const TOKENLESS_WINDOW_MS: i128 = 7 * 24 * 3600 * 1000;
/// …and this long after discovery found no way to register a client.
const REGISTRATION_WINDOW_MS: i128 = 30 * 24 * 3600 * 1000;
/// A stamp up to a minute in the future still counts (clock skew).
const SKEW_MS: i128 = 60 * 1000;
/// The redirect of the provider Claude Code makes for connecting (`clt()`'s
/// default port), named by a registration made then.
const CONNECT_REDIRECT: &str = "http://localhost:3118/callback";

/// Claude Code's per-process `tokenlessRejections`, by server key.
fn tokenless() -> &'static Mutex<HashMap<String, u64>> {
    static MAP: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    MAP.get_or_init(Mutex::default)
}

fn recent(at: Option<u64>, window: i128) -> bool {
    at.is_some_and(|at| {
        let age = i128::from(now_ms()) - i128::from(at);
        age > -SKEW_MS && age <= window
    })
}

/// Whether Claude Code would keep this URL as an authorization server (`w`).
fn web_origin(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

/// `P`: a stored state that names an authorization server.
fn names_server(state: Option<&Map<String, Value>>) -> Option<&Map<String, Value>> {
    state.filter(|s| s.get("authorizationServerUrl").and_then(Value::as_str) != Some(""))
}

fn copy(from: &Map<String, Value>, to: &mut Map<String, Value>, keys: &[&str]) {
    for k in keys {
        if let Some(v) = from.get(*k) {
            to.insert((*k).into(), v.clone());
        }
    }
}

/// Claude Code's `N_r`: what a discovery run makes of the stored state.
pub(super) fn merge_discovered(
    prev: Option<&Map<String, Value>>,
    auth_server: &str,
    found: bool,
    served: Option<&str>,
) -> Map<String, Value> {
    let named = names_server(prev);
    let mut out = Map::new();
    if let Some(p) = named
        && p.contains_key("authServerMetadataUrl")
    {
        copy(p, &mut out, &["authorizationServerUrl", "oauthMetadataFound", "authServerMetadataUrl"]);
        if p.get("xaaPinned") == Some(&Value::Bool(true)) {
            out.insert("xaaPinned".into(), true.into());
        }
        return out;
    }
    let discovered = |out: &mut Map<String, Value>| {
        out.insert("authorizationServerUrl".into(), auth_server.into());
        out.insert("oauthMetadataFound".into(), found.into());
    };
    if !found || !web_origin(auth_server) {
        match named {
            Some(p) => copy(p, &mut out, &["authorizationServerUrl", "oauthMetadataFound"]),
            None => discovered(&mut out),
        }
        return out;
    }
    if let Some(p) = named
        && p.get("oauthMetadataFound") != Some(&Value::Bool(true))
        && served.is_some()
    {
        copy(p, &mut out, &["authorizationServerUrl", "oauthMetadataFound"]);
        return out;
    }
    discovered(&mut out);
    out.insert("authServerMetadataUrl".into(), served.map_or(Value::Null, Value::from));
    out
}

/// Claude Code's `F_r` then `pkn`: the stored state after tokens are saved
/// (`sign_in`: the server and whether its metadata was found).
pub(super) fn merge_signed_in(
    prev: Option<&Map<String, Value>>,
    sign_in: Option<(&str, bool)>,
    served: Option<&str>,
    interactive: bool,
) -> Option<Map<String, Value>> {
    let mut out = signed_in_state(prev, sign_in, served, interactive)?;
    out.remove("tokenlessRejectedAt");
    Some(out)
}

fn signed_in_state(
    prev: Option<&Map<String, Value>>,
    sign_in: Option<(&str, bool)>,
    served: Option<&str>,
    interactive: bool,
) -> Option<Map<String, Value>> {
    let served_value = || served.map_or(Value::Null, Value::from);
    if let Some(p) = names_server(prev)
        && p.get("xaaPinned") == Some(&Value::Bool(true))
    {
        return Some(p.clone());
    }
    if let Some((auth_server, found)) = sign_in
        && web_origin(auth_server)
        && (interactive || names_server(prev).is_none())
    {
        let mut out = prev.cloned().unwrap_or_default();
        out.remove("authServerMetadataUrl");
        out.remove("xaaPinned");
        out.insert("authorizationServerUrl".into(), auth_server.into());
        out.insert("oauthMetadataFound".into(), found.into());
        if interactive || found {
            out.insert("authServerMetadataUrl".into(), served_value());
        }
        return Some(out);
    }
    let prev = prev?;
    if !interactive
        && prev.get("oauthMetadataFound") != Some(&Value::Bool(true))
        && !prev.contains_key("authServerMetadataUrl")
    {
        let mut out = prev.clone();
        if let Some((auth_server, true)) = sign_in
            && served.is_none()
            && web_origin(auth_server)
        {
            out.insert("authorizationServerUrl".into(), auth_server.into());
            out.insert("oauthMetadataFound".into(), true.into());
            out.insert("authServerMetadataUrl".into(), Value::Null);
        }
        return Some(out);
    }
    if names_server(Some(prev)).is_none() {
        return Some(prev.clone());
    }
    let keep = if interactive {
        prev.get("authServerMetadataUrl") == Some(&served_value())
    } else {
        prev.contains_key("authServerMetadataUrl")
    };
    let mut out = prev.clone();
    if !keep {
        out.insert("authServerMetadataUrl".into(), served_value());
    }
    Some(out)
}

impl ServerAuth {
    /// After a 401 no stored token fixes: the SDK's `auth()` as Claude Code
    /// runs it while connecting — the discovery state (stored, configured or
    /// discovered now) is saved before the server is reported as needing a
    /// sign-in. A client is not registered here when the server offers
    /// registration (Claude Code defers it to the sign-in); one whose
    /// authorization server publishes no metadata gets one registered at its
    /// `/register` right away (`challenge_scope`: the 401's `scope`).
    pub(super) async fn discover_on_connect(&self, challenge_scope: Option<&str>) {
        if self.oauth.xaa {
            return;
        }
        let Ok(entry) = self.ctx.credentials.entry(&self.key).await else { return };
        let advertised = lock(&self.advertised).clone().or_else(|| entry.resource_metadata_url.clone());
        if let Some(url) = &self.oauth.auth_server_metadata_url
            && let Ok(metadata) = discovery::configured_metadata(&self.http, url).await
        {
            let issuer = metadata.get("issuer").and_then(Value::as_str).unwrap_or(url).to_owned();
            // The SDK saves only when the resource's metadata adds something.
            let resource = discovery::resource_metadata(&self.http, &self.url, advertised.as_deref()).await;
            if matches!(resource, Ok(Some(_))) {
                self.save_discovery_state(&issuer, Some(&metadata), advertised.as_deref(), Some(url)).await;
            }
            return;
        }
        let (auth_server, metadata, resource_metadata) = match &entry.auth_server_url {
            Some(auth_server) => match discovery::auth_server_metadata(&self.http, auth_server).await {
                Ok(metadata) => {
                    let prm = discovery::resource_metadata(&self.http, &self.url, advertised.as_deref()).await;
                    (auth_server.clone(), metadata, prm.ok().flatten().map(|(doc, _)| doc))
                }
                Err(_) => return,
            },
            None => match discovery::server_info(&self.http, &self.url, advertised.as_deref()).await {
                Ok(info) => (info.auth_server_url, info.metadata, info.resource_metadata),
                Err(_) => return,
            },
        };
        self.save_discovery_state(&auth_server, metadata.as_ref(), advertised.as_deref(), None).await;
        if metadata.is_none() {
            self.register_on_connect(&auth_server, challenge_scope, resource_metadata.as_ref()).await;
        }
    }

    /// The SDK's `auth()` at connect when the authorization server has no
    /// metadata and no client is stored or configured: Claude Code's provider
    /// (made for connecting, with its default `http://localhost:3118/callback`)
    /// registers at `<server>/register` and saves the client; a later sign-in
    /// keeps it while it can listen on that port. A refused registration
    /// leaves the server waiting for the sign-in, which registers again.
    async fn register_on_connect(
        &self,
        auth_server: &str,
        challenge_scope: Option<&str>,
        resource_metadata: Option<&Map<String, Value>>,
    ) {
        if self.oauth.client_id.is_some() {
            return;
        }
        let Ok(entry) = self.ctx.credentials.entry(&self.key).await else { return };
        if entry.client_id.is_some() {
            return;
        }
        // The SDK's scope: the challenge's, else the resource's (no
        // `offline_access` without authorization-server metadata).
        let prm_scopes = resource_metadata
            .and_then(|p| p.get("scopes_supported")?.as_array().cloned())
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty());
        let scope = challenge_scope.filter(|s| !s.is_empty()).map(str::to_owned).or(prm_scopes);
        let _ = self.register(auth_server, None, CONNECT_REDIRECT, None, scope.as_deref()).await;
    }

    /// Claude Code's `saveDiscoveryState`. `resource_metadata_url` is the one
    /// the discovery started from (the challenge's or the stored one);
    /// `served` the configured metadata URL when it named the server.
    pub(super) async fn save_discovery_state(
        &self,
        auth_server: &str,
        metadata: Option<&Map<String, Value>>,
        resource_metadata_url: Option<&str>,
        served: Option<&str>,
    ) {
        if self.oauth.xaa {
            return;
        }
        if let Some(m) = metadata {
            *lock(&self.metadata) = Some((auth_server.to_owned(), m.clone()));
        }
        let no_registration = metadata.map(|m| {
            !m.get("registration_endpoint").is_some_and(Value::is_string)
                && m.get("client_id_metadata_document_supported") != Some(&Value::Bool(true))
        });
        let remembered = lock(tokenless()).get(&self.key).copied().filter(|at| recent(Some(*at), TOKENLESS_WINDOW_MS));
        let (name, url, now) = (self.name.clone(), self.url.clone(), now_ms());
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                let prev = e.get("discoveryState").and_then(Value::as_object).cloned();
                let has_tokens = e.get("accessToken").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
                    || e.get("refreshToken").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
                let mut state = merge_discovered(prev.as_ref(), auth_server, metadata.is_some(), served);
                set_or_remove(&mut state, "resourceMetadataUrl", resource_metadata_url.map(Value::from));
                let previous = |k: &str| prev.as_ref().and_then(|p| p.get(k)).cloned();
                let tokenless = previous("tokenlessRejectedAt")
                    .or_else(|| (!has_tokens).then(|| remembered.map(Value::from)).flatten());
                set_or_remove(&mut state, "tokenlessRejectedAt", tokenless);
                let not_offered = match no_registration {
                    None => previous("registrationNotOfferedAt"),
                    Some(true) => Some(now.into()),
                    Some(false) => None,
                };
                set_or_remove(&mut state, "registrationNotOfferedAt", not_offered);
                e.insert("serverName".into(), name.into());
                e.insert("serverUrl".into(), url.into());
                if !e.get("accessToken").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
                    e.insert("accessToken".into(), "".into());
                }
                e.insert("discoveryState".into(), Value::Object(state));
                true
            })
            .await;
    }

    /// Claude Code's `ksr`: a request without a token was turned away with a
    /// 401. Remembered for the process, and in the stored state when that
    /// holds no tokens.
    pub(super) async fn note_tokenless_rejection(&self) {
        let now = now_ms();
        lock(tokenless()).insert(self.key.clone(), now);
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                let has = |k: &str| e.get(k).and_then(Value::as_str).is_some_and(|t| !t.is_empty());
                if has("refreshToken") || has("accessToken") {
                    return false;
                }
                let Some(state) = e.get_mut("discoveryState").and_then(Value::as_object_mut) else { return false };
                let at = state.get("tokenlessRejectedAt").and_then(Value::as_f64).map(|ms| ms as u64);
                if recent(at, TOKENLESS_WINDOW_MS) {
                    return false;
                }
                state.insert("tokenlessRejectedAt".into(), now.into());
                true
            })
            .await;
    }

    /// A connection worked: the process forgets the refusal (Claude Code keeps
    /// the stored note until a sign-in saves tokens).
    pub(super) fn forget_tokenless_rejection(&self) {
        lock(tokenless()).remove(&self.key);
    }

    /// Claude Code's `bsr`: at start, a server whose stored state says it
    /// needs a sign-in is not connected (its auth tools stand in).
    pub(crate) async fn skips_connect(&self) -> bool {
        if self.has_headers || (self.oauth.xaa && self.ctx.xaa_enabled()) {
            return false;
        }
        let Ok(entry) = self.ctx.credentials.entry(&self.key).await else { return false };
        if !entry.discovered_without_tokens() {
            return false;
        }
        if recent(entry.tokenless_rejected_at, TOKENLESS_WINDOW_MS) {
            if let Some(at) = entry.tokenless_rejected_at {
                lock(tokenless()).insert(self.key.clone(), at);
            }
            return true;
        }
        self.oauth.client_id.is_none()
            && self.oauth.auth_server_metadata_url.is_none()
            && !self.oauth.xaa
            && entry.client_id.is_none()
            && recent(entry.registration_not_offered_at, REGISTRATION_WINDOW_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().expect("object")
    }

    #[test]
    fn discovery_merges_like_claude_code() {
        let found = merge_discovered(None, "https://as.dev", true, None);
        assert_eq!(
            Value::Object(found),
            json!({"authorizationServerUrl": "https://as.dev", "oauthMetadataFound": true, "authServerMetadataUrl": null})
        );
        let legacy = merge_discovered(None, "https://x.dev/", false, None);
        assert_eq!(
            Value::Object(legacy),
            json!({"authorizationServerUrl": "https://x.dev/", "oauthMetadataFound": false})
        );
        let pinned = map(json!({"authorizationServerUrl": "https://old.dev", "oauthMetadataFound": true,
            "authServerMetadataUrl": null, "resourceMetadataUrl": "https://x.dev/prm"}));
        assert_eq!(
            Value::Object(merge_discovered(Some(&pinned), "https://new.dev", true, None)),
            json!({"authorizationServerUrl": "https://old.dev", "oauthMetadataFound": true, "authServerMetadataUrl": null})
        );
    }

    #[test]
    fn saved_tokens_shape_the_state_like_claude_code() {
        let prev = map(json!({"authorizationServerUrl": "https://as.dev", "oauthMetadataFound": true,
            "resourceMetadataUrl": "https://x.dev/prm", "tokenlessRejectedAt": 5}));
        let signed = merge_signed_in(Some(&prev), Some(("https://as.dev", true)), None, true).expect("state");
        assert_eq!(
            Value::Object(signed),
            json!({"authorizationServerUrl": "https://as.dev", "oauthMetadataFound": true,
                "resourceMetadataUrl": "https://x.dev/prm", "authServerMetadataUrl": null})
        );
        let refreshed = merge_signed_in(Some(&prev), None, None, false).expect("state");
        assert_eq!(refreshed["authServerMetadataUrl"], Value::Null);
        assert!(!refreshed.contains_key("tokenlessRejectedAt"));
        assert!(merge_signed_in(None, None, None, false).is_none());
    }

    #[test]
    fn stamps_count_within_their_window() {
        assert!(recent(Some(now_ms()), TOKENLESS_WINDOW_MS));
        assert!(recent(Some(now_ms() + 30_000), TOKENLESS_WINDOW_MS));
        assert!(!recent(Some(now_ms() + 120_000), TOKENLESS_WINDOW_MS));
        assert!(!recent(Some(now_ms() - 8 * 24 * 3600 * 1000), TOKENLESS_WINDOW_MS));
        assert!(!recent(None, TOKENLESS_WINDOW_MS));
    }
}
