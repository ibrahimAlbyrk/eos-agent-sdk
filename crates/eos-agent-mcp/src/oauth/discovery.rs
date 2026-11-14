//! Finding a server's authorization server the way the MCP SDK inside Claude
//! Code does: the `WWW-Authenticate` challenge, protected-resource metadata
//! (RFC 9728), then authorization-server metadata (RFC 8414 / OpenID).

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Url;
use serde_json::{Map, Value};

/// The MCP protocol version the SDK announces on discovery requests.
const PROTOCOL_VERSION: &str = "2025-11-25";
/// Claude Code's limit for one OAuth request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A `Bearer` challenge's parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Challenge {
    pub(crate) resource_metadata: Option<String>,
    pub(crate) scope: Option<String>,
    pub(crate) error: Option<String>,
}

/// Parses `WWW-Authenticate: Bearer k="v", k2=v2`; `None` for other schemes.
pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let (scheme, params) = header.split_once(char::is_whitespace).unwrap_or((header, ""));
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut out = Challenge::default();
    let mut rest = params;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim_matches(|c: char| c == ',' || c.is_whitespace()).to_owned();
        let after = rest[eq + 1..].trim_start();
        let (value, tail) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            (quoted[..end].to_owned(), quoted.get(end + 1..).unwrap_or(""))
        } else {
            let end = after.find(|c: char| c == ',' || c.is_whitespace()).unwrap_or(after.len());
            (after[..end].to_owned(), &after[end..])
        };
        match name.as_str() {
            "resource_metadata" => out.resource_metadata = Some(value),
            "scope" => out.scope = Some(value),
            "error" => out.error = Some(value),
            _ => {}
        }
        rest = tail;
    }
    Some(out)
}

/// OAuth requests for one server: Claude Code's timeout and one retry on a
/// network failure, and the server's configured headers on requests to the
/// server's own origin.
#[derive(Debug, Clone)]
pub(crate) struct OAuthHttp {
    pub(crate) client: reqwest::Client,
    origin: Option<String>,
    headers: BTreeMap<String, String>,
}

pub(crate) struct Answer {
    pub(crate) status: u16,
    pub(crate) body: Option<Value>,
}

impl OAuthHttp {
    pub(crate) fn new(client: reqwest::Client, server_url: &str, headers: &BTreeMap<String, String>) -> Self {
        Self {
            client,
            origin: Url::parse(server_url).ok().map(|u| u.origin().ascii_serialization()),
            headers: headers.clone(),
        }
    }

    fn same_origin(&self, url: &str) -> bool {
        let theirs = Url::parse(url).ok().map(|u| u.origin().ascii_serialization());
        self.origin.is_some() && theirs == self.origin
    }

    async fn send(&self, build: impl Fn() -> reqwest::RequestBuilder, url: &str) -> Result<Answer, String> {
        let attempt = || async {
            let mut req = build().timeout(REQUEST_TIMEOUT);
            if self.same_origin(url) {
                for (k, v) in &self.headers {
                    req = req.header(k, v);
                }
            }
            req.send().await
        };
        let res = match attempt().await {
            Ok(res) => res,
            Err(e) if e.is_timeout() || e.is_connect() || e.is_request() => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                attempt().await.map_err(|e| format!("{url}: {e}"))?
            }
            Err(e) => return Err(format!("{url}: {e}")),
        };
        let status = res.status().as_u16();
        let text = res.text().await.map_err(|e| format!("{url}: {e}"))?;
        Ok(Answer { status, body: serde_json::from_str(&text).ok() })
    }

    pub(crate) async fn get(&self, url: &str, accept_json: bool) -> Result<Answer, String> {
        self.send(
            || {
                let req = self.client.get(url).header("MCP-Protocol-Version", PROTOCOL_VERSION);
                if accept_json { req.header("Accept", "application/json") } else { req }
            },
            url,
        )
        .await
    }

    pub(crate) async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
        basic: Option<&str>,
    ) -> Result<Answer, String> {
        let authorization = basic.map(|b| format!("Basic {b}"));
        self.post_form_as(url, form, authorization.as_deref()).await
    }

    /// A form POST with this `Authorization` header value, if any.
    pub(crate) async fn post_form_as(
        &self,
        url: &str,
        form: &[(&str, &str)],
        authorization: Option<&str>,
    ) -> Result<Answer, String> {
        let body = form_encode(form);
        self.send(
            || {
                let req = self
                    .client
                    .post(url)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .header("Accept", "application/json")
                    .body(body.clone());
                match authorization {
                    Some(value) => req.header("Authorization", value),
                    None => req,
                }
            },
            url,
        )
        .await
    }

    pub(crate) async fn post_json(&self, url: &str, body: &Value) -> Result<Answer, String> {
        let body = body.to_string();
        self.send(|| self.client.post(url).header("Content-Type", "application/json").body(body.clone()), url).await
    }
}

pub(crate) use eos_agent_oauth::form_encode;

/// What discovery found.
#[derive(Debug, Clone, Default)]
pub(crate) struct ServerInfo {
    pub(crate) auth_server_url: String,
    /// Authorization-server metadata (`None`: none published; the legacy
    /// `/authorize`, `/token`, `/register` paths apply).
    pub(crate) metadata: Option<Map<String, Value>>,
    pub(crate) resource_metadata: Option<Map<String, Value>>,
}

/// `/.well-known/<name>` placed before `url`'s path (trailing `/` dropped).
fn well_known(url: &Url, name: &str, with_path: bool) -> String {
    let path = url.path().trim_end_matches('/');
    let mut out = url.clone();
    out.set_fragment(None);
    if with_path {
        out.set_path(&format!("/.well-known/{name}{path}"));
    } else {
        out.set_path(&format!("/.well-known/{name}"));
        out.set_query(None);
    }
    out.to_string()
}

fn retry_next(status: u16) -> bool {
    (400..500).contains(&status) || status == 502
}

/// Protected-resource metadata: the advertised URL only, else the
/// path-aware well-known URL, then the root one.
pub(crate) async fn resource_metadata(
    http: &OAuthHttp,
    server_url: &str,
    advertised: Option<&str>,
) -> Result<Option<(Map<String, Value>, String)>, String> {
    let url = Url::parse(server_url).map_err(|e| format!("invalid server URL: {e}"))?;
    let candidates: Vec<String> = match advertised {
        Some(a) => vec![a.to_owned()],
        None if url.path() == "/" || url.path().is_empty() => vec![well_known(&url, "oauth-protected-resource", false)],
        None => vec![
            well_known(&url, "oauth-protected-resource", true),
            well_known(&url, "oauth-protected-resource", false),
        ],
    };
    for candidate in candidates {
        let answer = http.get(&candidate, false).await?;
        if (200..300).contains(&answer.status) {
            let doc = answer.body.and_then(|b| b.as_object().cloned()).ok_or("invalid resource metadata")?;
            if !doc.get("resource").is_some_and(Value::is_string) {
                return Err("resource metadata has no \"resource\"".into());
            }
            return Ok(Some((doc, candidate)));
        }
        if !retry_next(answer.status) {
            return Err(format!("HTTP {} trying to load resource metadata", answer.status));
        }
    }
    Ok(None)
}

/// Authorization-server metadata from the RFC 8414 and OpenID well-known
/// URLs, in the SDK's order.
pub(crate) async fn auth_server_metadata(
    http: &OAuthHttp,
    auth_server: &str,
) -> Result<Option<Map<String, Value>>, String> {
    let url = Url::parse(auth_server).map_err(|e| format!("invalid authorization server URL: {e}"))?;
    let has_path = !(url.path() == "/" || url.path().is_empty());
    let candidates = if has_path {
        let mut own = url.clone();
        own.set_path(&format!("{}/.well-known/openid-configuration", url.path().trim_end_matches('/')));
        vec![
            well_known(&url, "oauth-authorization-server", true),
            well_known(&url, "openid-configuration", true),
            own.to_string(),
        ]
    } else {
        vec![well_known(&url, "oauth-authorization-server", false), well_known(&url, "openid-configuration", false)]
    };
    for candidate in candidates {
        let answer = http.get(&candidate, true).await?;
        if (200..300).contains(&answer.status) {
            return validate_metadata(answer.body).map(Some);
        }
        if !retry_next(answer.status) {
            return Err(format!("HTTP {} trying to load OAuth metadata from {candidate}", answer.status));
        }
    }
    Ok(None)
}

/// A metadata document from `oauth.authServerMetadataUrl`.
pub(crate) async fn configured_metadata(http: &OAuthHttp, url: &str) -> Result<Map<String, Value>, String> {
    if !url.starts_with("https://") {
        return Err(format!("authServerMetadataUrl must use https:// (got: {url})"));
    }
    let answer = http.get(url, true).await?;
    if !(200..300).contains(&answer.status) {
        return Err(format!("HTTP {} fetching configured auth server metadata from {url}", answer.status));
    }
    if answer.body.is_none() {
        return Err(format!("Configured auth server metadata at {url} is not valid JSON"));
    }
    validate_metadata(answer.body)
}

fn validate_metadata(body: Option<Value>) -> Result<Map<String, Value>, String> {
    let doc = body.and_then(|b| b.as_object().cloned()).ok_or("Server OAuth metadata invalid: not a JSON object")?;
    for field in ["authorization_endpoint", "token_endpoint"] {
        if !doc.get(field).is_some_and(Value::is_string) {
            return Err(format!("Server OAuth metadata invalid: {field} — Required"));
        }
    }
    Ok(doc)
}

/// The SDK's `discoverOAuthServerInfo`: the resource's metadata names the
/// authorization server (else the server's origin), whose metadata follows.
pub(crate) async fn server_info(
    http: &OAuthHttp,
    server_url: &str,
    advertised: Option<&str>,
) -> Result<ServerInfo, String> {
    let resource = match resource_metadata(http, server_url, advertised).await {
        Ok(found) => found,
        Err(_) if advertised.is_none() => None,
        Err(e) => return Err(e),
    };
    let auth_server_url = resource
        .as_ref()
        .and_then(|(doc, _)| doc.get("authorization_servers")?.as_array()?.first()?.as_str().map(str::to_owned))
        .map_or_else(|| origin_root(server_url), Ok)?;
    let metadata = auth_server_metadata(http, &auth_server_url).await?;
    let resource_metadata = resource.map(|(d, _)| d);
    Ok(ServerInfo { auth_server_url, metadata, resource_metadata })
}

pub(crate) fn origin_root(url: &str) -> Result<String, String> {
    let mut u = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    u.set_path("/");
    u.set_query(None);
    u.set_fragment(None);
    Ok(u.to_string())
}

/// Whether `requested` lies under the resource the metadata names (same
/// origin, path prefix at a segment boundary).
pub(crate) fn resource_allowed(requested: &str, configured: &str) -> bool {
    let (Ok(r), Ok(c)) = (Url::parse(requested), Url::parse(configured)) else { return false };
    if r.origin() != c.origin() {
        return false;
    }
    let with_slash = |p: &str| if p.ends_with('/') { p.to_owned() } else { format!("{p}/") };
    with_slash(r.path()).starts_with(&with_slash(c.path()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_are_parsed() {
        let c = parse_challenge(
            r#"Bearer error="insufficient_scope", scope="read write", resource_metadata="https://x.dev/.well-known/oauth-protected-resource/mcp""#,
        )
        .expect("bearer");
        assert_eq!(c.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(c.scope.as_deref(), Some("read write"));
        assert_eq!(c.resource_metadata.as_deref(), Some("https://x.dev/.well-known/oauth-protected-resource/mcp"));
        assert_eq!(
            parse_challenge("Bearer realm=x, error=invalid_token").and_then(|c| c.error).as_deref(),
            Some("invalid_token")
        );
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn well_known_urls_follow_the_sdk() {
        let u = Url::parse("https://x.dev/api/mcp/?q=1").expect("url");
        assert_eq!(
            well_known(&u, "oauth-protected-resource", true),
            "https://x.dev/.well-known/oauth-protected-resource/api/mcp?q=1"
        );
        assert_eq!(
            well_known(&u, "oauth-protected-resource", false),
            "https://x.dev/.well-known/oauth-protected-resource"
        );
        assert!(resource_allowed("https://x.dev/api/mcp", "https://x.dev/api"));
        assert!(!resource_allowed("https://x.dev/apix", "https://x.dev/api"));
        assert_eq!(form_encode(&[("a", "x y&z")]), "a=x+y%26z");
    }
}
