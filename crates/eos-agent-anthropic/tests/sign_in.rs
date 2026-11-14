//! Signing in to Claude against a mock authorization server, and renewing a
//! sign-in that several holders share.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use eos_agent_anthropic::auth::{
    ClaudeLogin, Credential, MemorySecrets, SecretStore, SignIn, SignInConfig, SignInError, TokenSource,
};
use eos_agent_oauth::{now_ms, pkce_challenge};
use serde_json::{Value, json};

/// Claude's token and profile endpoints, as far as a sign-in uses them.
#[derive(Default)]
struct Server {
    /// Authorization codes handed out, with the PKCE challenge and redirect they were issued for.
    codes: Mutex<HashMap<String, (String, String)>>,
    /// Refresh tokens still valid (each works once).
    refresh_tokens: Mutex<Vec<String>>,
    exchanges: Mutex<Vec<Value>>,
    refreshes: Mutex<u32>,
}

async fn token(State(server): State<Arc<Server>>, headers: HeaderMap, body: String) -> (StatusCode, Json<Value>) {
    let form = headers.get("content-type").is_some_and(|v| v.as_bytes().starts_with(b"application/x-www-form"));
    let request: Value = if form {
        let pairs: HashMap<String, String> =
            reqwest::Url::parse(&format!("http://x/?{body}")).expect("form").query_pairs().into_owned().collect();
        json!(pairs)
    } else {
        serde_json::from_str(&body).expect("json body")
    };
    let refresh = request["grant_type"] == "refresh_token";
    let answer = server.answer(&request);
    if refresh {
        // Slow enough for another holder to line up behind the lock.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    answer
}

impl Server {
    fn issue(&self, n: u32) -> Value {
        let refresh = format!("refresh-{n}");
        self.refresh_tokens.lock().expect("lock").push(refresh.clone());
        json!({"access_token": format!("access-{n}"), "refresh_token": refresh, "expires_in": 3600,
               "scope": "user:profile user:inference",
               "account": {"uuid": "acc-1", "email_address": "ada@example.com"},
               "organization": {"uuid": "org-1", "name": "Ada's Org"}})
    }

    fn answer(&self, request: &Value) -> (StatusCode, Json<Value>) {
        match request["grant_type"].as_str() {
            Some("authorization_code") => {
                self.exchanges.lock().expect("lock").push(request.clone());
                let code = request["code"].as_str().unwrap_or_default();
                let Some((challenge, redirect)) = self.codes.lock().expect("lock").remove(code) else {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": "invalid_grant"})));
                };
                let verified = pkce_challenge(request["code_verifier"].as_str().unwrap_or_default()) == challenge
                    && request["redirect_uri"] == json!(redirect)
                    && request["client_id"] == "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
                if !verified {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_request"})));
                }
                (StatusCode::OK, Json(self.issue(1)))
            }
            Some("refresh_token") => {
                let presented = request["refresh_token"].as_str().unwrap_or_default();
                let mut valid = self.refresh_tokens.lock().expect("lock");
                let Some(at) = valid.iter().position(|t| t == presented) else {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
                };
                valid.remove(at);
                drop(valid);
                let mut refreshes = self.refreshes.lock().expect("lock");
                *refreshes += 1;
                let n = *refreshes + 1;
                drop(refreshes);
                (StatusCode::OK, Json(self.issue(n)))
            }
            _ => (StatusCode::BAD_REQUEST, Json(json!({"error": "unsupported_grant_type"}))),
        }
    }
}

async fn profile(headers: HeaderMap) -> (StatusCode, Json<Value>) {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer access-1") {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let body = json!({
        "account": {"uuid": "acc-1", "email": "ada@example.com", "display_name": "Ada"},
        "organization": {"uuid": "org-1", "name": "Ada's Org", "organization_type": "claude_max",
                         "rate_limit_tier": "default_claude_max_20x"},
    });
    (StatusCode::OK, Json(body))
}

async fn serve() -> (Arc<Server>, SignInConfig) {
    let server = Arc::new(Server::default());
    let app = Router::new()
        .route("/v1/oauth/token", post(token))
        .route("/api/oauth/profile", get(profile))
        .with_state(Arc::clone(&server));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let config = SignInConfig {
        authorize_url: format!("{base}/oauth/authorize"),
        token_url: format!("{base}/v1/oauth/token"),
        profile_url: format!("{base}/api/oauth/profile"),
        manual_redirect_url: format!("{base}/oauth/code/callback"),
        success_url: format!("{base}/oauth/code/success?app=claude-code"),
        timeout: Duration::from_secs(10),
        ..SignInConfig::default()
    };
    (server, config)
}

/// What the authorization page does once the user agrees: issue a code for
/// the request's challenge and redirect. Returns the code, state and redirect.
fn authorize(server: &Server, url: &str, code: &str) -> (String, String) {
    let url = reqwest::Url::parse(url).expect("url");
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["code"], "true");
    assert_eq!(q["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("user:inference") && q["scope"].contains("user:profile"));
    server
        .codes
        .lock()
        .expect("lock")
        .insert(code.to_owned(), (q["code_challenge"].clone(), q["redirect_uri"].clone()));
    (q["state"].clone(), q["redirect_uri"].clone())
}

fn login(dir: &std::path::Path, secrets: &Arc<MemorySecrets>, config: &SignInConfig) -> ClaudeLogin {
    ClaudeLogin::new(Arc::clone(secrets) as Arc<dyn SecretStore>, dir.to_path_buf()).with_token_url(&config.token_url)
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("eos-claude-sign-in-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[tokio::test]
async fn the_browser_comes_back_and_the_store_holds_a_sign_in_claude_code_can_read() {
    let (server, config) = serve().await;
    let dir = scratch("browser");
    let secrets = Arc::new(MemorySecrets::default());
    // What was there before (an MCP sign-in) stays.
    secrets.write(r#"{"mcpOAuth":{"github":{"accessToken":"keep"}}}"#).await.expect("seed");
    let login = login(&dir, &secrets, &config);

    let sign_in =
        SignIn::start(SignInConfig { login_hint: Some("ada@example.com".into()), ..config }).await.expect("start");
    assert!(sign_in.browser_url().contains("login_hint=ada%40example.com"));
    let (state, redirect) = authorize(&server, sign_in.browser_url(), "code-1");
    assert!(redirect.starts_with("http://localhost:") && redirect.ends_with("/callback"));
    let browser = tokio::spawn(async move {
        let page = reqwest::get(format!("{redirect}?code=code-1&state={state}")).await.expect("callback");
        page.text().await.expect("page")
    });
    let account = sign_in.finish(&login, std::future::pending()).await.expect("signed in");
    assert!(browser.await.expect("join").contains("/oauth/code/success?app=claude-code"));

    assert_eq!(account.email.as_deref(), Some("ada@example.com"));
    assert_eq!(account.display_name.as_deref(), Some("Ada"));
    assert_eq!(account.organization_uuid.as_deref(), Some("org-1"));
    assert_eq!(account.subscription_type.as_deref(), Some("max"));
    assert_eq!(account.scopes, ["user:profile", "user:inference"]);
    let exchange = server.exchanges.lock().expect("lock")[0].clone();
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert!(exchange["state"].is_string(), "Claude Code sends the state with the exchange");

    // The document Claude Code keeps: its `claudeAiOauth` member.
    let doc: Value = serde_json::from_str(&secrets.read().await.expect("read").expect("doc")).expect("json");
    let stored = &doc["claudeAiOauth"];
    assert_eq!(stored["accessToken"], "access-1");
    assert_eq!(stored["refreshToken"], "refresh-1");
    assert!(stored["expiresAt"].as_u64().expect("ms") > now_ms() + 3_000_000);
    assert_eq!(stored["scopes"], json!(["user:profile", "user:inference"]));
    assert_eq!(stored["subscriptionType"], "max");
    assert_eq!(stored["rateLimitTier"], "default_claude_max_20x");
    assert_eq!(doc["mcpOAuth"]["github"]["accessToken"], "keep");
    // The reader that renews Claude Code's sign-ins takes it as it is.
    assert_eq!(login.credential().await.expect("credential"), Credential::Bearer("access-1".into()));

    assert!(login.sign_out().await.expect("sign out"));
    let doc: Value = serde_json::from_str(&secrets.read().await.expect("read").expect("doc")).expect("json");
    assert!(doc.get("claudeAiOauth").is_none());
    assert_eq!(doc["mcpOAuth"]["github"]["accessToken"], "keep");
    assert!(login.credential().await.is_err(), "signed out");
    assert!(!login.sign_out().await.expect("again"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_pasted_code_signs_in_and_a_foreign_one_does_not() {
    let (server, config) = serve().await;
    let dir = scratch("paste");
    let secrets = Arc::new(MemorySecrets::default());
    let login = login(&dir, &secrets, &config);

    let sign_in = SignIn::start(config.clone()).await.expect("start");
    let (state, redirect) = authorize(&server, sign_in.manual_url(), "code-2");
    assert_eq!(redirect, config.manual_redirect_url);
    let pasted = format!("code-2#{state}\n");
    let account = sign_in.finish(&login, async { Some(pasted) }).await.expect("signed in");
    assert_eq!(account.email.as_deref(), Some("ada@example.com"));
    assert_eq!(server.exchanges.lock().expect("lock")[0]["redirect_uri"], json!(config.manual_redirect_url));

    let other = SignIn::start(config.clone()).await.expect("start");
    let refused = other.finish(&login, async { Some("code-3#someone-elses-state".to_owned()) }).await;
    assert!(matches!(refused, Err(SignInError::Failed(why)) if why.contains("not from this sign-in")));
    // A code the server does not know.
    let unknown = SignIn::start(config).await.expect("start");
    let state = reqwest::Url::parse(unknown.manual_url())
        .expect("url")
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state");
    let refused = unknown.finish(&login, async move { Some(format!("nope#{state}")) }).await;
    assert!(matches!(refused, Err(SignInError::Failed(why)) if why.contains("invalid authorization code")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn nobody_answering_times_out() {
    let (_server, config) = serve().await;
    let dir = scratch("timeout");
    let login = login(&dir, &Arc::new(MemorySecrets::default()), &config);
    let sign_in = SignIn::start(SignInConfig { timeout: Duration::from_millis(50), ..config }).await.expect("start");
    assert!(matches!(sign_in.finish(&login, async { None }).await, Err(SignInError::Timeout)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn holders_of_one_store_renew_it_once() {
    let (server, config) = serve().await;
    let dir = scratch("shared");
    let secrets = Arc::new(MemorySecrets::default());
    server.refresh_tokens.lock().expect("lock").push("refresh-0".into());
    let expired = json!({"claudeAiOauth": {"accessToken": "access-0", "refreshToken": "refresh-0",
                                           "expiresAt": now_ms() - 1000, "scopes": ["user:inference"]}});
    secrets.write(&expired.to_string()).await.expect("seed");

    // Several agents, each with its own handle on the same store (as separate
    // processes have): the refresh token works once, so only one may spend it.
    let holders: Vec<ClaudeLogin> = (0..4).map(|_| login(&dir, &secrets, &config)).collect();
    let got = futures_util::future::join_all(holders.iter().map(|h| h.credential())).await;
    for credential in got {
        assert_eq!(credential.expect("renewed"), Credential::Bearer("access-2".into()));
    }
    assert_eq!(*server.refreshes.lock().expect("lock"), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
