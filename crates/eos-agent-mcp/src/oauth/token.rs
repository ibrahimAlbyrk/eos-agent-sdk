//! Token endpoint calls with the client's authentication (as the MCP SDK
//! picks it), and how the answers are kept in Claude Code's store.

use serde_json::{Map, Value};

use super::connect::merge_signed_in;
use super::now_ms;
use super::provider::{ServerAuth, Tokens};

/// An OAuth error answer from a token or registration endpoint.
#[derive(Debug)]
pub(crate) enum EndpointError {
    OAuth { code: String, description: Option<String> },
    Http(u16),
    Network(String),
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuth { code, description: Some(d) } => write!(f, "{code}: {d}"),
            Self::OAuth { code, description: None } => f.write_str(code),
            Self::Http(status) => write!(f, "HTTP {status}"),
            Self::Network(e) => f.write_str(e),
        }
    }
}

impl ServerAuth {
    /// A token request with the client's authentication, as the SDK picks it.
    pub(super) async fn token_request(
        &self,
        auth_server: &str,
        metadata: Option<&Map<String, Value>>,
        client: &Client,
        grant: &[(&str, &str)],
        resource: Option<&str>,
    ) -> Result<Tokens, EndpointError> {
        let endpoint = match metadata.and_then(|m| m.get("token_endpoint")).and_then(Value::as_str) {
            Some(url) => url.to_owned(),
            None => reqwest::Url::parse(auth_server)
                .and_then(|u| u.join("/token"))
                .map(|u| u.to_string())
                .map_err(|e| EndpointError::Network(e.to_string()))?,
        };
        let refreshing = grant.first().is_some_and(|(_, g)| *g == "refresh_token");
        if refreshing
            && let Some(grants) = metadata.and_then(|m| m.get("grant_types_supported")).and_then(Value::as_array)
            && !grants.iter().any(|g| g == "refresh_token")
        {
            return Err(EndpointError::Network(
                "Incompatible auth server: does not support grant type refresh_token".into(),
            ));
        }
        let mut form: Vec<(&str, &str)> = grant.to_vec();
        let method = client.auth_method_for(metadata);
        let basic = match (method, &client.secret) {
            ("client_secret_basic", Some(secret)) => Some(basic_auth(&client.id, secret)),
            ("client_secret_post", Some(secret)) => {
                form.push(("client_id", &client.id));
                form.push(("client_secret", secret));
                None
            }
            _ => {
                form.push(("client_id", &client.id));
                None
            }
        };
        if let Some(resource) = resource {
            form.push(("resource", resource));
        }
        let answer = self.http.post_form(&endpoint, &form, basic.as_deref()).await.map_err(EndpointError::Network)?;
        let body = answer.body.unwrap_or(Value::Null);
        if !(200..300).contains(&answer.status) {
            return Err(match body.get("error").and_then(Value::as_str) {
                // Claude Code reads these non-standard codes as invalid_grant.
                Some("invalid_refresh_token" | "expired_refresh_token" | "token_expired") => {
                    EndpointError::OAuth { code: "invalid_grant".into(), description: None }
                }
                Some(code) => EndpointError::OAuth {
                    code: code.to_owned(),
                    description: body.get("error_description").and_then(Value::as_str).map(str::to_owned),
                },
                None if answer.status >= 500 => EndpointError::Network(format!("HTTP {}", answer.status)),
                None => EndpointError::Http(answer.status),
            });
        }
        let access = body.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty());
        let Some(access) = access else {
            return Err(EndpointError::OAuth { code: "invalid_token_response".into(), description: None });
        };
        let expires_in = body.get("expires_in").and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()));
        Ok(Tokens {
            access: access.to_owned(),
            refresh: body.get("refresh_token").and_then(Value::as_str).map(str::to_owned),
            expires_at: expires_in.map(|s| now_ms() + (s * 1000.0) as u64),
            scope: body.get("scope").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// Claude Code's `saveTokens`.
    pub(super) async fn save_tokens(&self, tokens: &Tokens, by: SavedBy<'_>) {
        let (name, url) = (self.name.clone(), self.url.clone());
        let (sign_in, served, interactive) = match by {
            SavedBy::SignIn { auth_server, found, served } => (Some((auth_server, found)), served, true),
            SavedBy::Refresh => (None, None, false),
        };
        let saved = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                e.insert("serverName".into(), name.into());
                e.insert("serverUrl".into(), url.into());
                e.insert("accessToken".into(), tokens.access.clone().into());
                if let Some(r) = &tokens.refresh {
                    e.insert("refreshToken".into(), r.clone().into());
                }
                set_or_remove(e, "expiresAt", tokens.expires_at.map(Value::from));
                set_or_remove(e, "scope", tokens.scope.clone().map(Value::from));
                let prev = e.get("discoveryState").and_then(Value::as_object).cloned();
                let state = merge_signed_in(prev.as_ref(), sign_in, served, interactive);
                set_or_remove(e, "discoveryState", state.map(Value::Object));
                true
            })
            .await;
        if saved.is_ok() {
            self.forget_tokenless_rejection();
            self.set_cached(Some(tokens.clone()));
        }
    }

    /// The tokens no longer work: keep the client and discovery state.
    pub(super) async fn invalidate_tokens(&self) {
        self.set_cached(None);
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                e.insert("accessToken".into(), "".into());
                e.remove("refreshToken");
                e.insert("expiresAt".into(), 0.into());
                true
            })
            .await;
    }

    /// The client registration no longer works either.
    pub(super) async fn invalidate_all(&self) {
        self.set_cached(None);
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                let keep = ["serverName", "serverUrl", "discoveryState", "stepUpScope"];
                e.retain(|k, _| keep.contains(&k.as_str()));
                e.insert("accessToken".into(), "".into());
                true
            })
            .await;
    }
}

/// Who saved tokens: an interactive sign-in (with the server it used,
/// whether its metadata was found and the configured metadata URL that named
/// it), or a refresh.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SavedBy<'a> {
    SignIn { auth_server: &'a str, found: bool, served: Option<&'a str> },
    Refresh,
}

/// An OAuth client: its ID, secret and how it authenticates to the token endpoint.
#[derive(Debug, Clone)]
pub(crate) struct Client {
    pub(super) id: String,
    pub(super) secret: Option<String>,
    /// From a dynamic registration's answer.
    pub(super) auth_method: Option<String>,
}

impl Client {
    /// The SDK's choice of client authentication.
    pub(super) fn auth_method_for(&self, metadata: Option<&Map<String, Value>>) -> &'static str {
        let supported: Vec<&str> = metadata
            .and_then(|m| m.get("token_endpoint_auth_methods_supported"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let has_secret = self.secret.is_some();
        // A method the registration answered with wins when the server takes it.
        for method in ["client_secret_basic", "client_secret_post", "none"] {
            if self.auth_method.as_deref() == Some(method) && (supported.is_empty() || supported.contains(&method)) {
                return method;
            }
        }
        if supported.is_empty() {
            return if has_secret { "client_secret_basic" } else { "none" };
        }
        if has_secret && supported.contains(&"client_secret_basic") {
            "client_secret_basic"
        } else if has_secret && supported.contains(&"client_secret_post") {
            "client_secret_post"
        } else if supported.contains(&"none") {
            "none"
        } else if has_secret {
            "client_secret_post"
        } else {
            "none"
        }
    }
}

pub(crate) fn basic_auth(id: &str, secret: &str) -> String {
    use base64::Engine as _;
    // encodeURIComponent
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'.'
                | b'!'
                | b'~'
                | b'*'
                | b'\''
                | b'('
                | b')' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", enc(id), enc(secret)))
}

pub(crate) fn set_or_remove(e: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(v) => {
            e.insert(key.into(), v);
        }
        None => {
            e.remove(key);
        }
    }
}

/// Space-separated scopes, each once, in order of first appearance.
pub(crate) fn union(scopes: &[Option<&str>]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for s in scopes.iter().flatten().flat_map(|s| s.split_whitespace()) {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out.join(" ")
}

pub(crate) fn random_u16() -> u16 {
    let mut b = [0u8; 2];
    let _ = getrandom::fill(&mut b);
    u16::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_merge_once() {
        assert_eq!(union(&[Some("a b"), None, Some("b c")]), "a b c");
    }

    #[test]
    fn client_auth_follows_the_sdk() {
        let public = Client { id: "c".into(), secret: None, auth_method: None };
        let secret = Client { id: "c".into(), secret: Some("s".into()), auth_method: None };
        let meta = |m: &[&str]| {
            let mut map = Map::new();
            map.insert("token_endpoint_auth_methods_supported".into(), m.iter().map(|s| Value::from(*s)).collect());
            map
        };
        assert_eq!(public.auth_method_for(None), "none");
        assert_eq!(secret.auth_method_for(None), "client_secret_basic");
        assert_eq!(
            secret.auth_method_for(Some(&meta(&["client_secret_post", "client_secret_basic"]))),
            "client_secret_basic"
        );
        assert_eq!(public.auth_method_for(Some(&meta(&["client_secret_basic"]))), "none");
        assert_eq!(basic_auth("a b", "c:d"), base64_of("a%20b:c%3Ad"));
    }

    fn base64_of(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(s)
    }
}
