//! Claude Code's cross-app access (XAA, `oauth.xaa`, behind
//! `CLAUDE_CODE_ENABLE_XAA`): no consent screen of the MCP server's own. The
//! user's ID token from the enterprise IdP is exchanged at the IdP for an
//! ID-JAG (RFC 8693 token exchange, `audience` = the authorization server,
//! `resource` = the MCP server), which the authorization server takes as a
//! JWT bearer grant (RFC 7523) from the configured client. The authorization
//! server a sign-in used is pinned (`xaaPinned`); while the cached ID token
//! lasts, expiring tokens are renewed the same way without the user.

use reqwest::Url;
use serde_json::{Map, Value};

use super::discovery;
use super::provider::{ServerAuth, Tokens, basic_auth, set_or_remove};
use super::xaa_idp::{Login, oidc_metadata, send};
use super::{AuthRequest, XaaIdp, now_ms};

const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const ID_JAG: &str = "urn:ietf:params:oauth:token-type:id-jag";
const ID_TOKEN: &str = "urn:ietf:params:oauth:token-type:id_token";

/// A failed exchange; `clear_id_token` when the IdP turned the ID token down.
#[derive(Debug)]
pub(super) struct XaaError {
    pub(super) message: String,
    pub(super) clear_id_token: bool,
}

impl From<String> for XaaError {
    fn from(message: String) -> Self {
        Self { message, clear_id_token: false }
    }
}

/// Claude Code's comparison of URLs (`te`): the href without a trailing slash.
fn same_url(a: &str, b: &str) -> bool {
    let norm =
        |s: &str| Url::parse(s).map_or_else(|_| s.to_owned(), |u| u.to_string()).trim_end_matches('/').to_owned();
    norm(a) == norm(b)
}

/// Token values never reach an error message.
fn redact(text: &str) -> String {
    let mut out = text.to_owned();
    for key in ["access_token", "refresh_token", "id_token", "assertion", "subject_token", "client_secret"] {
        let needle = format!("\"{key}\"");
        let mut from = 0;
        while let Some(at) = out[from..].find(&needle).map(|i| i + from) {
            let rest = &out[at + needle.len()..];
            let colon = rest.len() - rest.trim_start().len();
            let Some(value) = rest.trim_start().strip_prefix(':').map(str::trim_start) else {
                from = at + needle.len();
                continue;
            };
            let Some(body) = value.strip_prefix('"') else {
                from = at + needle.len();
                continue;
            };
            let Some(end) = body.find('"') else { break };
            let start = at + needle.len() + colon;
            let stop = out.len() - body.len() + end + 1;
            let replacement = ":\"[REDACTED]\"".to_owned();
            out.replace_range(start..stop, &replacement);
            from = start + replacement.len();
        }
    }
    out
}

fn first_200(text: &str) -> String {
    text.chars().take(200).collect()
}

/// What the exchange needs.
pub(super) struct Exchange<'a> {
    pub(super) client_id: &'a str,
    pub(super) client_secret: &'a str,
    pub(super) idp_client_id: &'a str,
    pub(super) idp_client_secret: Option<&'a str>,
    pub(super) id_token: &'a str,
    pub(super) idp_token_endpoint: &'a str,
    pub(super) pinned: Option<&'a str>,
}

/// Tokens from the authorization server, and which one issued them.
pub(super) struct Issued {
    pub(super) tokens: Tokens,
    pub(super) issuer: String,
}

impl ServerAuth {
    fn xaa_http(&self) -> &reqwest::Client {
        &self.ctx.http
    }

    /// Claude Code's `performCrossAppAccess`.
    pub(super) async fn xaa_exchange(&self, x: &Exchange<'_>) -> Result<Issued, XaaError> {
        let http = discovery::OAuthHttp::new(self.xaa_http().clone(), "", &Default::default());
        let prm = match discovery::resource_metadata(&http, &self.url, None).await {
            Ok(Some((doc, _))) => doc,
            Ok(None) => return Err(format!("XAA: PRM discovery failed for {} (Error)", self.url).into()),
            Err(e) => {
                let status = e.split("HTTP ").nth(1).and_then(|s| s.get(..3)).filter(|s| s.parse::<u16>().is_ok());
                let why = status.map_or_else(|| "Error".to_owned(), |s| format!("HTTP {s}"));
                return Err(format!("XAA: PRM discovery failed for {} ({why})", self.url).into());
            }
        };
        let resource = prm.get("resource").and_then(Value::as_str).unwrap_or_default().to_owned();
        let servers: Vec<String> = prm
            .get("authorization_servers")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();
        if resource.is_empty() || servers.is_empty() {
            return Err("XAA: PRM discovery failed: PRM missing resource or authorization_servers".to_owned().into());
        }
        if !same_url(&resource, &self.url) {
            return Err(format!(
                "XAA: PRM discovery failed: PRM resource mismatch: expected {}, got {resource}",
                self.url
            )
            .into());
        }
        let candidates: Vec<&String> = match x.pinned {
            None => servers.iter().collect(),
            Some(pin) => servers.iter().filter(|s| same_url(s, pin)).collect(),
        };
        if let (Some(pin), true) = (x.pinned, candidates.is_empty()) {
            return Err(format!(
                "XAA: authorization server changed: signed in with {pin}, server now names [{}]. If this move is \
                 expected, run `claude mcp remove` for this server and add it again with its client secret.",
                servers.join(", ")
            )
            .into());
        }
        let mut tried = Vec::new();
        let mut chosen = None;
        for server in &candidates {
            match as_metadata(&http, server).await {
                Err(e) => tried.push(format!("{server}: {e}")),
                Ok(m) => {
                    let grants: Option<Vec<&str>> = m
                        .get("grant_types_supported")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).collect());
                    if let Some(grants) = grants.filter(|g| !g.contains(&JWT_BEARER)) {
                        tried.push(format!(
                            "{server}: does not advertise jwt-bearer grant (supported: {})",
                            grants.join(", ")
                        ));
                        continue;
                    }
                    chosen = Some(m);
                    break;
                }
            }
        }
        let Some(metadata) = chosen else {
            return Err(format!("XAA: no authorization server supports jwt-bearer. Tried: {}", tried.join("; ")).into());
        };
        let issuer = metadata.get("issuer").and_then(Value::as_str).unwrap_or_default().to_owned();
        let jag = self.id_jag(x, &issuer, &resource).await?;
        let tokens = self.jwt_bearer(x, &metadata, &jag).await?;
        Ok(Issued { tokens, issuer })
    }

    /// RFC 8693 at the IdP: the ID token for an ID-JAG.
    async fn id_jag(&self, x: &Exchange<'_>, audience: &str, resource: &str) -> Result<String, XaaError> {
        let mut form = vec![
            ("grant_type", TOKEN_EXCHANGE),
            ("requested_token_type", ID_JAG),
            ("audience", audience),
            ("resource", resource),
            ("subject_token", x.id_token),
            ("subject_token_type", ID_TOKEN),
            ("client_id", x.idp_client_id),
        ];
        if let Some(secret) = x.idp_client_secret {
            form.push(("client_secret", secret));
        }
        let request = self
            .xaa_http()
            .post(x.idp_token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(eos_agent_oauth::form_encode(&form));
        let (status, text) = send(request).await?;
        let fail = |message: String, clear: bool| XaaError { message, clear_id_token: clear };
        if !(200..300).contains(&status) {
            let body = first_200(&redact(&text));
            return Err(fail(format!("XAA: token exchange failed: HTTP {status}: {body}"), status < 500));
        }
        let body: Value = serde_json::from_str(&text).map_err(|_| {
            fail(format!("XAA: token exchange returned non-JSON (captive portal?) at {}", x.idp_token_endpoint), false)
        })?;
        let shown = || redact(&body.to_string());
        let shape_ok = body.is_object()
            && ["access_token", "issued_token_type", "scope"].iter().all(|k| body.get(*k).is_none_or(Value::is_string))
            && body.get("expires_in").is_none_or(|v| v.is_number());
        if !shape_ok {
            return Err(fail(format!("XAA: token exchange response did not match expected shape: {}", shown()), true));
        }
        let Some(jag) = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return Err(fail(format!("XAA: token exchange response missing access_token: {}", shown()), true));
        };
        let issued = body.get("issued_token_type").and_then(Value::as_str);
        if issued != Some(ID_JAG) {
            let shown = issued.map_or_else(|| "undefined".to_owned(), str::to_owned);
            return Err(fail(format!("XAA: token exchange returned unexpected issued_token_type: {shown}"), true));
        }
        Ok(jag.to_owned())
    }

    /// RFC 7523 at the authorization server: the ID-JAG for tokens.
    async fn jwt_bearer(&self, x: &Exchange<'_>, metadata: &Map<String, Value>, jag: &str) -> Result<Tokens, XaaError> {
        let endpoint = metadata.get("token_endpoint").and_then(Value::as_str).unwrap_or_default();
        let methods: Option<Vec<&str>> = metadata
            .get("token_endpoint_auth_methods_supported")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect());
        let post = methods.is_some_and(|m| !m.contains(&"client_secret_basic") && m.contains(&"client_secret_post"));
        let mut form = vec![("grant_type", JWT_BEARER), ("assertion", jag)];
        let mut request = self.xaa_http().post(endpoint).header("Content-Type", "application/x-www-form-urlencoded");
        if post {
            form.push(("client_id", x.client_id));
            form.push(("client_secret", x.client_secret));
        } else {
            request = request.header("Authorization", format!("Basic {}", basic_auth(x.client_id, x.client_secret)));
        }
        let (status, text) = send(request.body(eos_agent_oauth::form_encode(&form))).await?;
        if !(200..300).contains(&status) {
            return Err(format!("XAA: jwt-bearer grant failed: HTTP {status}: {}", first_200(&redact(&text))).into());
        }
        let body: Value = serde_json::from_str(&text)
            .map_err(|_| format!("XAA: jwt-bearer grant returned non-JSON (captive portal?) at {endpoint}"))?;
        let access = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty());
        let strings_ok =
            ["token_type", "scope", "refresh_token"].iter().all(|k| body.get(*k).is_none_or(Value::is_string));
        let (Some(access), true) = (access, strings_ok && body.get("expires_in").is_none_or(Value::is_number)) else {
            return Err(format!(
                "XAA: jwt-bearer response did not match expected shape: {}",
                redact(&body.to_string())
            )
            .into());
        };
        Ok(Tokens {
            access: access.to_owned(),
            refresh: body.get("refresh_token").and_then(Value::as_str).map(str::to_owned),
            expires_at: body.get("expires_in").and_then(Value::as_f64).map(|s| now_ms() + (s * 1000.0) as u64),
            scope: body.get("scope").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// The configured IdP, client and secrets an XAA sign-in needs.
    async fn xaa_parts(&self) -> Result<(XaaIdp, String, String), String> {
        let idp = self.ctx.xaa_idp.clone().ok_or(
            "XAA: no IdP connection configured. Run 'claude mcp xaa setup --issuer <url> --client-id <id> \
             --client-secret' to configure.",
        )?;
        let client_id =
            self.oauth.client_id.clone().ok_or_else(|| {
                format!("XAA: server '{}' needs an AS client_id. Re-add with --client-id.", self.name)
            })?;
        let entry = self.ctx.credentials.entry(&self.key).await?;
        let secret = entry.configured_secret.ok_or_else(|| {
            format!("XAA: AS client secret not found for '{}'. Re-add with --client-secret.", self.name)
        })?;
        Ok((idp, client_id, secret))
    }

    /// Claude Code's XAA sign-in: the IdP's ID token (cached, else the user
    /// signs in at the IdP; `on_url` gets its address), exchanged for tokens.
    pub(super) async fn xaa_sign_in(
        &self,
        agent_id: Option<String>,
        on_url: &(dyn Fn(String) + Send + Sync),
    ) -> Result<(), String> {
        let (idp, client_id, secret) = self.xaa_parts().await?;
        let idp_secret = self.ctx.credentials.xaa_idp_secret(&idp.issuer).await;
        let (prompter, name) = (std::sync::Arc::clone(&self.ctx.prompter), self.name.clone());
        let show = move |url: String, redirect: String| {
            on_url(url.clone());
            prompter.authorize(&AuthRequest {
                server: name.clone(),
                url,
                redirect_uri: redirect,
                agent_id: agent_id.clone(),
                step_up: false,
            });
        };
        let login = Login { idp: &idp, secret: idp_secret.as_deref(), on_url: &show };
        let id_token = self.ctx.xaa_id_token(login).await?;
        self.xaa_finish(&idp, &client_id, &secret, idp_secret.as_deref(), &id_token, true).await.map(drop)
    }

    /// Claude Code's `xaaRefresh`: new tokens from the cached ID token, without
    /// the user. `None` when there is no cached token or the exchange failed.
    pub(super) async fn xaa_silent(&self) -> Option<Tokens> {
        let idp = self.ctx.xaa_idp.clone()?;
        let id_token = self.ctx.credentials.xaa_id_token(&idp.issuer).await?;
        let client_id = self.oauth.client_id.clone()?;
        let secret = self.ctx.credentials.entry(&self.key).await.ok()?.configured_secret?;
        let idp_secret = self.ctx.credentials.xaa_idp_secret(&idp.issuer).await;
        self.xaa_finish(&idp, &client_id, &secret, idp_secret.as_deref(), &id_token, false).await.ok()
    }

    async fn xaa_finish(
        &self,
        idp: &XaaIdp,
        client_id: &str,
        secret: &str,
        idp_secret: Option<&str>,
        id_token: &str,
        interactive: bool,
    ) -> Result<Tokens, String> {
        let idp_metadata = oidc_metadata(&self.ctx.http, &idp.issuer).await?;
        let pinned = self.ctx.credentials.entry(&self.key).await?.xaa_pin().map(str::to_owned);
        let exchange = Exchange {
            client_id,
            client_secret: secret,
            idp_client_id: &idp.client_id,
            idp_client_secret: idp_secret,
            id_token,
            idp_token_endpoint: idp_metadata["token_endpoint"].as_str().unwrap_or_default(),
            pinned: pinned.as_deref(),
        };
        let issued = match self.xaa_exchange(&exchange).await {
            Ok(issued) => issued,
            Err(e) => {
                if e.clear_id_token {
                    self.ctx.credentials.clear_xaa_id_token(&idp.issuer).await;
                }
                return Err(e.message);
            }
        };
        self.save_xaa_tokens(&issued, client_id, secret, interactive).await;
        Ok(issued.tokens)
    }

    /// Saves as Claude Code does after an XAA exchange: the tokens, the
    /// client and its secret, and the pinned authorization server (`TJt`).
    async fn save_xaa_tokens(&self, issued: &Issued, client_id: &str, secret: &str, interactive: bool) {
        let (name, url, tokens) = (self.name.clone(), self.url.clone(), &issued.tokens);
        let saved = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                let prev = e.get("discoveryState").and_then(Value::as_object).cloned();
                let pinned = prev
                    .as_ref()
                    .filter(|p| p.get("xaaPinned") == Some(&Value::Bool(true)))
                    .and_then(|p| p.get("authorizationServerUrl").and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let metadata_url = prev
                    .as_ref()
                    .and_then(|p| p.get("authServerMetadataUrl").cloned())
                    .filter(|_| !interactive)
                    .unwrap_or(Value::Null);
                let mut state = Map::new();
                state.insert("authorizationServerUrl".into(), pinned.unwrap_or_else(|| issued.issuer.clone()).into());
                state.insert("oauthMetadataFound".into(), true.into());
                state.insert("authServerMetadataUrl".into(), metadata_url);
                state.insert("xaaPinned".into(), true.into());
                e.insert("serverName".into(), name.into());
                e.insert("serverUrl".into(), url.into());
                e.insert("accessToken".into(), tokens.access.clone().into());
                if let Some(r) = &tokens.refresh {
                    e.insert("refreshToken".into(), r.clone().into());
                }
                set_or_remove(e, "expiresAt", tokens.expires_at.map(Value::from));
                set_or_remove(e, "scope", tokens.scope.clone().map(Value::from));
                e.insert("clientId".into(), client_id.into());
                e.insert("clientSecret".into(), secret.into());
                e.insert("discoveryState".into(), Value::Object(state));
                true
            })
            .await;
        if saved.is_ok() {
            self.set_cached(Some(tokens.clone()));
        }
    }
}

/// The authorization server's metadata for XAA: issuer echoed, https token endpoint.
async fn as_metadata(http: &discovery::OAuthHttp, server: &str) -> Result<Map<String, Value>, String> {
    let found = discovery::auth_server_metadata(http, server).await.map_err(|e| {
        let status = e.split("HTTP ").nth(1).and_then(|s| s.get(..3)).filter(|s| s.parse::<u16>().is_ok());
        format!(
            "XAA: AS metadata discovery failed ({})",
            status.map_or_else(|| "Error".to_owned(), |s| format!("HTTP {s}"))
        )
    })?;
    let m = found
        .filter(|m| {
            m.get("issuer").is_some_and(Value::is_string) && m.get("token_endpoint").is_some_and(Value::is_string)
        })
        .ok_or_else(|| format!("XAA: AS metadata discovery failed: no valid metadata at {server}"))?;
    let issuer = m["issuer"].as_str().unwrap_or_default();
    if !same_url(issuer, server) {
        return Err(format!("XAA: AS metadata discovery failed: issuer mismatch: expected {server}, got {issuer}"));
    }
    let token = m["token_endpoint"].as_str().unwrap_or_default();
    if Url::parse(token).map(|u| u.scheme() != "https").unwrap_or(true) {
        return Err(format!("XAA: refusing non-HTTPS token endpoint: {token}"));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_redacted_from_errors() {
        let text = r#"{"error":"invalid_grant","subject_token": "eyJ.secret","access_token":"abc"}"#;
        assert_eq!(
            redact(text),
            r#"{"error":"invalid_grant","subject_token":"[REDACTED]","access_token":"[REDACTED]"}"#
        );
        assert!(same_url("https://x.dev/mcp/", "https://x.dev/mcp"));
        assert!(!same_url("https://x.dev/a", "https://x.dev/b"));
    }
}
