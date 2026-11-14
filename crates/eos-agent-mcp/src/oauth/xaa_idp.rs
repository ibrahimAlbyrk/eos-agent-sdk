//! The identity-provider side of Claude Code's cross-app access (XAA): the
//! IdP named in its settings (`xaaIdp`), the user's OIDC sign-in there
//! (authorization code + PKCE, `scope=openid`, a loopback callback), and the
//! ID token it yields, cached per issuer in the credentials store
//! (`mcpXaaIdp`) until a minute before it expires.

use std::time::Duration;

use eos_agent_oauth::{form_encode, page, pkce_challenge};
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::callback::{check_issuer, pick_port};
use super::store::McpCredentials;
use super::token::{Client, basic_auth};
use super::{AuthContext, now_ms, random_token};

/// Claude Code's limit for one IdP request, and for the user's sign-in.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// A cached ID token is used until this close to its expiry.
const ID_TOKEN_MARGIN_MS: u64 = 60_000;

/// Claude Code's `xaaIdp` setting: the enterprise IdP its XAA servers trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XaaIdp {
    pub issuer: String,
    pub client_id: String,
    /// A fixed loopback callback port registered with the IdP.
    pub callback_port: Option<u16>,
}

impl XaaIdp {
    /// From a settings document's `xaaIdp` object.
    pub fn from_settings(value: &Value) -> Option<Self> {
        let o = value.as_object()?;
        let s = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
        Some(Self {
            issuer: s("issuer")?,
            client_id: s("clientId")?,
            callback_port: o.get("callbackPort").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()),
        })
    }
}

/// Claude Code's key for an issuer (`nkn`): trailing slashes of the path
/// dropped, the host in lower case.
pub(super) fn issuer_key(issuer: &str) -> String {
    match Url::parse(issuer) {
        Ok(mut u) => {
            let path = u.path().trim_end_matches('/').to_owned();
            u.set_path(&path);
            let s = u.to_string();
            // An empty path prints as "/" again; Claude Code's keys end without it.
            if path.is_empty() && u.query().is_none() { s.trim_end_matches('/').to_owned() } else { s }
        }
        Err(_) => issuer.trim_end_matches('/').to_owned(),
    }
}

/// The `exp` claim of a JWT (seconds), unverified.
pub(super) fn jwt_exp(token: &str) -> Option<u64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    eos_agent_oauth::jwt_claims(token)?.get("exp")?.as_f64().map(|s| s as u64)
}

impl McpCredentials {
    /// A cached ID token for `issuer` with more than a minute left.
    pub(super) async fn xaa_id_token(&self, issuer: &str) -> Option<String> {
        let doc = self.read().await.ok()?;
        let entry = &doc["mcpXaaIdp"][issuer_key(issuer)];
        let expires = entry["expiresAt"].as_f64()? as u64;
        (expires.saturating_sub(now_ms()) > ID_TOKEN_MARGIN_MS)
            .then(|| entry["idToken"].as_str().map(str::to_owned))
            .flatten()
    }

    pub(super) async fn save_xaa_id_token(&self, issuer: &str, token: &str, expires_at: u64) {
        let key = issuer_key(issuer);
        let _ = self
            .mutate(|root| {
                let all = root.entry("mcpXaaIdp").or_insert_with(|| Value::Object(Map::new()));
                if !all.is_object() {
                    *all = Value::Object(Map::new());
                }
                all.as_object_mut()
                    .map(|all| all.insert(key, json!({"idToken": token, "expiresAt": expires_at})))
                    .is_some()
            })
            .await;
    }

    pub(super) async fn clear_xaa_id_token(&self, issuer: &str) {
        let key = issuer_key(issuer);
        let _ = self
            .mutate(|root| {
                root.get_mut("mcpXaaIdp").and_then(Value::as_object_mut).is_some_and(|all| all.remove(&key).is_some())
            })
            .await;
    }

    /// The IdP client's secret (`mcpXaaIdpConfig[issuer].clientSecret`).
    pub(super) async fn xaa_idp_secret(&self, issuer: &str) -> Option<String> {
        let doc = self.read().await.ok()?;
        doc["mcpXaaIdpConfig"][issuer_key(issuer)]["clientSecret"].as_str().map(str::to_owned)
    }
}

/// A plain request with Claude Code's 30-second limit (no retry, no
/// configured headers: XAA's requests are its own).
pub(super) async fn send(builder: reqwest::RequestBuilder) -> Result<(u16, String), String> {
    let res = builder.timeout(REQUEST_TIMEOUT).send().await.map_err(|e| e.to_string())?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| e.to_string())?;
    Ok((status, text))
}

/// The IdP's OpenID configuration; its token endpoint must be https.
pub(super) async fn oidc_metadata(http: &reqwest::Client, issuer: &str) -> Result<Map<String, Value>, String> {
    let base = if issuer.ends_with('/') { issuer.to_owned() } else { format!("{issuer}/") };
    let url = Url::parse(&base)
        .and_then(|b| b.join(".well-known/openid-configuration"))
        .map_err(|_| format!("XAA IdP: OIDC discovery failed: issuer is not a valid URL: {issuer}"))?;
    let (status, text) = send(http.get(url.clone()).header("Accept", "application/json")).await?;
    if !(200..300).contains(&status) {
        return Err(format!("XAA IdP: OIDC discovery failed: HTTP {status} at {url}"));
    }
    let doc: Value = serde_json::from_str(&text)
        .map_err(|_| format!("XAA IdP: OIDC discovery returned non-JSON at {url} (captive portal or proxy?)"))?;
    let doc = doc.as_object().cloned().ok_or("XAA IdP: invalid OIDC metadata: not an object")?;
    for field in ["issuer", "authorization_endpoint", "token_endpoint"] {
        if !doc.get(field).is_some_and(Value::is_string) {
            return Err(format!("XAA IdP: invalid OIDC metadata: {field} — Required"));
        }
    }
    let token = doc["token_endpoint"].as_str().unwrap_or_default();
    if Url::parse(token).map(|u| u.scheme() != "https").unwrap_or(true) {
        return Err(format!("XAA IdP: refusing non-HTTPS token endpoint: {token}"));
    }
    Ok(doc)
}

/// What a sign-in at the IdP needs to show the user.
pub(super) struct Login<'a> {
    pub(super) idp: &'a XaaIdp,
    pub(super) secret: Option<&'a str>,
    /// Gets the IdP's authorization URL once the listener is up.
    pub(super) on_url: &'a (dyn Fn(String, String) + Send + Sync),
}

impl AuthContext {
    /// The user's ID token at the IdP: cached, else a browser sign-in (Claude
    /// Code's `acquireIdpIdToken`), then cached.
    pub(super) async fn xaa_id_token(&self, login: Login<'_>) -> Result<String, String> {
        let issuer = &login.idp.issuer;
        if let Some(token) = self.credentials.xaa_id_token(issuer).await {
            return Ok(token);
        }
        let metadata = oidc_metadata(&self.http, issuer).await?;
        let port = match login.idp.callback_port {
            Some(p) => p,
            None => pick_port(None)?,
        };
        let redirect = format!("http://localhost:{port}/callback");
        let state = random_token()?;
        let verifier = random_token()?;
        let url = authorization_url(&metadata, &login.idp.client_id, &redirect, &state, &verifier)?;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                format!(
                    "XAA IdP: callback port {port} is already in use. Run `lsof -ti:{port} -sTCP:LISTEN` to find the \
                     holder."
                )
            } else {
                format!("XAA IdP: callback server failed: {e}")
            }
        })?;
        (login.on_url)(url, redirect.clone());
        let (code, iss) = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(listener, &state))
            .await
            .map_err(|_| "XAA IdP: login timed out".to_owned())??;
        check_issuer(Some(&metadata), iss.as_deref())?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
        ];
        let token_endpoint = metadata["token_endpoint"].as_str().unwrap_or_default();
        let request = self.http.post(token_endpoint).header("Accept", "application/json");
        let client =
            Client { id: login.idp.client_id.clone(), secret: login.secret.map(str::to_owned), auth_method: None };
        let request = match (client.auth_method_for(Some(&metadata)), &client.secret) {
            ("client_secret_basic", Some(secret)) => {
                request.header("Authorization", format!("Basic {}", basic_auth(&client.id, secret)))
            }
            ("client_secret_post", Some(secret)) => {
                form.push(("client_id", &client.id));
                form.push(("client_secret", secret));
                request
            }
            _ => {
                form.push(("client_id", &client.id));
                request
            }
        };
        let request = request.header("Content-Type", "application/x-www-form-urlencoded").body(form_encode(&form));
        let (status, text) = send(request).await?;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) {
            let why = body.get("error").and_then(Value::as_str).unwrap_or("token request failed");
            return Err(format!("XAA IdP: {why} (HTTP {status})"));
        }
        let id_token = body
            .get("id_token")
            .and_then(Value::as_str)
            .ok_or("XAA IdP: token response missing id_token (check scope=openid)")?
            .to_owned();
        let expires_at = jwt_exp(&id_token).map_or_else(
            || now_ms() + body.get("expires_in").and_then(Value::as_u64).unwrap_or(3600) * 1000,
            |s| s * 1000,
        );
        self.credentials.save_xaa_id_token(issuer, &id_token, expires_at).await;
        Ok(id_token)
    }
}

/// The SDK's `startAuthorization` for the IdP: `scope=openid`, PKCE S256.
fn authorization_url(
    metadata: &Map<String, Value>,
    client_id: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> Result<String, String> {
    let list = |k: &str| -> Vec<String> {
        metadata
            .get(k)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let types = list("response_types_supported");
    if !types.is_empty() && !types.iter().any(|t| t == "code") {
        return Err("Incompatible auth server: does not support response type code".into());
    }
    let methods = list("code_challenge_methods_supported");
    if !methods.is_empty() && !methods.iter().any(|m| m == "S256") {
        return Err("Incompatible auth server: does not support code challenge method S256".into());
    }
    let endpoint = metadata["authorization_endpoint"].as_str().unwrap_or_default();
    let mut url = Url::parse(endpoint).map_err(|e| format!("Invalid authorization URL: {e}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("code_challenge", &pkce_challenge(verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", redirect)
        .append_pair("state", state)
        .append_pair("scope", "openid");
    Ok(url.to_string())
}

/// Claude Code's IdP callback listener: only `/callback`; an error, a wrong
/// `state` or a missing code ends the sign-in.
async fn wait_for_code(listener: TcpListener, state: &str) -> Result<(String, Option<String>), String> {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return Err("XAA IdP: callback server failed".into());
        };
        let mut head = Vec::with_capacity(1024);
        let mut buf = [0u8; 1024];
        while head.len() < 16 * 1024 && !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await {
                Ok(Ok(n)) if n > 0 => head.extend_from_slice(&buf[..n]),
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&head);
        let target = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let url = Url::parse(&format!("http://localhost{target}")).ok();
        let get = |k: &str| url.as_ref()?.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let failed = |message: &str, detail: Option<&str>| page(false, "Sign-in failed", message, detail);
        let (status, html, outcome) = if url.as_ref().is_none_or(|u| u.path() != "/callback") {
            ("404 Not Found", String::new(), None)
        } else if let Some(error) = get("error") {
            let description = get("error_description");
            let detail = format!("{error}: {}", description.clone().unwrap_or_default());
            let why = match &description {
                Some(d) => format!("XAA IdP: {error} — {d}"),
                None => format!("XAA IdP: {error}"),
            };
            ("400 Bad Request", failed("Close this tab and try again from Claude Code.", Some(&detail)), Some(Err(why)))
        } else if get("state").as_deref() != Some(state) {
            (
                "400 Bad Request",
                failed("State mismatch. Close this tab and try again.", None),
                Some(Err("XAA IdP: state mismatch (possible CSRF)".to_owned())),
            )
        } else if let Some(code) = get("code") {
            let html = page(true, "Sign-in complete", "You can close this tab and return to Claude Code.", None);
            ("200 OK", html, Some(Ok((code, get("iss").filter(|s| !s.is_empty())))))
        } else {
            (
                "400 Bad Request",
                failed("No authorization code received. Close this tab and try again.", None),
                Some(Err("XAA IdP: callback missing code".to_owned())),
            )
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
        if let Some(outcome) = outcome {
            return outcome;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuers_key_like_claude_code() {
        assert_eq!(issuer_key("https://IdP.Example.com/oauth2/default/"), "https://idp.example.com/oauth2/default");
        assert_eq!(issuer_key("https://idp.example.com/"), "https://idp.example.com");
        assert_eq!(issuer_key("not a url/"), "not a url");
        let cfg = json!({"issuer": "https://idp.dev", "clientId": "c", "callbackPort": 8765});
        assert_eq!(
            XaaIdp::from_settings(&cfg),
            Some(XaaIdp { issuer: "https://idp.dev".into(), client_id: "c".into(), callback_port: Some(8765) })
        );
        assert_eq!(XaaIdp::from_settings(&json!({"issuer": "https://idp.dev"})), None);
    }
}
