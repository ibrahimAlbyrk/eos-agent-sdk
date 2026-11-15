//! The client against the mock Responses endpoint: streaming, a tool call's
//! round trip with its reasoning carried over, errors and plan limits, a
//! renewal after a 401, the stream watchdog.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use eos_agent_core::{
    Assembled, Assembler, BoxFuture, Credential, ModelClient, ModelError, ModelRequest, StaticCredential,
    StoredMessage, SystemPrompt, Thinking, TokenSource,
};
use eos_agent_mock_api::{MockApi, Output, Recorded, Reply};
use eos_agent_openai::{ClientConfig, OpenAiClient};
use eos_agent_types::{ContentBlock, Role, StopReason, ToolResultContent};
use futures_util::StreamExt;
use serde_json::{Value, json};

fn client(mock: &MockApi, auth: Arc<dyn TokenSource>) -> OpenAiClient {
    let config = ClientConfig { base_url: format!("{}/v1", mock.base_url()), ..ClientConfig::default() };
    OpenAiClient::new(auth, config).expect("client")
}

fn bearer(token: &str) -> Arc<dyn TokenSource> {
    Arc::new(StaticCredential(Credential::Bearer(token.into())))
}

fn request(messages: &[StoredMessage]) -> ModelRequest {
    ModelRequest {
        model: "gpt-5.5".into(),
        max_tokens: 128_000,
        system: SystemPrompt::new(&[("You are Eos.", None)]).json().clone(),
        tools: Some(Bytes::from_static(
            br#"[{"name":"Read","description":"Read a file","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]}}]"#,
        )),
        messages: messages.iter().map(|m| m.json().clone()).collect(),
        thinking: Some(Thinking::Adaptive { summarized: true }),
        effort: Some("medium".into()),
        extra: None,
        cache_key: Some("agent-1".into()),
        betas: &[],
        auth: None,
    }
}

async fn run(client: &OpenAiClient, req: &ModelRequest) -> Result<Assembled, ModelError> {
    let mut events = client.stream(req).await?;
    let mut a = Assembler::default();
    while let Some(event) = events.next().await {
        a.push(event?);
    }
    Ok(a.finish())
}

/// What every Sign in with ChatGPT request must look like.
fn check_body(r: &Recorded) -> Value {
    assert_eq!(r.path, "/v1/responses");
    let v = r.json();
    assert_eq!(v["store"], false);
    assert_eq!(v["stream"], true);
    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
    for unsupported in [
        "background",
        "conversation",
        "max_output_tokens",
        "max_tool_calls",
        "metadata",
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "temperature",
        "top_p",
        "truncation",
        "user",
    ] {
        assert!(v.get(unsupported).is_none(), "{unsupported} is not supported on a ChatGPT plan");
    }
    let items = v["input"].as_array().expect("input");
    assert!(items.iter().all(|i| i["role"] != "system"), "system items are rejected");
    v
}

#[tokio::test]
async fn streams_text_with_usage() {
    let mock = MockApi::start([Reply::responses(&[Output::text("Hello there")], (1200, 1000, 7))]).await.expect("mock");
    let client = client(&mock, bearer("tok"));
    let done = run(&client, &request(&[StoredMessage::user_text("hi")])).await.expect("turn");
    assert!(done.complete);
    assert_eq!(done.stop_reason, Some(StopReason::EndTurn));
    assert!(matches!(&done.content[..], [ContentBlock::Text { text, .. }] if text == "Hello there"));
    assert_eq!((done.usage.input_tokens, done.usage.cache_read_input_tokens), (Some(200), Some(1000)));
    let sent = &mock.requests()[0];
    assert_eq!(sent.headers["authorization"], "Bearer tok");
    let body = check_body(sent);
    assert_eq!(body["instructions"], "You are Eos.");
    assert_eq!(body["prompt_cache_key"], "agent-1");
    assert_eq!(body["tools"][0]["type"], "namespace");
}

#[tokio::test]
async fn a_tool_call_round_trips_with_its_reasoning() {
    let args = r#"{"file_path":"/tmp/a b.txt"}"#;
    let mock = MockApi::start([
        Reply::responses(
            &[Output::reasoning("rs_1", "Need the file.", "ENC_1+/="), Output::call("call_1", "Read", args)],
            (50, 0, 9),
        ),
        Reply::responses(&[Output::reasoning("rs_2", "", "ENC_2"), Output::text("It says hi.")], (80, 50, 4)),
    ])
    .await
    .expect("mock");
    let client = client(&mock, bearer("tok"));
    let user = StoredMessage::user_text("what is in /tmp/a b.txt?");
    let first = run(&client, &request(std::slice::from_ref(&user))).await.expect("first");
    assert_eq!(first.stop_reason, Some(StopReason::ToolUse));
    let ContentBlock::ToolUse { input, .. } = &first.content[1] else { panic!("a call") };
    assert_eq!(input.get(), args, "arguments streamed in odd pieces come out whole");

    let assistant = StoredMessage::new(Role::Assistant, &first.content).expect("message");
    let result = StoredMessage::new(
        Role::User,
        &[ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: Some(ToolResultContent::Text("hi".into())),
            is_error: false,
        }],
    )
    .expect("message");
    let second = run(&client, &request(&[user, assistant, result])).await.expect("second");
    assert!(matches!(&second.content[1], ContentBlock::Text { text, .. } if text == "It says hi."));

    let body = check_body(&mock.requests()[1]);
    let items = body["input"].as_array().expect("input");
    let kinds: Vec<&str> = items.iter().filter_map(|i| i["type"].as_str()).collect();
    assert_eq!(kinds, ["message", "reasoning", "function_call", "function_call_output"]);
    assert_eq!(items[1]["id"], "rs_1");
    assert_eq!(items[1]["encrypted_content"], "ENC_1+/=", "reasoning goes back verbatim");
    assert_eq!(items[1]["summary"][0]["text"], "Need the file.");
    assert_eq!(items[2]["call_id"], "call_1");
    assert_eq!(items[2]["arguments"], args);
    assert_eq!(items[3], json!({"type": "function_call_output", "call_id": "call_1", "output": "hi"}));
}

/// A ChatGPT plan reads the cache only for requests that carry a session
/// header (live: 0 cached tokens without it, the whole prefix with it), and
/// only for a prefix that is byte for byte the same.
#[tokio::test]
async fn requests_of_a_conversation_can_reach_its_prompt_cache() {
    let mock = MockApi::start([
        Reply::responses(&[Output::call("call_1", "Read", r#"{"file_path":"a"}"#)], (5000, 0, 9)),
        Reply::responses(&[Output::text("done")], (5100, 4992, 2)),
    ])
    .await
    .expect("mock");
    let client = client(&mock, bearer("tok"));
    let user = StoredMessage::user_text("read a");
    let first = run(&client, &request(std::slice::from_ref(&user))).await.expect("first");
    let assistant = StoredMessage::new(Role::Assistant, &first.content).expect("message");
    let result = StoredMessage::new(
        Role::User,
        &[ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: Some(ToolResultContent::Text("hi".into())),
            is_error: false,
        }],
    )
    .expect("message");
    let second = run(&client, &request(&[user, assistant, result])).await.expect("second");
    assert_eq!(second.usage.cache_read_input_tokens, Some(4992), "cached input is read back");
    assert_eq!(second.usage.input_tokens, Some(108));

    let sent = mock.requests();
    for r in &sent {
        assert_eq!(r.headers["session-id"], "agent-1", "the session header is the cache key");
        assert_eq!(check_body(r)["prompt_cache_key"], "agent-1");
    }
    // Everything up to the end of the first request's input comes again,
    // byte for byte, at the start of the second.
    let (a, b) = (&sent[0].body[..], &sent[1].body[..]);
    let marker = br#"],"tools":"#;
    let input_end = a.windows(marker.len()).position(|w| w == marker).expect("input then tools");
    assert_eq!(&a[..input_end], &b[..input_end], "the prefix is byte-stable");
    assert_eq!(b[input_end], b',', "the second request appends to the input");

    // No cache key, no header.
    let mock = MockApi::start([Reply::responses_text("ok")]).await.expect("mock");
    let client = self::client(&mock, bearer("tok"));
    let mut req = request(&[StoredMessage::user_text("hi")]);
    req.cache_key = None;
    run(&client, &req).await.expect("turn");
    assert!(mock.requests()[0].headers.get("session-id").is_none());
}

#[tokio::test]
async fn plan_limits_and_errors_map_to_model_errors() {
    let mock = MockApi::start([
        Reply::responses_error(429, "subscription_sharing_usage_limit_exceeded", "limit"),
        Reply::responses_failed("subscription_sharing_usage_limit_exceeded", "limit mid-stream"),
        Reply::responses_error(400, "context_length_exceeded", "too long"),
        Reply::responses_error(403, "subscription_sharing_user_not_eligible", "no"),
        Reply::responses_error(503, "subscription_sharing_usage_unavailable", "later"),
        Reply::Status {
            status: 429,
            headers: vec![("retry-after".into(), "2".into())],
            body: r#"{"error":{"code":"rate_limit_exceeded","message":"slow down"}}"#.into(),
        },
        Reply::responses(&[Output::text("par")], (1, 0, 1)).dropped_after(3),
    ])
    .await
    .expect("mock");
    let client = client(&mock, bearer("tok"));
    let req = request(&[StoredMessage::user_text("hi")]);
    let limit = run(&client, &req).await.expect_err("limit");
    assert!(matches!(&limit, ModelError::LimitReached { info } if info.resets_at.is_none()), "{limit:?}");
    assert!(!limit.is_retryable());
    assert!(matches!(run(&client, &req).await, Err(ModelError::LimitReached { .. })), "mid-stream too");
    assert!(matches!(run(&client, &req).await, Err(ModelError::ContextWindowExceeded(_))));
    assert!(matches!(run(&client, &req).await, Err(ModelError::Auth(_))));
    let unavailable = run(&client, &req).await.expect_err("503");
    assert!(unavailable.is_retryable(), "{unavailable:?}");
    let slow = run(&client, &req).await.expect_err("429");
    assert_eq!(slow.retry_after(), Some(Duration::from_secs(2)));
    let cut = run(&client, &req).await.expect_err("cut");
    assert!(cut.is_retryable(), "a stream that breaks is retried: {cut:?}");
}

/// Refuses the first token and hands out a second one.
#[derive(Default)]
struct Rotating {
    renewed: AtomicU32,
}

impl TokenSource for Rotating {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        let n = self.renewed.load(Ordering::SeqCst);
        Box::pin(async move { Ok(Credential::Bearer(format!("t{n}"))) })
    }

    fn refused<'a>(&'a self, _rejected: &'a Credential) -> BoxFuture<'a, bool> {
        self.renewed.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { true })
    }
}

#[tokio::test]
async fn a_refused_token_is_renewed_once() {
    let mock = MockApi::start([
        Reply::Status { status: 401, headers: Vec::new(), body: r#"{"detail":"token expired"}"#.into() },
        Reply::responses_text("ok"),
    ])
    .await
    .expect("mock");
    let auth = Arc::new(Rotating::default());
    let client = client(&mock, auth.clone());
    let done = run(&client, &request(&[StoredMessage::user_text("hi")])).await.expect("renewed and answered");
    assert!(matches!(&done.content[..], [ContentBlock::Text { text, .. }] if text == "ok"));
    let sent = mock.requests();
    assert_eq!(
        (sent[0].headers["authorization"].to_str().ok(), sent[1].headers["authorization"].to_str().ok()),
        (Some("Bearer t0"), Some("Bearer t1"))
    );
    assert_eq!(auth.renewed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_silent_stream_is_given_up() {
    let mock = MockApi::start([Reply::responses_text("late").slowed(Duration::from_millis(400))]).await.expect("mock");
    let config = ClientConfig {
        base_url: format!("{}/v1", mock.base_url()),
        idle_timeout: Duration::from_millis(100),
        ..ClientConfig::default()
    };
    let client = OpenAiClient::new(bearer("tok"), config).expect("client");
    let err = run(&client, &request(&[StoredMessage::user_text("hi")])).await.expect_err("stalled");
    assert!(matches!(err, ModelError::Stalled));
}

#[tokio::test]
async fn a_response_that_never_starts_is_stalled() {
    // Takes the request and never answers, as a connection that died unnoticed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base_url = format!("http://{}/v1", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let config = ClientConfig { base_url, idle_timeout: Duration::from_millis(200), ..ClientConfig::default() };
    let client = OpenAiClient::new(bearer("tok"), config).expect("client");
    let req = request(&[StoredMessage::user_text("hi")]);
    let out = tokio::time::timeout(Duration::from_secs(5), run(&client, &req)).await.expect("bounded");
    assert!(matches!(out, Err(ModelError::Stalled)), "{out:?}");
}

#[tokio::test]
async fn the_catalog_lists_the_accounts_models() {
    let mock = MockApi::start([]).await.expect("mock");
    mock.set_models(json!({"models": [{"slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list"}]}));
    let client = Arc::new(client(&mock, bearer("tok")));
    let dir = std::env::temp_dir().join(format!("eos-openai-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let catalog = eos_agent_openai::ModelCatalog::new(Arc::clone(&client), Some(&dir));
    assert!(catalog.snapshot().models.is_empty());
    let fetched = catalog.ensure().await;
    assert_eq!(fetched.default_model().map(|m| m.slug.as_str()), Some("gpt-6-sol"));
    let reloaded = eos_agent_openai::ModelCatalog::new(client, Some(&dir));
    assert_eq!(reloaded.snapshot().models.len(), 1, "kept on disk");
    let _ = std::fs::remove_dir_all(&dir);
}
