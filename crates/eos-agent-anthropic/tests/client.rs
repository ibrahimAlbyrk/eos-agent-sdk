use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use eos_agent_anthropic::auth::{ClaudeLogin, Credential, FileSecrets, StaticCredential, TokenSource};
use eos_agent_anthropic::{AnthropicClient, ClientConfig};
use eos_agent_core::{BoxFuture, ModelClient, ModelError, ModelRequest};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_types::{Delta, StreamEvent};
use futures_util::StreamExt;

fn request() -> ModelRequest {
    ModelRequest {
        model: "mock-model".into(),
        max_tokens: 64,
        system: Bytes::from_static(br#"[{"type":"text","text":"s"}]"#),
        tools: None,
        messages: vec![Bytes::from_static(br#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#)],
        thinking: None,
        effort: None,
        extra: None,
        cache_key: None,
        betas: &[],
        auth: None,
    }
}

fn client(mock: &MockApi, auth: Arc<dyn TokenSource>) -> AnthropicClient {
    AnthropicClient::new(auth, ClientConfig { base_url: mock.base_url(), ..ClientConfig::default() }).expect("client")
}

fn key() -> Arc<dyn TokenSource> {
    Arc::new(StaticCredential(Credential::ApiKey("test-key".into())))
}

async fn collect(client: &AnthropicClient) -> Result<Vec<StreamEvent>, ModelError> {
    let req = request();
    let mut stream = client.stream(&req).await?;
    let mut out = Vec::new();
    while let Some(e) = stream.next().await {
        out.push(e?);
    }
    Ok(out)
}

#[tokio::test]
async fn streams_events_and_sends_the_right_headers() {
    let mock = MockApi::start([Reply::text("hello")]).await.expect("mock");
    let events = collect(&client(&mock, key())).await.expect("stream");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockDelta { delta: Delta::Text(t), .. } if t == "hello"))
    );
    let req = &mock.requests()[0];
    assert_eq!(req.headers["x-api-key"], "test-key");
    assert_eq!(req.headers["anthropic-version"], "2023-06-01");
    assert!(!req.headers["anthropic-beta"].to_str().expect("ascii").contains("oauth"));
    assert_eq!(req.json()["stream"], true);
    assert_eq!(req.headers["content-length"], req.body.len().to_string().as_str(), "sent with its length, not chunked");
}

#[tokio::test]
async fn a_request_adds_its_own_betas() {
    let mock = MockApi::start([Reply::text("x")]).await.expect("mock");
    let mut req = request();
    req.betas = &["mid-conversation-tool-changes-2026-07-01", "inline-tools-2026-09-15"];
    let mut stream = client(&mock, key()).stream(&req).await.expect("stream");
    while stream.next().await.is_some() {}
    let betas = mock.requests()[0].headers["anthropic-beta"].to_str().expect("ascii").to_owned();
    assert!(betas.ends_with(",mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15"), "{betas}");
}

#[tokio::test]
async fn bearer_tokens_add_the_oauth_beta() {
    let mock = MockApi::start([Reply::text("x")]).await.expect("mock");
    let auth = Arc::new(StaticCredential(Credential::Bearer("tok".into())));
    collect(&client(&mock, auth)).await.expect("stream");
    let req = &mock.requests()[0];
    assert_eq!(req.headers["authorization"], "Bearer tok");
    assert!(req.headers["anthropic-beta"].to_str().expect("ascii").contains("oauth-2025-04-20"));
}

#[tokio::test]
async fn maps_http_errors_and_broken_streams() {
    let mock = MockApi::start([Reply::rate_limited(2), Reply::overloaded(), Reply::text("cut").dropped_after(3)])
        .await
        .expect("mock");
    let c = client(&mock, key());
    assert!(matches!(collect(&c).await, Err(ModelError::RateLimited { .. })));
    assert!(matches!(collect(&c).await, Err(ModelError::Overloaded { .. })));
    assert!(matches!(collect(&c).await, Err(ModelError::Transient(_))));
}

#[tokio::test]
async fn a_prompt_over_the_context_window_is_its_own_error() {
    let in_stream = concat!(
        "event: error\n",
        r#"data: {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
        "\n\n"
    );
    let mock = MockApi::start([Reply::prompt_too_long(), Reply::sse(in_stream)]).await.expect("mock");
    let c = client(&mock, key());
    for how in ["refused", "mid-stream"] {
        let error = collect(&c).await.expect_err(how);
        assert!(
            matches!(&error, ModelError::ContextWindowExceeded(m) if m.contains("210000 tokens")),
            "{how}: {error:?}"
        );
        assert!(!error.is_retryable());
    }
}

/// Takes requests and never answers, as a connection that died unnoticed.
async fn silent_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    url
}

#[tokio::test]
async fn a_response_that_never_starts_is_stalled() {
    let config = ClientConfig {
        base_url: silent_server().await,
        idle_timeout: std::time::Duration::from_millis(200),
        ..ClientConfig::default()
    };
    let c = AnthropicClient::new(key(), config).expect("client");
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), collect(&c)).await.expect("bounded");
    let error = out.expect_err("no answer");
    assert!(matches!(error, ModelError::Stalled), "{error:?}");
    assert!(error.is_retryable());
}

/// Hands out "old" first; after a refusal, "new".
#[derive(Default)]
struct Rotating(AtomicU32);
impl TokenSource for Rotating {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        let token = if self.0.load(Ordering::SeqCst) == 0 { "old" } else { "new" };
        Box::pin(async move { Ok(Credential::Bearer(token.into())) })
    }
    fn refused<'a>(&'a self, _: &'a Credential) -> BoxFuture<'a, bool> {
        self.0.store(1, Ordering::SeqCst);
        Box::pin(async { true })
    }
}

#[tokio::test]
async fn a_refused_token_is_renewed_once() {
    let unauthorized = Reply::Status { status: 401, headers: vec![], body: "{}".into() };
    let mock = MockApi::start([unauthorized, Reply::text("ok")]).await.expect("mock");
    collect(&client(&mock, Arc::new(Rotating::default()))).await.expect("stream after renewal");
    let reqs = mock.requests();
    assert_eq!(reqs[0].headers["authorization"], "Bearer old");
    assert_eq!(reqs[1].headers["authorization"], "Bearer new");
}

async fn token_server(hits: Arc<AtomicU32>) -> String {
    use axum::routing::post;
    let app = axum::Router::new().route(
        "/token",
        post(move |body: String| {
            let hits = hits.clone();
            async move {
                assert!(body.contains("grant_type=refresh_token") && body.contains("refresh_token=r1"));
                hits.fetch_add(1, Ordering::SeqCst);
                axum::Json(serde_json::json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 3600}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}/token", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}

#[tokio::test]
async fn expired_claude_sign_in_is_renewed_under_the_lock() {
    let dir = std::env::temp_dir().join(format!("eos-agent-auth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let creds = dir.join("creds.json");
    let doc = serde_json::json!({
        "claudeAiOauth": {"accessToken": "a1", "refreshToken": "r1", "expiresAt": 1000, "scopes": ["user:inference"]},
        "other": "kept"
    });
    std::fs::write(&creds, doc.to_string()).expect("write");

    let hits = Arc::new(AtomicU32::new(0));
    let login = Arc::new(
        ClaudeLogin::new(Arc::new(FileSecrets::new(&creds)), dir.clone())
            .with_token_url(token_server(hits.clone()).await),
    );
    // Several agents ask at once: exactly one renewal may happen.
    let asks = (0..8).map(|_| {
        let login = login.clone();
        tokio::spawn(async move { login.credential().await })
    });
    for ask in asks {
        assert_eq!(ask.await.expect("join").expect("credential"), Credential::Bearer("a2".into()));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&creds).expect("read")).expect("json");
    assert_eq!(saved["claudeAiOauth"]["refreshToken"], "r2");
    assert_eq!(saved["claudeAiOauth"]["scopes"][0], "user:inference");
    assert_eq!(saved["other"], "kept");
    assert!(!dir.join(".oauth_refresh.lock").exists(), "lock released");
    let _ = std::fs::remove_dir_all(&dir);
}
