//! An agent that publishes an artifact watches it: a version published
//! elsewhere reaches its next request as a note, a comment sent to Claude
//! wakes it, and its watches end with it.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use common::{Dirs, all_text, mock};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::Reply;
use eos_agent_sdk::{AgentOptions, FrameConfig, Sdk, WatchConfig};
use serde_json::{Value, json};
use tokio::sync::broadcast;

const SLUG: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";

#[derive(Clone)]
struct Frame {
    open: Arc<AtomicUsize>,
    comments: Arc<Mutex<Value>>,
    frames: broadcast::Sender<String>,
}

async fn start_frame() -> (String, Frame) {
    let frame = Frame {
        open: Arc::default(),
        // A thread activated for Claude before the watch starts.
        comments: Arc::new(Mutex::new(json!({ "names": {}, "threads": [
            { "id": "t9", "claude_activated_at": "2026-11-08T09:00:00Z", "comments": [] }] }))),
        frames: broadcast::channel(16).0,
    };
    let app = Router::new()
        .route(
            "/api/frame/deploy/direct",
            post(|| async { axum::Json(json!({ "slug": SLUG, "version": "100-a", "seq": 1 })) }),
        )
        .route(
            &format!("/api/frame/{SLUG}"),
            get(|| async { axum::Json(json!({ "ver": "100-a", "assetToken": "at", "subscriptionToken": "sub" })) }),
        )
        .route(
            &format!("/api/frame/comments/{SLUG}"),
            get(|State(f): State<Frame>| async move { axum::Json(f.comments.lock().expect("c").clone()) }),
        )
        .route(&format!("/edge-api/frame-live/{SLUG}/ws"), get(socket))
        .with_state(frame.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, frame)
}

async fn socket(State(f): State<Frame>, ws: WebSocketUpgrade) -> Response {
    let rx = f.frames.subscribe();
    ws.on_upgrade(move |socket| serve(socket, rx, f)).into_response()
}

async fn serve(mut socket: WebSocket, mut rx: broadcast::Receiver<String>, f: Frame) {
    f.open.fetch_add(1, Ordering::SeqCst);
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(text) => if socket.send(Message::Text(text.into())).await.is_err() { break },
                Err(_) => break,
            },
            incoming = socket.recv() => if incoming.is_none_or(|m| m.is_err()) { break },
        }
    }
    f.open.fetch_sub(1, Ordering::SeqCst);
}

async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..250 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_published_artifact_is_watched_for_its_agent() {
    let dirs = Dirs::new("watch");
    dirs.write("project/p.html", "<p>hi</p>");
    let (base, frame) = start_frame().await;
    let publish = r#"{"file_path":"p.html","icon":"test"}"#;
    let model = mock([
        Reply::tool_use("toolu_p", "Artifact", publish),
        Reply::text("published"),
        Reply::text("ok"),
        Reply::text("on it"),
    ])
    .await;
    let watch = WatchConfig {
        live_origin: base.clone(),
        comment_debounce: Duration::from_millis(20),
        ..WatchConfig::default()
    };
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::Bearer("tok".into()))))
        .base_url(model.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .artifact_api(FrameConfig { base_url: base.clone(), ..FrameConfig::default() }, watch)
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("publish p.html").await.expect("alive").expect("turn");
    let published = all_text(&model.requests()[1]);
    assert!(published.contains("Live subscription: arming in the background"), "{published}");
    until("the live channel", || frame.open.load(Ordering::SeqCst) == 1).await;

    // A version from elsewhere waits for the agent's next request.
    let _ = frame.frames.send(json!({ "kind": "publish", "ver": "200-b" }).to_string());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(model.requests().len(), 2, "a republish starts no turn");
    agent.send_text("anything new?").await.expect("alive").expect("turn");
    let next = all_text(&model.requests()[2]);
    assert!(next.contains("[Version 200-b of this artifact was published after the version you have."), "{next}");

    // A comment sent to Claude wakes it: the agent publishes from a local
    // file, so the thread is handed to it (no automatic reply).
    *frame.comments.lock().expect("c") = json!({ "names": {}, "threads": [{ "id": "t9", "claude_activated_at": "2026-11-08T09:00:00Z", "comments": [
        { "id": "c1", "author": { "account": "u_1", "role": "human" }, "text": "Bigger title please",
          "to_claude_at": "2026-11-08T10:00:00Z" }
    ]}]});
    let _ = frame.frames.send(json!({ "kind": "comment" }).to_string());
    until("the woken turn", || model.requests().len() == 4).await;
    let woken = all_text(&model.requests()[3]);
    assert!(
        woken.contains("A human comment sent to Claude is waiting on thread t9")
            && woken
                .contains("No automatic reply was posted and no automatic edit was attempted: this session publishes")
            && woken.contains("p.html"),
        "{woken}"
    );

    drop(agent);
    until("the watch to end with its agent", || frame.open.load(Ordering::SeqCst) == 0).await;
}

/// Real frame API, stand-in model: the agent publishes a test page (the
/// stand-in model asks it to), a second client republishes it, and the
/// agent's next request carries the note. The page is deleted at the end.
/// `EOS_CLAUDE_STORE=~/.eos/accounts/claude cargo test -p eos-agent-sdk --test watch -- --ignored --nocapture`
#[tokio::test]
#[ignore = "publishes and deletes a real artifact"]
async fn real_watch() {
    use eos_agent_core::{CancellationToken, Extensions, ToolContext, Workspace};
    let store = std::env::var_os("EOS_CLAUDE_STORE").expect("EOS_CLAUDE_STORE");
    let login = Arc::new(eos_agent_anthropic::auth::ClaudeLogin::keychain(Some(std::path::Path::new(&store))));
    let dirs = Dirs::new("real-watch");
    dirs.write("project/p.html", "<!doctype html><title>eos-agent-sdk test — safe to delete</title><p>one</p>");
    dirs.write("project/q.html", "<!doctype html><title>eos-agent-sdk test — safe to delete</title><p>two</p>");
    let model = mock([
        Reply::tool_use("toolu_p", "Artifact", r#"{"file_path":"p.html","icon":"test"}"#),
        Reply::text("published"),
        Reply::text("ok"),
    ])
    .await;
    let sdk = Sdk::builder()
        .auth(login.clone())
        .base_url(model.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("publish p.html").await.expect("alive").expect("turn");
    let result = all_text(&model.requests()[1]);
    let url = result
        .split_whitespace()
        .find(|w| w.starts_with("https://claude.ai/artifact/"))
        .expect("published url")
        .trim_end_matches(['.', ','])
        .to_owned();
    println!("published {url}; {}", result.lines().find(|l| l.starts_with("Live subscription")).unwrap_or(""));
    tokio::time::sleep(Duration::from_secs(3)).await;

    // A second client: the artifact tools alone, the same sign-in.
    let tools = eos_agent_artifacts::tools(login, eos_agent_artifacts::FrameConfig::default()).expect("tools");
    let ext = Extensions::default();
    ext.insert(Workspace::new(dirs.project(), Vec::new(), dirs.root.join("scratch")));
    let call = |input: Value| {
        let (tools, ext) = (&tools, &ext);
        async move {
            let raw = serde_json::value::to_raw_value(&input).expect("raw");
            let cancel = CancellationToken::new();
            let ctx =
                ToolContext { agent_id: "other", tool_use_id: "t", cancel: &cancel, extensions: ext, transcript: &[] };
            match tools[0].call(&raw, ctx).await.content {
                eos_agent_types::ToolResultContent::Text(t) => t,
                eos_agent_types::ToolResultContent::Blocks(_) => String::new(),
            }
        }
    };
    call(json!({ "action": "read", "url": url })).await;
    println!(
        "second client: {}",
        call(json!({ "file_path": "q.html", "url": url })).await.lines().next().unwrap_or("")
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    agent.send_text("anything new?").await.expect("alive").expect("turn");
    let next = all_text(&model.requests()[2]);
    let deleted = call(json!({ "action": "delete", "url": url })).await;
    println!("cleanup: {deleted}");
    let note = next.lines().find(|l| l.contains("was published after the version you have")).unwrap_or("(none)");
    println!("the agent's next request carried: {note}");
    assert!(note != "(none)", "{next}");
}

/// Claude Code re-arms a resumed session's watches: an agent resumed from a
/// history that published an artifact follows its live channel again.
#[tokio::test]
async fn a_resumed_conversation_watches_its_artifacts_again() {
    use eos_agent_core::StoredMessage;
    use eos_agent_types::{ContentBlock, Role, ToolResultContent};

    let dirs = Dirs::new("watch-resume");
    let (base, frame) = start_frame().await;
    let model = mock([Reply::text("ok")]).await;
    let watch = WatchConfig { live_origin: base.clone(), ..WatchConfig::default() };
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::Bearer("tok".into()))))
        .base_url(model.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .artifact_api(FrameConfig { base_url: base.clone(), ..FrameConfig::default() }, watch)
        .build()
        .await
        .expect("sdk");
    let call = serde_json::value::RawValue::from_string(r#"{"file_path":"p.html"}"#.into()).expect("json");
    let url = eos_agent_artifacts::viewer_url(SLUG);
    let result = format!("Published /w/p.html at {url} (Version 1, version id 100-a).");
    let history = vec![
        StoredMessage::user_text("publish it"),
        StoredMessage::new(
            Role::Assistant,
            &[ContentBlock::ToolUse { id: "toolu_p".into(), name: "Artifact".into(), input: call }],
        )
        .expect("message"),
        StoredMessage::new(
            Role::User,
            &[ContentBlock::ToolResult {
                tool_use_id: "toolu_p".into(),
                content: Some(ToolResultContent::Text(result)),
                is_error: false,
            }],
        )
        .expect("message"),
    ];
    let (_agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).history(history)).await.expect("spawn");
    until("the re-armed live channel", || frame.open.load(Ordering::SeqCst) == 1).await;
}
