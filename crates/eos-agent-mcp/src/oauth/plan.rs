//! What a sign-in sends the user to: discovery, the client (stored,
//! configured, a client ID metadata document or a dynamic registration),
//! PKCE and the authorization URL with Claude Code's scope adjustments.

use eos_agent_oauth::pkce_challenge;
use reqwest::Url;
use serde_json::{Map, Value, json};

use super::discovery;
use super::provider::{Client, ServerAuth, set_or_remove, union};
use super::random_token;
use super::store::Entry;

/// Claude Code's client ID metadata document (`MCP_OAUTH_CLIENT_METADATA_URL` overrides).
const CLIENT_METADATA_URL: &str = "https://claude.ai/oauth/claude-code-client-metadata";
const MICROSOFT_HOSTS: [&str; 4] = [
    "login.microsoftonline.com",
    "login.microsoftonline.us",
    "login.partner.microsoftonline.cn",
    "login.chinacloudapi.cn",
];

impl ServerAuth {
    /// Discovery, the client, PKCE and the authorization URL.
    pub(super) async fn plan(
        &self,
        stored: &Entry,
        redirect: &str,
        state: &str,
        step_up: Option<&str>,
        advertised: Option<&str>,
    ) -> Result<Plan, String> {
        // The SDK starts from the stored discovery state when there is one
        // (configured metadata first), else discovers; the state is saved
        // with the resource-metadata URL the discovery started from.
        let mut served = None;
        let (auth_server, metadata, resource_metadata) = if let Some(url) = &self.oauth.auth_server_metadata_url {
            let m = discovery::configured_metadata(&self.http, url).await?;
            let issuer = m.get("issuer").and_then(Value::as_str).unwrap_or(url).to_owned();
            let prm = discovery::resource_metadata(&self.http, &self.url, advertised).await.ok().flatten();
            served = Some(url.clone());
            if prm.is_some() {
                self.save_discovery_state(&issuer, Some(&m), advertised, Some(url)).await;
            }
            (issuer, Some(m), prm.map(|(d, _)| d))
        } else if let Some(auth_server) = &stored.auth_server_url {
            let m = discovery::auth_server_metadata(&self.http, auth_server)
                .await
                .map_err(|e| format!("SDK auth failed: {e}"))?;
            let prm = discovery::resource_metadata(&self.http, &self.url, advertised).await.ok().flatten();
            self.save_discovery_state(auth_server, m.as_ref(), advertised, None).await;
            (auth_server.clone(), m, prm.map(|(d, _)| d))
        } else {
            let mut info = discovery::server_info(&self.http, &self.url, advertised)
                .await
                .map_err(|e| format!("SDK auth failed: {e}"))?;
            if info.metadata.is_none() && Url::parse(&self.url).is_ok_and(|u| u.path() != "/") {
                // Claude Code's last resort: path-aware discovery on the server URL itself.
                info.metadata = discovery::auth_server_metadata(&self.http, &self.url).await.ok().flatten();
            }
            self.save_discovery_state(&info.auth_server_url, info.metadata.as_ref(), advertised, None).await;
            (info.auth_server_url, info.metadata, info.resource_metadata)
        };

        let resource = match &resource_metadata {
            None => None,
            Some(prm) => {
                let named = prm.get("resource").and_then(Value::as_str).unwrap_or_default();
                if !discovery::resource_allowed(&self.url, named) {
                    return Err(format!(
                        "SDK auth failed: Protected resource {named} does not match expected {} (or origin)",
                        self.url
                    ));
                }
                Some(named.to_owned())
            }
        };
        let get = |m: &Option<Map<String, Value>>, k: &str| m.as_ref().and_then(|m| m.get(k)).cloned();
        let supported = |k: &str| -> Vec<String> {
            get(&metadata, k)
                .and_then(|v| v.as_array().cloned())
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default()
        };
        let offline = supported("scopes_supported").iter().any(|s| s == "offline_access");
        let curated = get(&metadata, "scope")
            .or_else(|| get(&metadata, "default_scope"))
            .and_then(|v| v.as_str().map(str::to_owned))
            .or_else(|| {
                self.oauth.auth_server_metadata_url.as_ref()?;
                let all = supported("scopes_supported");
                (!all.is_empty()).then(|| all.join(" "))
            });
        let prm_scopes = resource_metadata
            .as_ref()
            .and_then(|p| p.get("scopes_supported")?.as_array().cloned())
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty());
        let mut scope = step_up.map(str::to_owned).or(prm_scopes).or_else(|| curated.clone());
        if offline && let Some(s) = &scope {
            scope = Some(with_offline(s));
        }

        let client = match self.resolve_client(stored, redirect).await {
            Some(c) => c,
            None => {
                self.register(&auth_server, metadata.as_ref(), redirect, curated.as_deref(), scope.as_deref()).await?
            }
        };

        let verifier = random_token()?;
        let challenge = pkce_challenge(&verifier);
        if metadata.is_some() {
            let types = supported("response_types_supported");
            if !types.is_empty() && !types.iter().any(|t| t == "code") {
                return Err("SDK auth failed: Incompatible auth server: does not support response type code".into());
            }
            let methods = supported("code_challenge_methods_supported");
            if !methods.is_empty() && !methods.iter().any(|m| m == "S256") {
                return Err(
                    "SDK auth failed: Incompatible auth server: does not support code challenge method S256".into()
                );
            }
        }
        let endpoint = match get(&metadata, "authorization_endpoint").and_then(|v| v.as_str().map(str::to_owned)) {
            Some(e) => e,
            None => Url::parse(&auth_server).and_then(|u| u.join("/authorize")).map_err(|e| e.to_string())?.to_string(),
        };
        let mut params: Vec<(String, String)> = vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), client.id.clone()),
            ("code_challenge".into(), challenge),
            ("code_challenge_method".into(), "S256".into()),
            ("redirect_uri".into(), redirect.into()),
            ("state".into(), state.into()),
        ];
        if let Some(s) = &scope {
            params.push(("scope".into(), s.clone()));
            if s.split_whitespace().any(|x| x == "offline_access") {
                params.push(("prompt".into(), "consent".into()));
            }
        }
        if let Some(r) = &resource {
            params.push(("resource".into(), r.clone()));
        }
        // Claude Code's adjustments: configured scopes win (not during a
        // step-up), offline_access is asked for when offered, and Microsoft's
        // sign-in pages get no prompt=consent.
        let configured = if step_up.is_some() {
            None
        } else {
            self.oauth.scopes.clone().or_else(|| self.oauth.auth_server_metadata_url.as_ref().and(curated.clone()))
        };
        let in_url = scope.clone();
        if let Some(wanted) = configured.or(in_url.clone()) {
            let wanted = if offline { with_offline(&wanted) } else { wanted };
            if Some(&wanted) != in_url.as_ref() {
                params.retain(|(k, _)| k != "scope");
                params.push(("scope".into(), wanted));
            }
        }
        let mut url = Url::parse(&endpoint).map_err(|e| format!("Invalid authorization URL: {e}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Invalid authorization URL: must use http:// or https:// scheme".into());
        }
        let microsoft = url.host_str().is_some_and(|h| {
            MICROSOFT_HOSTS.contains(&h) || h.ends_with(".b2clogin.com") || h.ends_with(".ciamlogin.com")
        });
        if microsoft {
            params.retain(|(k, v)| !(k == "prompt" && v == "consent"));
        }
        url.query_pairs_mut().extend_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        Ok(Plan { url: url.to_string(), verifier, client, auth_server, metadata, resource, served })
    }

    /// Claude Code's `resolveClientInformation` for an interactive sign-in.
    async fn resolve_client(&self, stored: &Entry, redirect: &str) -> Option<Client> {
        let configured = self.oauth.client_id.clone();
        if let Some(id) = &stored.client_id {
            let same_host = match &stored.redirect_uri {
                Some(r) => scheme_host(r) == scheme_host(redirect),
                None => redirect.starts_with("http://localhost"),
            };
            if !same_host {
                match &configured {
                    None => return None,
                    Some(c) if c != id => {
                        self.patch_client(Some(c), None, redirect).await;
                        return Some(Client {
                            id: c.clone(),
                            secret: stored.configured_secret.clone(),
                            auth_method: None,
                        });
                    }
                    Some(_) => self.patch_client(None, None, redirect).await,
                }
            }
            return self.stored_client(stored);
        }
        configured.map(|id| Client { id, secret: stored.configured_secret.clone(), auth_method: None })
    }

    async fn patch_client(&self, id: Option<&str>, secret: Option<&str>, redirect: &str) {
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                if let Some(id) = id {
                    e.insert("clientId".into(), id.into());
                    set_or_remove(e, "clientSecret", secret.map(Value::from));
                }
                e.insert("redirectUri".into(), redirect.into());
                true
            })
            .await;
    }

    /// A client ID metadata document when the server takes one, else dynamic
    /// client registration with Claude Code's client metadata.
    pub(super) async fn register(
        &self,
        auth_server: &str,
        metadata: Option<&Map<String, Value>>,
        redirect: &str,
        curated: Option<&str>,
        scope: Option<&str>,
    ) -> Result<Client, String> {
        if metadata.and_then(|m| m.get("client_id_metadata_document_supported")) == Some(&Value::Bool(true)) {
            let url = std::env::var("MCP_OAUTH_CLIENT_METADATA_URL")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| CLIENT_METADATA_URL.into());
            if !url.starts_with("https://") {
                return Err(format!("SDK auth failed: client metadata URL must be https: {url}"));
            }
            self.save_client(&url, None, redirect).await;
            return Ok(Client { id: url, secret: None, auth_method: Some("none".into()) });
        }
        let endpoint = match metadata {
            Some(m) => m
                .get("registration_endpoint")
                .and_then(Value::as_str)
                .ok_or("SDK auth failed: Incompatible auth server: does not support dynamic client registration")?
                .to_owned(),
            None => Url::parse(auth_server).and_then(|u| u.join("/register")).map_err(|e| e.to_string())?.to_string(),
        };
        let mut body = json!({
            "client_name": format!("Claude Code ({})", self.name),
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "native",
        });
        if let Some(s) = scope.or(curated) {
            body["scope"] = s.into();
        }
        let answer = self.http.post_json(&endpoint, &body).await.map_err(|e| format!("SDK auth failed: {e}"))?;
        let doc = answer.body.unwrap_or(Value::Null);
        let id = doc.get("client_id").and_then(Value::as_str);
        let (true, Some(id)) = ((200..300).contains(&answer.status), id) else {
            let why = doc.get("error_description").or_else(|| doc.get("error")).and_then(Value::as_str);
            return Err(format!(
                "SDK auth failed: dynamic client registration failed: HTTP {}{}",
                answer.status,
                why.map(|w| format!(" ({w})")).unwrap_or_default()
            ));
        };
        let secret = doc.get("client_secret").and_then(Value::as_str);
        self.save_client(id, secret, redirect).await;
        Ok(Client {
            id: id.to_owned(),
            secret: secret.map(str::to_owned),
            auth_method: doc.get("token_endpoint_auth_method").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// Claude Code's `saveClientInformation`.
    async fn save_client(&self, id: &str, secret: Option<&str>, redirect: &str) {
        let (name, url) = (self.name.clone(), self.url.clone());
        let _ = self
            .ctx
            .credentials
            .mutate_entry(&self.key, |e| {
                e.insert("serverName".into(), name.into());
                e.insert("serverUrl".into(), url.into());
                e.insert("clientId".into(), id.into());
                set_or_remove(e, "clientSecret", secret.map(Value::from));
                e.insert("redirectUri".into(), redirect.into());
                if !e.get("accessToken").is_some_and(Value::is_string) {
                    e.insert("accessToken".into(), "".into());
                }
                true
            })
            .await;
    }
}

pub(super) struct Plan {
    pub(super) url: String,
    pub(super) verifier: String,
    pub(super) client: Client,
    pub(super) auth_server: String,
    pub(super) metadata: Option<Map<String, Value>>,
    pub(super) resource: Option<String>,
    /// The configured metadata URL, when it named the server.
    pub(super) served: Option<String>,
}

pub(super) fn with_offline(scope: &str) -> String {
    union(&[Some(scope), Some("offline_access")])
}

/// `scheme://host` without the port (Claude Code compares redirects so).
pub(super) fn scheme_host(url: &str) -> String {
    Url::parse(url).map_or_else(|_| url.to_owned(), |u| format!("{}://{}", u.scheme(), u.host_str().unwrap_or("")))
}
