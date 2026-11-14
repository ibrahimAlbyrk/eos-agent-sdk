//! Cross-app access (XAA) against a mock IdP, authorization server and MCP
//! server: the user's OIDC sign-in at the IdP, the RFC 8693 exchange for an
//! ID-JAG, the RFC 7523 grant at the authorization server, the pinned server
//! and the silent renewal from the cached ID token. Token endpoints must be
//! https, so they are served over TLS (a self-signed certificate the test
//! client trusts).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use eos_agent_anthropic::auth::FileSecrets;
use eos_agent_core::{CancellationToken, Extensions, SecretStore, ToolContext, ToolOutput};
use eos_agent_mcp::{
    AuthPrompter, AuthRequest, McpCredentials, McpHub, McpLimits, McpServerConfig, McpToolset, OAuthConfig,
    OAuthOptions, XaaIdp,
};
use eos_agent_types::ToolResultContent;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const ID_JAG: &str = "urn:ietf:params:oauth:token-type:id-jag";

#[derive(Default)]
struct Mock {
    base: String,
    tls: String,
    /// Valid access tokens at the MCP server.
    access: Vec<String>,
    idp_logins: usize,
    exchanges: Vec<HashMap<String, String>>,
    grants: Vec<(HashMap<String, String>, Option<String>)>,
    /// The IdP refuses the next token exchange with this status.
    refuse_exchange: Option<u16>,
    next: u32,
}

type Shared = Arc<Mutex<Mock>>;

fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn id_token() -> String {
    let enc = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", enc(json!({"alg": "none"})), enc(json!({"sub": "u1", "exp": now_s() + 3600})))
}

async fn start() -> Shared {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/.well-known/oauth-protected-resource/mcp", get(prm))
        .route("/.well-known/oauth-authorization-server/as", get(as_metadata))
        .route("/idp/.well-known/openid-configuration", get(oidc))
        .route("/idp/authorize", get(idp_authorize))
        .route("/mcp", axum::routing::post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    lock(&shared).base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let tls = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    lock(&shared).tls = format!("https://{}", tls.local_addr().expect("addr"));
    tokio::spawn(serve_tls(tls, Arc::clone(&shared)));
    shared
}

async fn prm(State(m): State<Shared>) -> Json<Value> {
    let base = lock(&m).base.clone();
    Json(json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/as")]}))
}

async fn as_metadata(State(m): State<Shared>) -> Json<Value> {
    let (base, tls) = {
        let m = lock(&m);
        (m.base.clone(), m.tls.clone())
    };
    Json(json!({
        "issuer": format!("{base}/as"),
        "authorization_endpoint": format!("{base}/as/authorize"),
        "token_endpoint": format!("{tls}/as/token"),
        "grant_types_supported": ["urn:ietf:params:oauth:grant-type:jwt-bearer"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
    }))
}

async fn oidc(State(m): State<Shared>) -> Json<Value> {
    let (base, tls) = {
        let m = lock(&m);
        (m.base.clone(), m.tls.clone())
    };
    Json(json!({
        "issuer": format!("{base}/idp"),
        "authorization_endpoint": format!("{base}/idp/authorize"),
        "token_endpoint": format!("{tls}/idp/token"),
        "jwks_uri": format!("{base}/idp/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn idp_authorize(State(m): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    lock(&m).idp_logins += 1;
    assert_eq!(q["scope"], "openid");
    assert_eq!(q["client_id"], "idp-client");
    assert_eq!(q["code_challenge_method"], "S256");
    let location = format!("{}?code=idp-code&state={}", q["redirect_uri"], q["state"]);
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

async fn mcp(State(m): State<Shared>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
    let token =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
    let ok = token.is_some_and(|t| lock(&m).access.iter().any(|a| a == t));
    if !ok {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")]).into_response();
    }
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"}}),
        "tools/list" => json!({"tools": [{"name": "whoami", "inputSchema": {"type": "object"}}]}),
        "tools/call" => json!({"content": [{"type": "text", "text": format!("token {}", token.unwrap_or_default())}]}),
        _ => json!({}),
    };
    ([("mcp-session-id", "s1")], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response()
}

/// The token endpoints, over TLS (HTTP/1.1, one request per connection).
async fn serve_tls(listener: tokio::net::TcpListener, m: Shared) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cert = CertificateDer::from(std::fs::read(dir.join("tls-cert.der")).expect("cert"));
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(std::fs::read(dir.join("tls-key.der")).expect("key")));
    let config = ServerConfig::builder().with_no_client_auth().with_single_cert(vec![cert], key).expect("tls config");
    let acceptor = TlsAcceptor::from(Arc::new(config));
    loop {
        let Ok((tcp, _)) = listener.accept().await else { return };
        let (acceptor, m) = (acceptor.clone(), Arc::clone(&m));
        tokio::spawn(async move {
            let Ok(mut tls) = acceptor.accept(tcp).await else { return };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let (head, body) = loop {
                let Ok(n) = tls.read(&mut buf).await else { return };
                if n == 0 {
                    return;
                }
                raw.extend_from_slice(&buf[..n]);
                if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..at]).into_owned();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_owned())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    while raw.len() < at + 4 + len {
                        let Ok(n) = tls.read(&mut buf).await else { return };
                        if n == 0 {
                            break;
                        }
                        raw.extend_from_slice(&buf[..n]);
                    }
                    break (head, String::from_utf8_lossy(&raw[at + 4..]).into_owned());
                }
            };
            let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let auth = head
                .lines()
                .find_map(|l| l.strip_prefix("authorization: ").or_else(|| l.strip_prefix("Authorization: ")))
                .map(str::to_owned);
            let parsed = reqwest::Url::parse(&format!("http://x/?{body}")).expect("form");
            let form: HashMap<String, String> =
                parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            let (status, answer) = token_endpoint(&m, &path, form, auth);
            let text = answer.to_string();
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
            let _ = tls.write_all(response.as_bytes()).await;
            let _ = tls.shutdown().await;
        });
    }
}

fn token_endpoint(m: &Shared, path: &str, form: HashMap<String, String>, auth: Option<String>) -> (u16, Value) {
    let mut m = lock(m);
    let base = m.base.clone();
    match (path, form.get("grant_type").map(String::as_str)) {
        ("/idp/token", Some("authorization_code")) => {
            assert_eq!(form["code"], "idp-code");
            assert!(form.contains_key("code_verifier"));
            (200, json!({"id_token": id_token(), "token_type": "Bearer", "expires_in": 3600}))
        }
        ("/idp/token", Some("urn:ietf:params:oauth:grant-type:token-exchange")) => {
            m.exchanges.push(form.clone());
            if let Some(status) = m.refuse_exchange.take() {
                return (status, json!({"error": "invalid_grant", "subject_token": "leak"}));
            }
            assert_eq!(form["audience"], format!("{base}/as"));
            assert_eq!(form["resource"], format!("{base}/mcp"));
            assert_eq!(form["requested_token_type"], ID_JAG);
            (200, json!({"access_token": "jag-1", "issued_token_type": ID_JAG, "token_type": "N_A"}))
        }
        ("/as/token", Some("urn:ietf:params:oauth:grant-type:jwt-bearer")) => {
            m.grants.push((form.clone(), auth.clone()));
            let basic = base64::engine::general_purpose::STANDARD.encode("as-client:as-secret");
            if auth.as_deref() != Some(&format!("Basic {basic}")) || form["assertion"] != "jag-1" {
                return (401, json!({"error": "invalid_client"}));
            }
            m.next += 1;
            let token = format!("at-xaa-{}", m.next);
            m.access.push(token.clone());
            (200, json!({"access_token": token, "token_type": "Bearer", "expires_in": 3600, "scope": "read"}))
        }
        _ => (400, json!({"error": "unsupported_grant_type"})),
    }
}

/// Follows the IdP's redirect back to the callback listener.
#[derive(Default)]
struct Browser(Mutex<Vec<AuthRequest>>);

impl AuthPrompter for Browser {
    fn authorize(&self, request: &AuthRequest) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(request.clone());
        let url = request.url.clone();
        tokio::spawn(async move {
            let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("c");
            let res = client.get(url).send().await.expect("authorize");
            let location = res.headers()[header::LOCATION].to_str().expect("location").to_owned();
            let _ = reqwest::get(location).await;
        });
    }
}

static DIRS: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eos-mcp-xaa-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn key_of(base: &str) -> String {
    let reached = format!(r#"{{"type":"http","url":"{base}/mcp","headers":{{}}}}"#);
    format!("xaa|{}", &hex::encode(Sha256::digest(reached.as_bytes()))[..16])
}

async fn seed(dir: &std::path::Path, base: &str) {
    let doc = json!({"mcpOAuthClientConfig": {key_of(base): {"clientSecret": "as-secret"}}});
    FileSecrets::new(dir.join(".credentials.json")).write(&doc.to_string()).await.expect("seed");
}

async fn stored(dir: &std::path::Path) -> Value {
    let raw = FileSecrets::new(dir.join(".credentials.json")).read().await.expect("read").unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn hub(dir: &std::path::Path, base: &str, browser: Arc<Browser>) -> Arc<McpHub> {
    let credentials = McpCredentials::new(Arc::new(FileSecrets::new(dir.join(".credentials.json"))), dir);
    let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().expect("client");
    let options = OAuthOptions {
        xaa_idp: Some(XaaIdp { issuer: format!("{base}/idp"), client_id: "idp-client".into(), callback_port: None }),
        http: Some(http),
        xaa_enabled: Some(true),
    };
    McpHub::with_oauth_options(McpLimits::default(), credentials, browser, options)
}

fn config(base: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::http("xaa", &format!("{base}/mcp"));
    if let eos_agent_mcp::McpTransport::Http { oauth, .. } = &mut cfg.transport {
        *oauth = Some(OAuthConfig { client_id: Some("as-client".into()), xaa: true, ..OAuthConfig::default() });
    }
    cfg
}

async fn call(set: &McpToolset, name: &str) -> ToolOutput {
    let tool = set.tools().into_iter().find(|t| t.spec().name() == name).expect("tool");
    let input = RawValue::from_string("{}".into()).expect("json");
    let (cancel, extensions) = (CancellationToken::new(), Extensions::default());
    let ctx =
        ToolContext { agent_id: "a1", tool_use_id: "t1", cancel: &cancel, extensions: &extensions, transcript: &[] };
    tool.call(&input, ctx).await
}

fn text(out: &ToolOutput) -> String {
    match &out.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                eos_agent_types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn signs_in_through_the_idp_and_exchanges_for_tokens() {
    let mock = start().await;
    let base = lock(&mock).base.clone();
    let dir = temp_dir();
    seed(&dir, &base).await;
    let browser = Arc::new(Browser::default());
    let hub = hub(&dir, &base, Arc::clone(&browser));
    let set = hub.connected_toolset(&[config(&base)]).await;
    assert_eq!(set.needs_auth(), ["xaa"]);

    let out = call(&set, "mcp__xaa__authenticate").await;
    assert!(text(&out).contains("/idp/authorize?"), "{}", text(&out));
    until("the server's tools", || set.tools().iter().any(|t| t.spec().name() == "mcp__xaa__whoami")).await;
    assert_eq!(text(&call(&set, "mcp__xaa__whoami").await), "token at-xaa-1");
    {
        let m = lock(&mock);
        assert_eq!(m.idp_logins, 1);
        let x = &m.exchanges[0];
        assert_eq!(x["client_id"], "idp-client");
        assert_eq!(x["subject_token_type"], "urn:ietf:params:oauth:token-type:id_token");
        assert!(!x.contains_key("client_secret"));
    }
    let doc = stored(&dir).await;
    let entry = &doc["mcpOAuth"][key_of(&base)];
    assert_eq!(entry["accessToken"], "at-xaa-1");
    assert_eq!(entry["clientId"], "as-client");
    assert_eq!(entry["clientSecret"], "as-secret");
    assert_eq!(
        entry["discoveryState"],
        json!({"authorizationServerUrl": format!("{base}/as"), "oauthMetadataFound": true,
            "authServerMetadataUrl": null, "xaaPinned": true})
    );
    let cached = &doc["mcpXaaIdp"][format!("{base}/idp")];
    assert!(cached["idToken"].as_str().is_some_and(|t| t.ends_with(".sig")));
    assert!(cached["expiresAt"].as_u64().is_some_and(|at| at > now_s() * 1000));

    // Expired without a refresh token: renewed from the cached ID token, no login.
    FileSecrets::new(dir.join(".credentials.json"))
        .write(&{
            let mut doc = stored(&dir).await;
            doc["mcpOAuth"][key_of(&base)]["expiresAt"] = json!(1);
            doc.to_string()
        })
        .await
        .expect("expire");
    let hub2 = self::hub(&dir, &base, Arc::new(Browser::default()));
    let set2 = hub2.connected_toolset(&[config(&base)]).await;
    assert_eq!(text(&call(&set2, "mcp__xaa__whoami").await), "token at-xaa-2");
    assert_eq!(lock(&mock).idp_logins, 1);
}

#[tokio::test]
async fn a_refused_exchange_drops_the_cached_id_token() {
    let mock = start().await;
    let base = lock(&mock).base.clone();
    let dir = temp_dir();
    seed(&dir, &base).await;
    let hub = hub(&dir, &base, Arc::new(Browser::default()));
    let set = hub.connected_toolset(&[config(&base)]).await;
    lock(&mock).refuse_exchange = Some(400);
    let out = call(&set, "mcp__xaa__authenticate").await;
    let shown = text(&out);
    assert!(
        shown.contains(
            "XAA: token exchange failed: HTTP 400: {\"error\":\"invalid_grant\",\"subject_token\":\"[REDACTED]\"}"
        ) || shown.contains("/idp/authorize?"),
        "{shown}"
    );
    until("the exchange", || !lock(&mock).exchanges.is_empty()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(stored(&dir).await["mcpXaaIdp"].as_object().is_none_or(|m| m.is_empty()), "the ID token was dropped");
}

#[tokio::test]
async fn without_the_switch_xaa_is_refused_with_claude_codes_words() {
    let mock = start().await;
    let base = lock(&mock).base.clone();
    let dir = temp_dir();
    let credentials = McpCredentials::new(Arc::new(FileSecrets::new(dir.join(".credentials.json"))), &dir);
    let options = OAuthOptions { xaa_enabled: Some(false), ..OAuthOptions::default() };
    let hub = McpHub::with_oauth_options(McpLimits::default(), credentials, Arc::new(Browser::default()), options);
    let set = hub.connected_toolset(&[config(&base)]).await;
    let out = call(&set, "mcp__xaa__authenticate").await;
    assert!(
        text(&out).contains(
            "XAA is not enabled (set CLAUDE_CODE_ENABLE_XAA=1). Remove 'oauth.xaa' from server 'xaa' to use the \
             standard consent flow."
        ),
        "{}",
        text(&out)
    );
}
