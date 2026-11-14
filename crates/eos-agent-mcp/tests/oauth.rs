//! OAuth against a mock authorization server and a protected MCP server
//! (both in-process, on 127.0.0.1): discovery, dynamic registration, PKCE,
//! the callback, stored sign-ins, refreshes and re-authorization.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use eos_agent_anthropic::auth::FileSecrets;
use eos_agent_core::{BoxFuture, CancellationToken, Extensions, SecretStore, Tool, ToolContext, ToolOutput};
use eos_agent_mcp::{
    AuthPrompter, AuthRequest, McpCredentials, McpHub, McpLimits, McpServerConfig, McpToolset, StepUpAnswer,
    StepUpRequest,
};
use eos_agent_types::{ContentBlock, ToolResultContent};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---- the mock servers --------------------------------------------------------

#[derive(Default)]
struct Grant {
    client_id: String,
    challenge: String,
    redirect_uri: String,
    scope: String,
}

#[derive(Default)]
struct Mock {
    base: String,
    registrations: Vec<Value>,
    authorizations: Vec<HashMap<String, String>>,
    token_requests: Vec<HashMap<String, String>>,
    clients: Vec<String>,
    codes: HashMap<String, Grant>,
    /// Access token → scope.
    access: HashMap<String, String>,
    /// Refresh token → scope; each works once.
    refresh: HashMap<String, String>,
    refreshes: usize,
    refused_refreshes: usize,
    next: u64,
    /// Revocation requests (RFC 7009), as sent.
    revoked: Vec<HashMap<String, String>>,
    /// Requests that reached the MCP endpoint.
    mcp_hits: usize,
    /// Never grants the `admin` scope.
    withhold_admin: bool,
    /// Publishes no metadata: the legacy `/register`, `/authorize` and
    /// `/token` at the origin, a challenge naming only a scope.
    bare: bool,
}

type Shared = Arc<Mutex<Mock>>;

fn lock(m: &Shared) -> std::sync::MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Mock {
    fn issue(&mut self, scope: &str) -> Value {
        self.next += 1;
        let (access, refresh) = (format!("at-{}", self.next), format!("rt-{}", self.next));
        self.access.insert(access.clone(), scope.to_owned());
        self.refresh.insert(refresh.clone(), scope.to_owned());
        json!({"access_token": access, "refresh_token": refresh, "token_type": "Bearer", "expires_in": 3600, "scope": scope})
    }
}

async fn start_mock() -> Shared {
    let shared: Shared = Arc::default();
    let app = Router::new()
        .route("/.well-known/oauth-protected-resource/mcp", get(resource_metadata))
        .route("/.well-known/oauth-authorization-server/as", get(auth_server_metadata))
        .route("/as/register", post(register))
        .route("/as/authorize", get(authorize))
        .route("/as/token", post(token))
        .route("/as/revoke", post(revoke))
        .route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }).delete(|| async { StatusCode::OK }))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    lock(&shared).base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    shared
}

/// A server and authorization server without any metadata.
async fn start_bare_mock() -> Shared {
    let shared: Shared = Arc::new(Mutex::new(Mock { bare: true, ..Mock::default() }));
    let app = Router::new()
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/mcp", post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }).delete(|| async { StatusCode::OK }))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    lock(&shared).base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    shared
}

async fn resource_metadata(State(m): State<Shared>) -> Json<Value> {
    let base = lock(&m).base.clone();
    Json(
        json!({"resource": format!("{base}/mcp"), "authorization_servers": [format!("{base}/as")], "scopes_supported": ["read"]}),
    )
}

async fn auth_server_metadata(State(m): State<Shared>) -> Json<Value> {
    let base = lock(&m).base.clone();
    Json(json!({
        "issuer": format!("{base}/as"),
        "authorization_endpoint": format!("{base}/as/authorize"),
        "token_endpoint": format!("{base}/as/token"),
        "revocation_endpoint": format!("{base}/as/revoke"),
        "registration_endpoint": format!("{base}/as/register"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["read", "admin", "offline_access"],
    }))
}

async fn revoke(State(m): State<Shared>, Form(form): Form<HashMap<String, String>>) -> StatusCode {
    let mut m = lock(&m);
    if let Some(token) = form.get("token") {
        m.access.remove(token);
        m.refresh.remove(token);
    }
    m.revoked.push(form);
    StatusCode::OK
}

async fn register(State(m): State<Shared>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let mut m = lock(&m);
    let id = format!("client-{}", m.registrations.len() + 1);
    m.registrations.push(body.clone());
    m.clients.push(id.clone());
    let mut answer = body;
    answer["client_id"] = id.into();
    (StatusCode::CREATED, Json(answer))
}

async fn authorize(State(m): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut m = lock(&m);
    m.authorizations.push(q.clone());
    let client = q.get("client_id").cloned().unwrap_or_default();
    if !m.clients.contains(&client) || q.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return StatusCode::BAD_REQUEST.into_response();
    }
    m.next += 1;
    let code = format!("code-{}", m.next);
    let grant = Grant {
        client_id: client,
        challenge: q.get("code_challenge").cloned().unwrap_or_default(),
        redirect_uri: q.get("redirect_uri").cloned().unwrap_or_default(),
        scope: {
            let asked = q.get("scope").cloned().unwrap_or_default();
            if m.withhold_admin {
                asked.split(' ').filter(|s| *s != "admin").collect::<Vec<_>>().join(" ")
            } else {
                asked
            }
        },
    };
    let location = format!("{}?code={code}&state={}", grant.redirect_uri, q.get("state").cloned().unwrap_or_default());
    m.codes.insert(code, grant);
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

async fn token(State(m): State<Shared>, Form(form): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
    let mut m = lock(&m);
    m.token_requests.push(form.clone());
    let refused = |error: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
    let field = |k: &str| form.get(k).cloned().unwrap_or_default();
    match field("grant_type").as_str() {
        "authorization_code" => {
            let Some(grant) = m.codes.remove(&field("code")) else { return refused("invalid_grant") };
            let verifier = field("code_verifier");
            let challenge =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            if challenge != grant.challenge
                || field("redirect_uri") != grant.redirect_uri
                || field("client_id") != grant.client_id
            {
                return refused("invalid_grant");
            }
            let scope = grant.scope.clone();
            (StatusCode::OK, Json(m.issue(&scope)))
        }
        "refresh_token" => match m.refresh.remove(&field("refresh_token")) {
            Some(scope) => {
                m.refreshes += 1;
                (StatusCode::OK, Json(m.issue(&scope)))
            }
            None => {
                m.refused_refreshes += 1;
                refused("invalid_grant")
            }
        },
        _ => refused("unsupported_grant_type"),
    }
}

async fn mcp(State(m): State<Shared>, headers: HeaderMap, Json(msg): Json<Value>) -> Response {
    let (base, scope, bare) = {
        let mut m = lock(&m);
        m.mcp_hits += 1;
        let token =
            headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        (m.base.clone(), token.and_then(|t| m.access.get(t).cloned()), m.bare)
    };
    let Some(scope) = scope else {
        let challenge = if bare {
            r#"Bearer error="invalid_token", scope="read""#.to_owned()
        } else {
            format!(
                r#"Bearer error="invalid_token", resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
            )
        };
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, challenge)]).into_response();
    };
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"},
        }),
        "tools/list" => json!({"tools": [
            {"name": "echo", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
            {"name": "admin", "inputSchema": {"type": "object"}},
        ]}),
        "tools/call" if msg["params"]["name"] == "admin" && !scope.split(' ').any(|s| s == "admin") => {
            let challenge = r#"Bearer error="insufficient_scope", scope="admin""#;
            return (StatusCode::FORBIDDEN, [(header::WWW_AUTHENTICATE, challenge)]).into_response();
        }
        "tools/call" => {
            let text = format!(
                "{}:{}",
                msg["params"]["name"].as_str().unwrap_or_default(),
                msg["params"]["arguments"]["text"].as_str().unwrap_or("-")
            );
            json!({"content": [{"type": "text", "text": text}]})
        }
        other => {
            return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": other}}))
                .into_response();
        }
    };
    ([("mcp-session-id", "s1")], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response()
}

// ---- the user's browser --------------------------------------------------------

/// Plays the user: opens the authorization URL and follows the redirect back
/// to the callback listener (unless `follow` is off).
#[derive(Default)]
struct Browser {
    follow: bool,
    seen: Mutex<Vec<AuthRequest>>,
    /// How the user answers the step-up question (unanswered by default).
    step_up: Option<StepUpAnswer>,
    asked: Mutex<Vec<StepUpRequest>>,
}

impl Browser {
    fn following() -> Arc<Self> {
        Arc::new(Self { follow: true, ..Self::default() })
    }

    fn answering(answer: StepUpAnswer) -> Arc<Self> {
        Arc::new(Self { follow: true, step_up: Some(answer), ..Self::default() })
    }

    fn last_url(&self) -> String {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).last().map(|r| r.url.clone()).unwrap_or_default()
    }
}

impl AuthPrompter for Browser {
    fn authorize(&self, request: &AuthRequest) {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner).push(request.clone());
        if !self.follow {
            return;
        }
        let url = request.url.clone();
        tokio::spawn(async move {
            let location = redirect_of(&url).await;
            let _ = reqwest::get(location).await;
        });
    }

    fn ask_step_up<'a>(&'a self, request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        self.asked.lock().unwrap_or_else(PoisonError::into_inner).push(request.clone());
        let answer = self.step_up.unwrap_or(StepUpAnswer::Unanswered);
        Box::pin(async move { answer })
    }
}

async fn redirect_of(url: &str) -> String {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().expect("client");
    let res = client.get(url).send().await.expect("authorize");
    res.headers()[header::LOCATION].to_str().expect("location").to_owned()
}

// ---- helpers --------------------------------------------------------------------

static DIRS: AtomicU32 = AtomicU32::new(0);

fn temp_dir() -> PathBuf {
    let n = DIRS.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("eos-mcp-oauth-test-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn credentials(dir: &std::path::Path) -> McpCredentials {
    McpCredentials::new(Arc::new(FileSecrets::new(dir.join(".credentials.json"))), dir)
}

fn hub(dir: &std::path::Path, browser: Arc<Browser>) -> Arc<McpHub> {
    McpHub::with_oauth(McpLimits::default(), credentials(dir), browser)
}

fn config(mock: &Shared) -> McpServerConfig {
    McpServerConfig::http("docs", &format!("{}/mcp", lock(mock).base))
}

fn names(set: &McpToolset) -> Vec<String> {
    set.tools().iter().map(|t| t.spec().name().to_owned()).collect()
}

fn find(set: &McpToolset, name: &str) -> Arc<dyn Tool> {
    set.tools().into_iter().find(|t| t.spec().name() == name).unwrap_or_else(|| panic!("no {name} in {:?}", names(set)))
}

async fn call(set: &McpToolset, name: &str, input: &str) -> ToolOutput {
    let tool = find(set, name);
    let input = RawValue::from_string(input.to_owned()).expect("json");
    let (cancel, extensions) = (CancellationToken::new(), Extensions::default());
    let ctx = ToolContext {
        agent_id: "agent-1",
        tool_use_id: "toolu_1",
        cancel: &cancel,
        extensions: &extensions,
        transcript: &[],
    };
    tool.call(&input, ctx).await
}

fn text(out: &ToolOutput) -> String {
    match &out.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Signs in through the `authenticate` tool, the browser following along.
async fn sign_in(set: &McpToolset) {
    let out = call(set, "mcp__docs__authenticate", "{}").await;
    assert!(text(&out).contains("Ask the user to open this URL in their browser"), "{}", text(&out));
    until("the server's own tools", || names(set).contains(&"mcp__docs__echo".to_owned())).await;
}

async fn stored(dir: &std::path::Path) -> Value {
    let raw = FileSecrets::new(dir.join(".credentials.json")).read().await.expect("read").unwrap_or_default();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

fn entry_of(doc: &Value) -> &Value {
    let all = doc["mcpOAuth"].as_object().expect("mcpOAuth");
    let (key, entry) = all.iter().find(|(k, _)| k.starts_with("docs|")).expect("docs entry");
    assert_eq!(key.len(), "docs|".len() + 16);
    entry
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A sign-in as if made earlier (by Claude Code or another process).
async fn seed(dir: &std::path::Path, mock: &Shared, access: &str, refresh: &str, expires_in_ms: u64) {
    let base = lock(mock).base.clone();
    // Claude Code's key: the name and sha256 of JSON {type, url, headers}.
    let reached = format!(r#"{{"type":"http","url":"{base}/mcp","headers":{{}}}}"#);
    let key = format!("docs|{}", &hex::encode(Sha256::digest(reached.as_bytes()))[..16]);
    // Registered with a callback port that is free now, so a new sign-in reuses it.
    let port = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("port").port();
    {
        let mut m = lock(mock);
        m.clients.push("pre".into());
        m.refresh.insert(refresh.into(), "read".into());
    }
    let doc = json!({"claudeAiOauth": {"accessToken": "keep-me"}, "mcpOAuth": {key: {
        "serverName": "docs", "serverUrl": format!("{base}/mcp"), "accessToken": access, "refreshToken": refresh,
        "expiresAt": now_ms() + expires_in_ms, "clientId": "pre", "redirectUri": format!("http://localhost:{port}/callback"),
        "discoveryState": {"authorizationServerUrl": format!("{base}/as")},
    }}});
    FileSecrets::new(dir.join(".credentials.json")).write(&doc.to_string()).await.expect("seed");
}

// ---- tests ------------------------------------------------------------------------

#[tokio::test]
async fn signs_in_like_claude_code_and_reuses_the_sign_in_after_a_restart() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let browser = Browser::following();
    let hub1 = hub(&dir, Arc::clone(&browser));
    let set = hub1.connected_toolset(&[config(&mock)]).await;
    assert!(set.failures().is_empty(), "{:?}", set.failures());
    assert_eq!(set.needs_auth(), ["docs"]);
    assert_eq!(names(&set), ["mcp__docs__authenticate", "mcp__docs__complete_authentication"]);
    let before = set.version();

    sign_in(&set).await;
    assert_ne!(set.version(), before);
    assert!(set.needs_auth().is_empty());
    assert_eq!(text(&call(&set, "mcp__docs__echo", r#"{"text":"hi"}"#).await), "echo:hi");

    let base = lock(&mock).base.clone();
    {
        let m = lock(&mock);
        let reg = &m.registrations[0];
        assert_eq!(reg["client_name"], "Claude Code (docs)");
        assert_eq!(reg["token_endpoint_auth_method"], "none");
        assert_eq!(reg["grant_types"], json!(["authorization_code", "refresh_token"]));
        assert_eq!(reg["response_types"], json!(["code"]));
        let redirect = reg["redirect_uris"][0].as_str().expect("redirect");
        assert!(redirect.starts_with("http://localhost:") && redirect.ends_with("/callback"), "{redirect}");
        let auth = &m.authorizations[0];
        assert_eq!(auth["response_type"], "code");
        assert_eq!(auth["code_challenge_method"], "S256");
        assert_eq!(auth["state"].len(), 43);
        assert_eq!(auth["resource"], format!("{base}/mcp"));
        assert_eq!(auth["scope"], "read offline_access");
        assert_eq!(auth["prompt"], "consent");
        assert_eq!(auth["redirect_uri"], redirect);
        let exchange = &m.token_requests[0];
        assert_eq!(exchange["client_id"], "client-1");
        assert_eq!(exchange["resource"], format!("{base}/mcp"));
    }
    let doc = stored(&dir).await;
    let entry = entry_of(&doc);
    assert_eq!(entry["serverName"], "docs");
    assert_eq!(entry["serverUrl"], format!("{base}/mcp"));
    assert_eq!(entry["clientId"], "client-1");
    assert!(entry["accessToken"].as_str().is_some_and(|t| t.starts_with("at-")));
    assert!(entry["refreshToken"].as_str().is_some_and(|t| t.starts_with("rt-")));
    assert!(entry["expiresAt"].as_u64().is_some_and(|at| at > now_ms()));
    assert_eq!(entry["discoveryState"]["authorizationServerUrl"], format!("{base}/as"));
    assert_eq!(
        entry["discoveryState"]["resourceMetadataUrl"],
        format!("{base}/.well-known/oauth-protected-resource/mcp")
    );

    // A new process (hub) finds the sign-in and connects straight away.
    drop(set);
    drop(hub1);
    let hub2 = hub(&dir, Browser::following());
    let set = hub2.connected_toolset(&[config(&mock)]).await;
    assert!(set.needs_auth().is_empty());
    assert_eq!(text(&call(&set, "mcp__docs__echo", r#"{"text":"again"}"#).await), "echo:again");
    assert_eq!(lock(&mock).registrations.len(), 1);
    assert_eq!(lock(&mock).authorizations.len(), 1);
}

#[tokio::test]
async fn one_refresh_serves_every_caller_in_every_process() {
    let mock = start_mock().await;
    let dir = temp_dir();
    // Valid on the server, but within five minutes of expiry: renewed first.
    seed(&dir, &mock, "old", "r1", 60_000).await;
    lock(&mock).access.insert("old".into(), "read".into());
    let (a, b) = (hub(&dir, Browser::following()), hub(&dir, Browser::following()));
    let configs = [config(&mock)];
    let (set_a, set_b) = tokio::join!(a.connected_toolset(&configs), b.connected_toolset(&configs));
    let calls = (0..6).map(|i| {
        let set = if i % 2 == 0 { &set_a } else { &set_b };
        async move { text(&call(set, "mcp__docs__echo", r#"{"text":"x"}"#).await) }
    });
    for out in futures_util::future::join_all(calls).await {
        assert_eq!(out, "echo:x");
    }
    {
        let m = lock(&mock);
        assert_eq!((m.refreshes, m.refused_refreshes), (1, 0));
        let refresh = m.token_requests.iter().find(|r| r["grant_type"] == "refresh_token").expect("refresh");
        assert_eq!(refresh["client_id"], "pre");
        assert_eq!(refresh["resource"], format!("{}/mcp", m.base));
    }
    let doc = stored(&dir).await;
    assert_eq!(doc["claudeAiOauth"]["accessToken"], "keep-me");
    assert_eq!(entry_of(&doc)["refreshToken"], "rt-1");
}

#[tokio::test]
async fn a_refused_token_is_renewed_and_a_dead_sign_in_asks_for_a_new_one() {
    let mock = start_mock().await;
    let dir = temp_dir();
    // Looks valid for an hour, but the server no longer takes it.
    seed(&dir, &mock, "revoked", "r1", 3_600_000).await;
    let doc_before = stored(&dir).await;
    let hub = hub(&dir, Browser::following());
    let set = hub.connected_toolset(&[config(&mock)]).await;
    assert!(set.needs_auth().is_empty(), "{:?}", names(&set));
    assert_eq!(text(&call(&set, "mcp__docs__echo", r#"{"text":"ok"}"#).await), "echo:ok");
    assert_eq!(lock(&mock).refreshes, 1);

    // Everything revoked: the next call cannot recover by itself.
    {
        let mut m = lock(&mock);
        m.access.clear();
        m.refresh.clear();
    }
    let before = set.version();
    let out = call(&set, "mcp__docs__echo", r#"{"text":"no"}"#).await;
    assert!(out.is_error);
    assert!(text(&out).contains("MCP server \"docs\" needs you to sign in again"), "{}", text(&out));
    assert_ne!(set.version(), before);
    assert_eq!(set.needs_auth(), ["docs"]);
    assert_eq!(names(&set), ["mcp__docs__authenticate", "mcp__docs__complete_authentication"]);

    // Signing in again keeps the registered client and its callback port.
    sign_in(&set).await;
    assert_eq!(text(&call(&set, "mcp__docs__echo", r#"{"text":"back"}"#).await), "echo:back");
    let m = lock(&mock);
    assert!(m.registrations.is_empty());
    let auth = m.authorizations.last().expect("authorized");
    assert_eq!(auth["client_id"], "pre");
    assert_eq!(
        Some(&auth["redirect_uri"]),
        entry_of(&doc_before).get("redirectUri").and_then(Value::as_str).map(str::to_owned).as_ref()
    );
}

#[tokio::test]
async fn insufficient_scope_signs_in_again_for_more() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let browser = Browser::following();
    let hub = hub(&dir, Arc::clone(&browser));
    let set = hub.connected_toolset(&[config(&mock)]).await;
    sign_in(&set).await;

    // Nobody answers the question (as in `claude -p`): the call fails and the
    // server waits for a sign-in.
    let out = call(&set, "mcp__docs__admin", "{}").await;
    assert!(out.is_error);
    assert!(text(&out).contains(r#"needs additional permissions (scope: "admin")"#), "{}", text(&out));
    assert_eq!(browser.asked.lock().expect("lock").len(), 1);
    assert_eq!(set.needs_auth(), ["docs"]);
    assert_eq!(entry_of(&stored(&dir).await)["stepUpScope"], "read offline_access admin");

    sign_in(&set).await;
    assert_eq!(
        lock(&mock).authorizations.last().map(|a| a["scope"].clone()).as_deref(),
        Some("read offline_access admin")
    );
    assert_eq!(text(&call(&set, "mcp__docs__admin", "{}").await), "admin:-");
}

/// Claude Code's in-call step-up: the user is asked, signs in again in the
/// browser, and the same call is retried, its result led by the new scope.
#[tokio::test]
async fn a_step_up_asked_in_the_call_retries_it_after_the_sign_in() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let browser = Browser::answering(StepUpAnswer::Reauthenticate);
    let hub = hub(&dir, Arc::clone(&browser));
    let set = hub.connected_toolset(&[config(&mock)]).await;
    sign_in(&set).await;

    let out = call(&set, "mcp__docs__admin", "{}").await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(text(&out), "Re-authenticated MCP server \"docs\" (scope: \"admin\")admin:-");
    let asked = browser.asked.lock().expect("lock").clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        asked[0].question,
        "MCP server \"docs\" needs additional permissions for this tool (it asked for the scope \"admin\"). \
         Re-authenticate now?"
    );
    assert_eq!(asked[0].choices.iter().map(|c| c.label).collect::<Vec<_>>(), ["Re-authenticate in browser", "Not now"]);
    assert_eq!(asked[0].agent_id.as_deref(), Some("agent-1"));
    assert_eq!(
        lock(&mock).authorizations.last().map(|a| a["scope"].clone()).as_deref(),
        Some("read offline_access admin")
    );
    assert!(set.needs_auth().is_empty());
    // The first sign-in came from the auth tool, the second from the question.
    let steps: Vec<bool> = browser.seen.lock().expect("lock").iter().map(|r| r.step_up).collect();
    assert_eq!(steps, [false, true]);
}

/// A scope still refused right after its own re-authentication is not asked
/// about again (Claude Code's `stepUpReauthExhausted`).
#[tokio::test]
async fn a_step_up_refused_again_is_not_asked_again() {
    let mock = start_mock().await;
    lock(&mock).withhold_admin = true;
    let dir = temp_dir();
    let browser = Browser::answering(StepUpAnswer::Reauthenticate);
    let hub = hub(&dir, Arc::clone(&browser));
    let set = hub.connected_toolset(&[config(&mock)]).await;
    sign_in(&set).await;

    let out = call(&set, "mcp__docs__admin", "{}").await;
    assert!(out.is_error);
    assert!(text(&out).contains(r#"needs additional permissions (scope: "admin")"#), "{}", text(&out));
    assert_eq!(browser.asked.lock().expect("lock").len(), 1, "one question, one retry");
    sign_in(&set).await;
    let again = call(&set, "mcp__docs__admin", "{}").await;
    assert!(again.is_error);
    assert_eq!(browser.asked.lock().expect("lock").len(), 1, "not asked again");
}

#[tokio::test]
async fn a_pasted_callback_url_completes_the_sign_in() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let browser = Arc::new(Browser::default());
    let hub = hub(&dir, Arc::clone(&browser));
    let set = hub.connected_toolset(&[config(&mock)]).await;

    let none =
        call(&set, "mcp__docs__complete_authentication", r#"{"callback_url":"http://localhost:1/callback?code=x"}"#)
            .await;
    assert!(text(&none).starts_with("No OAuth flow is in progress for docs."), "{}", text(&none));

    let out = call(&set, "mcp__docs__authenticate", "{}").await;
    let url = browser.last_url();
    assert!(text(&out).contains(&url), "{}", text(&out));
    let again = call(&set, "mcp__docs__authenticate", "{}").await;
    assert!(text(&again).starts_with("A sign-in for docs that started earlier is still in progress"));
    assert_eq!(browser.seen.lock().expect("lock").len(), 1);
    let seen = browser.seen.lock().expect("lock")[0].clone();
    assert_eq!(seen.agent_id.as_deref(), Some("agent-1"));
    assert_eq!(seen.server, "docs");

    let callback = redirect_of(&url).await;
    let forged = callback.replace("state=", "state=x");
    let wrong = call(&set, "mcp__docs__complete_authentication", &json!({"callback_url": forged}).to_string()).await;
    assert!(text(&wrong).contains("belongs to a different sign-in attempt"), "{}", text(&wrong));

    // A request to the listener with the wrong state is turned away too.
    let res = reqwest::get(&forged).await.expect("listener");
    assert_eq!(res.status(), 400);

    let done = call(&set, "mcp__docs__complete_authentication", &json!({"callback_url": callback}).to_string()).await;
    assert_eq!(text(&done), "Authentication complete for docs. The server's tools should now be available.");
    assert!(names(&set).contains(&"mcp__docs__echo".to_owned()));
    assert_eq!(text(&call(&set, "mcp__docs__echo", r#"{"text":"pasted"}"#).await), "echo:pasted");
}

#[tokio::test]
async fn servers_with_their_own_authorization_header_never_sign_in() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let mut cfg = config(&mock);
    if let eos_agent_mcp::McpTransport::Http { headers, .. } = &mut cfg.transport {
        headers.insert("Authorization".into(), "Bearer nope".into());
    }
    let hub = hub(&dir, Browser::following());
    let set = hub.connected_toolset(&[cfg]).await;
    assert!(set.needs_auth().is_empty());
    assert_eq!(set.failures().len(), 1);
    assert!(names(&set).is_empty());
}

/// `/mcp`'s "Clear authentication": the tokens are revoked at the server's
/// revocation endpoint (refresh token first), the entry is forgotten and the
/// server asks for a sign-in again; status follows along.
#[tokio::test]
async fn signing_out_revokes_and_forgets_the_sign_in() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let hub = hub(&dir, Browser::following());
    let set = hub.connected_toolset(&[config(&mock)]).await;
    assert_eq!(set.status()[0].state, eos_agent_mcp::McpServerState::NeedsAuth);
    sign_in(&set).await;
    assert_eq!(set.status()[0].state, eos_agent_mcp::McpServerState::Connected);
    assert_eq!(set.status()[0].tools, 2);
    let tokens = entry_of(&stored(&dir).await).clone();

    assert_eq!(set.sign_out("docs").await, Ok(true));
    let revoked = lock(&mock).revoked.clone();
    let sent: Vec<(&str, &str)> =
        revoked.iter().map(|f| (f["token_type_hint"].as_str(), f["token"].as_str())).collect();
    assert_eq!(
        sent,
        [
            ("refresh_token", tokens["refreshToken"].as_str().expect("refresh")),
            ("access_token", tokens["accessToken"].as_str().expect("access")),
        ]
    );
    assert!(revoked.iter().all(|f| f.get("client_id").is_some()), "a public client names itself");
    // The tokens are gone; the reconnect's discovery is kept again, as in Claude Code.
    let doc = stored(&dir).await;
    let entry = entry_of(&doc);
    assert_eq!(entry["accessToken"], "", "{doc}");
    assert!(entry.get("refreshToken").is_none() && entry.get("clientId").is_none(), "{doc}");
    assert_eq!(set.needs_auth(), ["docs"], "it asks for a sign-in again");
    assert!(set.sign_out("nope").await.is_err());
    assert!(set.reconnect("docs").await.is_ok());
}

/// Connecting before any sign-in runs discovery and keeps it as Claude Code
/// does (no client registered yet: the server offers registration); the
/// refused tokenless request is noted, so a new process skips connecting even
/// without the 15-minute cache; a sign-in clears the note.
#[tokio::test]
async fn connecting_before_a_sign_in_keeps_discovery_like_claude_code() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let hub1 = hub(&dir, Browser::following());
    let first = hub1.connected_toolset(&[config(&mock)]).await;
    assert_eq!(first.needs_auth(), ["docs"]);
    let base = lock(&mock).base.clone();
    let doc = stored(&dir).await;
    let entry = entry_of(&doc);
    assert_eq!(entry["serverName"], "docs");
    assert_eq!(entry["accessToken"], "");
    assert!(entry.get("clientId").is_none(), "registration waits for the sign-in");
    let state = &entry["discoveryState"];
    assert_eq!(state["authorizationServerUrl"], format!("{base}/as"));
    assert_eq!(state["oauthMetadataFound"], true);
    assert_eq!(state["authServerMetadataUrl"], Value::Null);
    assert_eq!(state["resourceMetadataUrl"], format!("{base}/.well-known/oauth-protected-resource/mcp"));
    assert!(state["tokenlessRejectedAt"].as_u64().is_some_and(|at| at <= now_ms()), "{state}");
    assert!(state.get("registrationNotOfferedAt").is_none());
    assert!(lock(&mock).registrations.is_empty());

    let _ = std::fs::remove_file(dir.join("mcp-needs-auth-cache.json"));
    let hits = lock(&mock).mcp_hits;
    let hub2 = hub(&dir, Browser::following());
    let restarted = hub2.connected_toolset(&[config(&mock)]).await;
    assert_eq!(restarted.needs_auth(), ["docs"]);
    assert_eq!(lock(&mock).mcp_hits, hits, "skipped connecting (tokenlessRejectedAt)");

    sign_in(&restarted).await;
    let doc = stored(&dir).await;
    let state = &entry_of(&doc)["discoveryState"];
    assert!(state.get("tokenlessRejectedAt").is_none(), "{state}");
    assert_eq!(state["authServerMetadataUrl"], Value::Null);
    assert_eq!(state["oauthMetadataFound"], true);
}

/// A server whose authorization server publishes no metadata: as the MCP
/// SDK's `auth()` inside Claude Code's transport, connecting registers a
/// client at the origin's `/register` right away, for the redirect of the
/// provider Claude Code makes for connecting (port 3118); the sign-in then
/// listens on that port and uses that client instead of registering again.
#[tokio::test]
async fn connecting_to_a_server_without_metadata_registers_a_client_at_once() {
    let mock = start_bare_mock().await;
    let dir = temp_dir();
    let hub1 = hub(&dir, Browser::following());
    let set = hub1.connected_toolset(&[config(&mock)]).await;
    assert_eq!(set.needs_auth(), ["docs"]);
    let base = lock(&mock).base.clone();
    let registration = {
        let m = lock(&mock);
        assert_eq!(m.registrations.len(), 1, "{:?}", m.registrations);
        m.registrations[0].clone()
    };
    assert_eq!(
        registration,
        json!({
            "client_name": "Claude Code (docs)",
            "redirect_uris": ["http://localhost:3118/callback"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "native",
            "scope": "read",
        })
    );
    let doc = stored(&dir).await;
    let entry = entry_of(&doc);
    assert_eq!(entry["clientId"], "client-1");
    assert_eq!(entry["redirectUri"], "http://localhost:3118/callback");
    assert_eq!(entry["accessToken"], "");
    assert!(entry.get("clientSecret").is_none(), "{doc}");
    assert_eq!(entry["discoveryState"]["authorizationServerUrl"], format!("{base}/"));
    assert_eq!(entry["discoveryState"]["oauthMetadataFound"], false);

    // The sign-in reuses the port, and with it the client (when it is free).
    if std::net::TcpListener::bind(("127.0.0.1", 3118)).is_err() {
        eprintln!("port 3118 is busy: the sign-in half is skipped");
        return;
    }
    sign_in(&set).await;
    let m = lock(&mock);
    assert_eq!(m.registrations.len(), 1, "no second registration");
    assert_eq!(m.authorizations[0]["client_id"], "client-1");
    assert_eq!(m.authorizations[0]["redirect_uri"], "http://localhost:3118/callback");
    assert_eq!(m.token_requests[0]["client_id"], "client-1");
}

/// Claude Code's needs-auth cache: a server that asked for a sign-in is not
/// connected again for 15 minutes, even by another process; a sign-in clears it.
#[tokio::test]
async fn a_server_that_needs_a_sign_in_is_not_reconnected_for_a_while() {
    let mock = start_mock().await;
    let dir = temp_dir();
    let hub1 = hub(&dir, Browser::following());
    let first = hub1.connected_toolset(&[config(&mock)]).await;
    assert_eq!(first.needs_auth(), ["docs"]);
    let cache = dir.join("mcp-needs-auth-cache.json");
    until("the cache entry", || std::fs::read_to_string(&cache).is_ok_and(|t| t.contains("\"docs\""))).await;
    let hits = lock(&mock).mcp_hits;

    let hub2 = hub(&dir, Browser::following());
    let restarted = hub2.connected_toolset(&[config(&mock)]).await;
    assert_eq!(restarted.needs_auth(), ["docs"]);
    assert_eq!(lock(&mock).mcp_hits, hits, "skipped connecting (cached needs-auth)");
    sign_in(&restarted).await;
    until("the entry cleared", || std::fs::read_to_string(&cache).is_ok_and(|t| !t.contains("\"docs\""))).await;
}
