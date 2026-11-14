mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use common::{Agent, FakeModel, prompt_of, reply, text_of, text_reply};
use eos_agent_tools_web::{FetchCache, WebFetchConfig, WebFetchTool};
use serde_json::json;

#[derive(Clone, Default)]
struct Hits(Arc<AtomicUsize>);

const PAGE: &str = "<html><head><title>T</title><script>evil()</script></head>\
<body><h1>Install</h1><p>Run <code>cargo add eos</code>.</p></body></html>";

async fn serve() -> (SocketAddr, Hits) {
    let hits = Hits::default();
    let app = Router::new()
        .route(
            "/page",
            get(|State(h): State<Hits>| async move {
                h.0.fetch_add(1, Ordering::SeqCst);
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE)
            }),
        )
        .route("/moved", get(|| async { redirect("/page") }))
        .route("/away", get(|| async { redirect("http://example.org/elsewhere") }))
        .route("/loop", get(|| async { redirect("/loop") }))
        .route("/big", get(|| async { "x".repeat(4096) }))
        .route("/long", get(|| async { "word ".repeat(30_000) }))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    (addr, hits)
}

fn redirect(to: &str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::LOCATION, to.parse().expect("header"));
    (StatusCode::FOUND, headers).into_response()
}

fn config() -> WebFetchConfig {
    WebFetchConfig { https_upgrade: false, ..WebFetchConfig::default() }
}

fn tool(model: &Arc<FakeModel>, config: WebFetchConfig) -> WebFetchTool {
    WebFetchTool::new(model.clone(), config)
}

#[tokio::test]
async fn converts_html_and_asks_the_small_model() {
    let (addr, _) = serve().await;
    let model = Arc::new(FakeModel::new([text_reply("Run cargo add eos.")]));
    let out = Agent::new("a")
        .call(&tool(&model, config()), json!({"url": format!("http://{addr}/page"), "prompt": "How to install?"}))
        .await;
    assert!(!out.is_error, "{}", text_of(&out));
    assert_eq!(text_of(&out), "Run cargo add eos.");

    let req = model.request(0);
    assert_eq!(req.model, "claude-haiku-5-5");
    let prompt = prompt_of(&req);
    assert!(prompt.starts_with("\nWeb page content:\n---\n"), "{prompt}");
    assert!(prompt.contains("# Install"), "{prompt}");
    assert!(prompt.contains("`cargo add eos`"));
    assert!(!prompt.contains("evil"));
    assert!(prompt.contains("How to install?\n\nProvide a concise response based only on the content above."));
}

/// Haiku 5.5 always thinks: WebFetch's prompt gets `[thinking, text]`, and
/// the text may come in more than one block.
#[tokio::test]
async fn reads_the_answer_after_the_side_model_thinks() {
    let (addr, _) = serve().await;
    let answer = reply(&[
        (
            r#"{"type":"thinking","thinking":"","signature":""}"#,
            Some(r#"{"type":"thinking_delta","thinking":"The page says to run cargo add."}"#),
        ),
        (r#"{"type":"text","text":""}"#, Some(r#"{"type":"text_delta","text":"Run "}"#)),
        (r#"{"type":"text","text":""}"#, Some(r#"{"type":"text_delta","text":"cargo add eos."}"#)),
    ]);
    let model = Arc::new(FakeModel::new([answer]));
    let mut config = config();
    config.side.always_thinks = true;
    let out = Agent::new("a")
        .call(&tool(&model, config), json!({"url": format!("http://{addr}/page"), "prompt": "How?"}))
        .await;
    assert!(!out.is_error, "{}", text_of(&out));
    assert_eq!(text_of(&out), "Run cargo add eos.");
}

#[tokio::test]
async fn cache_is_shared_across_agents_and_tools() {
    let (addr, hits) = serve().await;
    let model = Arc::new(FakeModel::new([text_reply("one"), text_reply("two")]));
    let cache = Arc::new(FetchCache::default());
    let first = tool(&model, WebFetchConfig { cache: Arc::clone(&cache), ..config() });
    let second = tool(&model, WebFetchConfig { cache: Arc::clone(&cache), ..config() });
    let url = format!("http://{addr}/page");
    Agent::new("a").call(&first, json!({"url": url, "prompt": "p1"})).await;
    let out = Agent::new("b").call(&second, json!({"url": url, "prompt": "p2"})).await;
    assert_eq!(text_of(&out), "two");
    assert_eq!(hits.0.load(Ordering::SeqCst), 1);
    assert_eq!(cache.len(), 1);
}

#[tokio::test]
async fn follows_same_site_redirects_only() {
    let (addr, hits) = serve().await;
    let model = Arc::new(FakeModel::new([text_reply("ok")]));
    let fetch = tool(&model, config());
    let agent = Agent::new("a");
    let out = agent.call(&fetch, json!({"url": format!("http://{addr}/moved"), "prompt": "p"})).await;
    assert_eq!(text_of(&out), "ok");
    assert_eq!(hits.0.load(Ordering::SeqCst), 1);

    let out =
        agent.call(&fetch, json!({"url": format!("http://{addr}/away"), "prompt": "summarize", "offset": 5})).await;
    let text = text_of(&out);
    assert!(!out.is_error);
    assert!(text.starts_with("REDIRECT DETECTED: The URL redirects to a location that was not fetched automatically."));
    assert!(text.contains("Redirect URL (from the server's Location header"));
    assert!(text.contains("- url: \"http://example.org/elsewhere\"\n    - prompt: \"summarize\"\n    - offset: 5"));
    assert!(text.contains("Status: 302 Found"));

    let out = agent.call(&fetch, json!({"url": format!("http://{addr}/loop"), "prompt": "p"})).await;
    assert!(out.is_error);
    assert_eq!(text_of(&out), "Too many redirects (exceeded 10)");
}

#[tokio::test]
async fn caps_the_body_size() {
    let (addr, _) = serve().await;
    let model = Arc::new(FakeModel::default());
    let fetch = tool(&model, WebFetchConfig { max_body: 1024, ..config() });
    let out = Agent::new("a").call(&fetch, json!({"url": format!("http://{addr}/big"), "prompt": "p"})).await;
    assert!(out.is_error);
    assert_eq!(text_of(&out), "maxContentLength size of 1024 exceeded");
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn pages_long_content_with_offset() {
    let (addr, _) = serve().await;
    let model = Arc::new(FakeModel::new([text_reply("first part"), text_reply("rest")]));
    let fetch = tool(&model, config());
    let agent = Agent::new("a");
    let url = format!("http://{addr}/long");
    let out = agent.call(&fetch, json!({"url": url, "prompt": "p"})).await;
    assert_eq!(
        text_of(&out),
        "first part\n\n[WebFetch note: this page's text is 150000 characters long and the answer above covers only \
characters 0 to 100000; the final 50000 were not read \u{2014} to read on, call WebFetch again with the same url and \
offset: 100000.]"
    );
    let out = agent.call(&fetch, json!({"url": url, "prompt": "p", "offset": "100000"})).await;
    assert_eq!(text_of(&out), "rest");
    assert!(prompt_of(&model.request(1)).starts_with(
        "The content below is one part of a longer page: it starts 100000 characters into the page's 150000.\n\nWeb page"
    ));
    let out = agent.call(&fetch, json!({"url": url, "prompt": "p", "offset": 200000})).await;
    assert_eq!(text_of(&out), "Nothing left to read from offset 200000: this page's text is 150000 characters long.");
}

#[tokio::test]
async fn short_notes_for_repeats_and_recent_failures() {
    let (addr, hits) = serve().await;
    let model = Arc::new(FakeModel::new([text_reply("answer")]));
    let fetch = tool(&model, config());
    let agent = Agent::new("a");
    let url = format!("http://{addr}/page");
    agent.call(&fetch, json!({"url": url, "prompt": "p"})).await;
    let out = agent.call(&fetch, json!({"url": url, "prompt": "p"})).await;
    assert!(text_of(&out).starts_with(&format!(
        "Already fetched {url} with the same prompt 0s ago in this conversation (tool call toolu_1)."
    )));
    assert!(text_of(&out).contains("WebFetch caches pages for 15 minutes"));

    let missing = format!("http://{addr}/missing");
    let out = agent.call(&fetch, json!({"url": missing, "prompt": "p"})).await;
    assert!(
        text_of(&out).starts_with("The server returned HTTP 404 Not Found.\n\nThe response body was not retrieved.")
    );
    let out = agent.call(&fetch, json!({"url": missing, "prompt": "p"})).await;
    assert!(
        text_of(&out).starts_with(&format!("Not fetched: {missing} failed 0s ago in this session (url_not_found)"))
    );
    // Told once: asking again fetches for real.
    let out = agent.call(&fetch, json!({"url": missing, "prompt": "p"})).await;
    assert!(text_of(&out).starts_with("The server returned HTTP 404"));
    assert_eq!(hits.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refuses_dotless_hosts_and_bad_urls() {
    let fetch = tool(&Arc::new(FakeModel::default()), WebFetchConfig::default());
    let agent = Agent::new("a");
    let out = agent.call(&fetch, json!({"url": "http://localhost:8080/x", "prompt": "p"})).await;
    assert!(out.is_error);
    assert!(text_of(&out).starts_with("WebFetch cannot fetch localhost or other hostnames without a dot."));
    let out = agent.call(&fetch, json!({"url": "nope", "prompt": "p"})).await;
    assert_eq!(
        text_of(&out),
        "<tool_use_error>Error: Invalid URL \"nope\". The URL provided could not be parsed.</tool_use_error>"
    );
    let out = agent.call(&fetch, json!({"url": "https://claude.ai/share/abc", "prompt": "p"})).await;
    assert!(text_of(&out).starts_with("https://claude.ai/share/abc is a shared claude.ai chat."));
}

#[test]
fn description_resolves_cache_ttl() {
    let fetch = tool(&Arc::new(FakeModel::default()), WebFetchConfig::default());
    let spec: serde_json::Value = serde_json::from_slice(eos_agent_core::Tool::spec(&fetch).json()).expect("json");
    let description = spec["description"].as_str().expect("str");
    assert!(description.starts_with("Fetches a URL, converts the page to markdown, and answers `prompt` against it"));
    assert!(description.ends_with("\n- Responses are cached for 15 minutes per URL."));
    assert!(!description.contains('{') && !description.contains("claude.ai artifact links"));
    assert_eq!(spec["input_schema"]["properties"]["url"]["format"], "uri");
}
