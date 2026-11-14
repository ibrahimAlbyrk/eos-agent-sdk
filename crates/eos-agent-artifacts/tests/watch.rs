//! Artifact watching against a local stand-in for the boot, comments and
//! live-channel endpoints.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use eos_agent_artifacts::{
    Armed, ArtifactWatcher, ComposeRequest, Composed, FrameClient, FrameConfig, Probe, WatchConfig, WatchNotifier,
    tools_watched, viewer_url,
};
use eos_agent_core::{
    BoxFuture, CancellationToken, Credential, Extensions, StaticCredential, Tool, ToolContext, Workspace,
};
use eos_agent_types::ToolResultContent;
use serde_json::{Value, json};
use tokio::sync::broadcast;

const SLUG: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";

#[derive(Clone)]
struct Server {
    boots: Arc<AtomicUsize>,
    renewals: Arc<AtomicUsize>,
    /// Boots say the token may be renewed.
    renewable: bool,
    sockets: Arc<AtomicUsize>,
    open: Arc<AtomicUsize>,
    protocols: Arc<Mutex<Vec<String>>>,
    comments: Arc<Mutex<Value>>,
    /// Replies posted: (thread, body).
    posted: Arc<Mutex<Vec<(String, Value)>>>,
    /// Summon-status updates: (thread, body).
    statuses: Arc<Mutex<Vec<(String, Value)>>>,
    /// Frames to send on every open socket; "close" drops them.
    frames: broadcast::Sender<String>,
    refuse_sockets: bool,
    /// Boots answered 503 before the real answer.
    unavailable_boots: Arc<AtomicUsize>,
    /// The live page's stored source, on the content host.
    page: Arc<Mutex<String>>,
    /// Publishes received.
    deploys: Arc<Mutex<Vec<Value>>>,
    /// Threads resolved: (thread, body).
    resolved: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Server {
    fn new(refuse_sockets: bool) -> Self {
        Self {
            boots: Arc::default(),
            renewals: Arc::default(),
            renewable: false,
            sockets: Arc::default(),
            open: Arc::default(),
            protocols: Arc::default(),
            comments: Arc::new(Mutex::new(json!({ "threads": [], "names": {} }))),
            posted: Arc::default(),
            statuses: Arc::default(),
            frames: broadcast::channel(64).0,
            refuse_sockets,
            unavailable_boots: Arc::default(),
            page: Arc::new(Mutex::new("<!doctype html><html><body><h1 id=\"t\">Plans</h1></body></html>".into())),
            deploys: Arc::default(),
            resolved: Arc::default(),
        }
    }

    async fn start(self) -> (String, Self) {
        let app = Router::new()
            .route("/api/frame/comments/{slug}", get(comments))
            .route("/api/frame/{slug}", get(boot))
            .route("/api/frame/watch-token/{slug}", post(renew))
            .route("/api/frame/comments/{slug}/{thread}", post(answer))
            .route("/api/frame/comments/{slug}/{thread}/summon-status", post(summon_status))
            .route("/edge-api/frame-live/{slug}/ws", get(socket))
            .route("/_host/{slug}/_f/{ver}/_src/index.html", get(page))
            .route("/api/frame/deploy/direct", post(deploy))
            .route("/api/frame/comments/{slug}/{thread}/resolve", post(resolve))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, self)
    }

    fn send(&self, frame: Value) {
        let _ = self.frames.send(frame.to_string());
    }
}

async fn boot(State(s): State<Server>, Path(slug): Path<String>) -> Response {
    s.boots.fetch_add(1, Ordering::SeqCst);
    // Boots come one at a time: no other boot races this check.
    if s.unavailable_boots.load(Ordering::SeqCst) > 0 {
        s.unavailable_boots.fetch_sub(1, Ordering::SeqCst);
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if slug != SLUG {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut answer =
        json!({ "ver": "100-a", "assetToken": "at", "subscriptionToken": "sub.tok", "perm": { "role": "owner" } });
    if s.renewable {
        answer["watchTokenRenewEnabled"] = json!(true);
        answer["subscriptionTokenExp"] = json!(now_s() + 3600);
    }
    axum::Json(answer).into_response()
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

async fn renew(State(s): State<Server>, Path(slug): Path<String>) -> Response {
    let n = s.renewals.fetch_add(1, Ordering::SeqCst) + 1;
    assert_eq!(slug, SLUG);
    axum::Json(json!({ "ver": "100-a", "subscriptionToken": format!("sub.tok{}", n + 1),
        "subscriptionTokenExp": now_s() + 3600 }))
    .into_response()
}

async fn answer(
    State(s): State<Server>,
    Path((_, thread)): Path<(String, String)>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    s.posted.lock().expect("posted").push((thread.clone(), body));
    axum::Json(json!({ "thread_id": thread, "comment_id": "c9" })).into_response()
}

async fn deploy(State(s): State<Server>, axum::Json(body): axum::Json<Value>) -> Response {
    s.deploys.lock().expect("deploys").push(body);
    axum::Json(json!({ "slug": SLUG, "version": "200-b", "seq": 2 })).into_response()
}

async fn resolve(
    State(s): State<Server>,
    Path((_, thread)): Path<(String, String)>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    s.resolved.lock().expect("resolved").push((thread, body));
    axum::Json(json!({ "ok": true })).into_response()
}

async fn page(State(s): State<Server>) -> Response {
    let html = s.page.lock().expect("page").clone();
    ([("content-type", "text/html; charset=utf-8")], html).into_response()
}

async fn summon_status(
    State(s): State<Server>,
    Path((_, thread)): Path<(String, String)>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    s.statuses.lock().expect("statuses").push((thread, body));
    StatusCode::NO_CONTENT.into_response()
}

async fn comments(State(s): State<Server>) -> Response {
    axum::Json(s.comments.lock().expect("comments").clone()).into_response()
}

async fn socket(State(s): State<Server>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let protocols = headers.get("sec-websocket-protocol").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    s.protocols.lock().expect("protocols").push(protocols);
    assert_eq!(headers.get("authorization"), None, "the live channel gets only its own token");
    if s.refuse_sockets {
        return StatusCode::FORBIDDEN.into_response();
    }
    s.sockets.fetch_add(1, Ordering::SeqCst);
    let rx = s.frames.subscribe();
    ws.protocols(["frame-live.v1"]).on_upgrade(move |socket| serve(socket, rx, s))
}

async fn serve(mut socket: WebSocket, mut rx: broadcast::Receiver<String>, s: Server) {
    s.open.fetch_add(1, Ordering::SeqCst);
    loop {
        tokio::select! {
            frame = rx.recv() => match frame {
                Ok(f) if f == "\"close\"" => break,
                Ok(f) if f == "\"expired\"" => {
                    let frame = CloseFrame { code: 4001, reason: "token expired".into() };
                    let _ = socket.send(Message::Close(Some(frame))).await;
                    break;
                }
                Ok(f) => if socket.send(Message::Text(f.into())).await.is_err() { break },
                Err(_) => break,
            },
            incoming = socket.recv() => if incoming.is_none_or(|m| m.is_err()) { break },
        }
    }
    s.open.fetch_sub(1, Ordering::SeqCst);
}

#[derive(Default)]
struct Notes {
    got: Mutex<Vec<(String, String, bool)>>,
    subagents: Vec<String>,
    /// What the agent's model answers (none: no model at hand).
    reply: Option<String>,
    /// The host's gate refuses the post.
    refuse: bool,
    prompts: Mutex<Vec<(String, String)>>,
    /// Answers given first, in order, before `reply`.
    script: Mutex<std::collections::VecDeque<String>>,
    /// The model's output limit (automatic edits need one).
    cap: Option<u32>,
    /// Calls the gate was asked about: (tool, input).
    calls: Mutex<Vec<(String, Value)>>,
    /// The session is in plan mode.
    plan: bool,
    /// The permission probe's verdict (none: from `refuse`).
    verdict: Option<Probe>,
}

impl WatchNotifier for Notes {
    fn notify<'a>(&'a self, agent: &'a str, text: String, wake: bool) -> BoxFuture<'a, bool> {
        self.got.lock().expect("notes").push((agent.to_owned(), text, wake));
        Box::pin(std::future::ready(true))
    }

    fn can_watch(&self, agent: &str) -> bool {
        !self.subagents.iter().any(|a| a == agent)
    }

    fn compose<'a>(&'a self, _agent: &'a str, request: &'a ComposeRequest) -> BoxFuture<'a, Option<Composed>> {
        self.prompts.lock().expect("prompts").push((request.system.clone(), request.blocks.join("\n----\n")));
        let scripted = self.script.lock().expect("script").pop_front();
        Box::pin(std::future::ready(scripted.or_else(|| self.reply.clone()).map(Composed::Text)))
    }

    fn output_cap(&self, _agent: &str) -> Option<u32> {
        self.cap
    }

    fn plan_mode(&self, _agent: &str) -> bool {
        self.plan
    }

    fn probe<'a>(&'a self, agent: &'a str, tool: &'a str, input: &'a Value) -> BoxFuture<'a, Probe> {
        match self.verdict {
            Some(v) => Box::pin(std::future::ready(v)),
            None => {
                Box::pin(async move { if self.may_call(agent, tool, input).await { Probe::Allow } else { Probe::Ask } })
            }
        }
    }

    fn may_call<'a>(&'a self, _agent: &'a str, tool: &'a str, input: &'a Value) -> BoxFuture<'a, bool> {
        assert!(tool == "Artifact" || (tool, &input["action"]) == ("ArtifactComments", &json!("reply")));
        self.calls.lock().expect("calls").push((tool.to_owned(), input.clone()));
        Box::pin(std::future::ready(!self.refuse))
    }
}

impl Notes {
    fn take(&self) -> Vec<(String, String, bool)> {
        std::mem::take(&mut *self.got.lock().expect("notes"))
    }
}

fn config(base: &str) -> WatchConfig {
    WatchConfig {
        live_origin: base.to_owned(),
        backoff: (Duration::from_millis(20), Duration::from_millis(80)),
        give_up_after: 3,
        min_uptime: Duration::from_millis(50),
        spread: (Duration::from_millis(10), Duration::from_millis(30)),
        comment_debounce: Duration::from_millis(30),
        notice_settle: (Duration::from_millis(20), Duration::from_millis(100)),
        ..WatchConfig::default()
    }
}

fn watcher(base: &str, notes: &Arc<Notes>, config: WatchConfig) -> ArtifactWatcher {
    let auth = Arc::new(StaticCredential(Credential::Bearer("tok".into())));
    let frame = FrameConfig {
        base_url: base.to_owned(),
        content_host: format!("{base}/_host/{{slug}}"),
        ..FrameConfig::default()
    };
    let client = FrameClient::new(auth, frame).expect("client");
    ArtifactWatcher::new(Arc::new(client), Arc::clone(notes) as Arc<dyn WatchNotifier>, config)
}

/// Threads activated for Claude before the watch starts, with no comments
/// yet (`edit`: with edit capability): the first read only learns them.
fn activated(ids: &[&str], edit: bool) -> Value {
    let thread = |id: &&str| {
        let mut t = json!({ "id": id, "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [] });
        if edit {
            t["claude_capability"] = json!("edit");
        }
        t
    };
    Value::Array(ids.iter().map(thread).collect())
}

/// Waits (bounded) for `check`.
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
async fn a_republish_leaves_a_note_and_a_comment_sent_to_claude_wakes() {
    let (base, server) = Server::new(false).start().await;
    *server.comments.lock().expect("c") = json!({ "names": { "u_1": "Ada" }, "threads": [
        { "id": "t1", "claude_activated_at": "2026-11-08T07:00:00Z", "comments": [{ "id": "c0",
          "author": { "account": "u_1", "role": "human" }, "text": "old one", "to_claude_at": "2026-11-08T08:00:00Z" }] }
    ]});
    let notes = Arc::new(Notes::default());
    let w = watcher(&base, &notes, config(&base));
    assert_eq!(w.watch("agent-1", SLUG, Some("100-a"), true, false), Armed::Arming);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    assert_eq!(w.watch("agent-1", SLUG, Some("100-a"), true, false), Armed::Connected);
    assert_eq!(server.protocols.lock().expect("p")[0], "frame-live.v1, sub.tok");

    // Its own version is not news; a version from elsewhere is, once.
    server.send(json!({ "kind": "publish", "ver": "100-a" }));
    server.send(json!({ "kind": "publish", "ver": "200-b" }));
    server.send(json!({ "kind": "publish", "ver": "200-b" }));
    until("the republish note", || !notes.got.lock().expect("n").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = notes.take();
    assert_eq!(got.len(), 1, "{got:?}");
    let (agent, text, wake) = &got[0];
    assert_eq!((agent.as_str(), *wake), ("agent-1", false), "a republish starts no turn");
    assert_eq!(
        text,
        &format!(
            "Artifact {}: [Version 200-b of this artifact was published after the version you have. Before you \
             publish to it, fetch the artifact's URL again and merge your edits onto that version.]",
            viewer_url(SLUG)
        )
    );

    // A comment sent to Claude after the watch began wakes the agent; the old one doesn't.
    server.comments.lock().expect("c")["threads"][0]["comments"]
        .as_array_mut()
        .expect("comments")
        .push(json!({ "id": "c1", "author": { "account": "u_1", "role": "human" }, "text": "Make the title blue",
            "to_claude_at": "2026-11-08T09:00:00.5Z" }));
    server.send(json!({ "kind": "comment" }));
    server.send(json!({ "kind": "comment" }));
    until("the comment note", || !notes.got.lock().expect("n").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = notes.take();
    assert_eq!(got.len(), 1, "a burst is read once: {got:?}");
    let (_, text, wake) = &got[0];
    assert!(*wake, "a comment sent to Claude wakes the agent");
    assert!(text.starts_with(
        "<task-notification>\n<task-type>artifact-auto-react</task-type>\n<summary>Comments are waiting on Artifact: "
    ));
    assert!(
        text.contains(&format!(
            "</summary>\nThe user sent you a comment in thread t1 of artifact {}",
            viewer_url(SLUG)
        ))
    );
    assert!(
        text.contains("author=\"Ada\"&gt;\nMake the title blue\n&lt;/artifact-comment&gt;")
            && !text.contains("old one")
    );

    w.forget("agent-1");
    until("the socket to close", || server.open.load(Ordering::SeqCst) == 0).await;
    assert_eq!(w.artifacts(), 0);
}

#[tokio::test]
async fn agents_share_one_connection_per_artifact() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes::default());
    let w = watcher(&base, &notes, config(&base));
    w.watch("a1", SLUG, Some("100-a"), true, false);
    w.watch("a2", SLUG, None, false, true);
    until("connected", || w.list("a2").first().is_some_and(|r| r.connected)).await;
    assert_eq!(server.sockets.load(Ordering::SeqCst), 1);
    assert!(w.list("a2")[0].replies && !w.list("a1")[0].replies, "the newest watch answers comments");

    server.send(json!({ "ver": "300-c" }));
    until("both notes", || notes.got.lock().expect("n").len() == 2).await;
    let mut agents: Vec<String> = notes.take().into_iter().map(|(a, _, _)| a).collect();
    agents.sort();
    assert_eq!(agents, ["a1", "a2"]);

    // The agent that read it has it: no note for that version later.
    w.saw("a1", SLUG, "400-d", false);
    server.send(json!({ "ver": "400-d" }));
    until("a2's note", || notes.got.lock().expect("n").len() == 1).await;
    assert_eq!(notes.take()[0].0, "a2");

    assert!(w.unwatch("a2", SLUG) && !w.unwatch("a2", SLUG));
    assert!(w.list("a1")[0].replies, "replies pass to the remaining watcher");
    assert_eq!(server.open.load(Ordering::SeqCst), 1);
    w.forget("a1");
    until("the socket to close", || server.open.load(Ordering::SeqCst) == 0).await;
}

#[tokio::test]
async fn reconnects_after_a_drop_with_its_leased_token() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes::default());
    let w = watcher(&base, &notes, config(&base));
    w.watch("a1", SLUG, Some("100-a"), true, false);
    until("connected", || server.open.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    server.send(json!("close"));
    until("a second socket", || server.sockets.load(Ordering::SeqCst) == 2).await;
    until("connected again", || w.list("a1").first().is_some_and(|r| r.connected)).await;
    assert_eq!(server.boots.load(Ordering::SeqCst), 1, "the token's lease still runs: no new boot");
    assert_eq!(server.protocols.lock().expect("p")[1], "frame-live.v1, sub.tok");
    assert!(notes.take().is_empty(), "nothing new went live meanwhile");
    w.forget("a1");
}

/// Claude Code renews a token the channel closed as expired (4001) through
/// the watch-token endpoint when the boot allowed it, without a new boot.
#[tokio::test]
async fn an_expired_token_is_renewed_without_a_boot() {
    let (base, server) = Server { renewable: true, ..Server::new(false) }.start().await;
    let notes = Arc::new(Notes::default());
    let w = watcher(&base, &notes, config(&base));
    w.watch("a1", SLUG, Some("100-a"), true, false);
    until("connected", || server.open.load(Ordering::SeqCst) == 1).await;
    server.send(json!("expired"));
    until("a second socket", || server.sockets.load(Ordering::SeqCst) == 2).await;
    assert_eq!(server.renewals.load(Ordering::SeqCst), 1);
    assert_eq!(server.boots.load(Ordering::SeqCst), 1);
    assert_eq!(server.protocols.lock().expect("p")[1], "frame-live.v1, sub.tok2");
    w.forget("a1");
}

#[tokio::test]
async fn a_watch_that_cannot_connect_ends_and_says_so() {
    let (base, _server) = Server::new(true).start().await;
    let notes = Arc::new(Notes::default());
    let w = watcher(&base, &notes, config(&base));
    w.watch("a1", SLUG, Some("100-a"), true, false);
    until("the end note", || !notes.got.lock().expect("n").is_empty()).await;
    let got = notes.take();
    assert!(got[0].1.starts_with(&format!("Watch on {} ended — could not connect.", viewer_url(SLUG))), "{got:?}");
    assert!(!got[0].2);
    assert_eq!(w.artifacts(), 0);

    // A deleted artifact ends its watch too.
    let w = watcher(&base, &notes, config(&base));
    w.watch("a1", "11111111-2222-4333-8444-555555555555", None, false, true);
    until("the end note", || !notes.got.lock().expect("n").is_empty()).await;
    assert!(notes.take()[0].1.contains("no longer exists"));
}

/// While the API answers 5xx, Claude Code waits its stall windows and does
/// not count the attempts toward giving up.
#[tokio::test]
async fn a_boot_answered_503_waits_and_does_not_give_up() {
    let (base, server) = Server::new(false).start().await;
    server.unavailable_boots.store(4, Ordering::SeqCst);
    let notes = Arc::new(Notes::default());
    let stall = (Duration::from_millis(10), Duration::from_millis(20));
    let w = watcher(&base, &notes, WatchConfig { give_up_after: 1, stall, long_stall: stall, ..config(&base) });
    w.watch("a1", SLUG, Some("100-a"), true, false);
    until("connected", || w.list("a1").first().is_some_and(|r| r.connected)).await;
    assert_eq!(server.boots.load(Ordering::SeqCst), 5);
    assert!(notes.take().is_empty(), "the watch did not end");
    w.forget("a1");
}

#[tokio::test]
async fn limits_and_subagents() {
    let (base, _server) = Server::new(false).start().await;
    let notes = Arc::new(Notes { subagents: vec!["sub".into()], ..Notes::default() });
    let w = watcher(&base, &notes, WatchConfig { max_per_agent: 1, max_artifacts: 2, ..config(&base) });
    assert_eq!(w.watch("sub", SLUG, None, true, false), Armed::NotHere);
    assert!(Armed::NotHere.line().contains("subagent"));

    let (one, two, three) = (
        "11111111-0000-4000-8000-000000000001",
        "11111111-0000-4000-8000-000000000002",
        "11111111-0000-4000-8000-000000000003",
    );
    w.watch("a1", one, None, false, true);
    assert!(matches!(w.watch("a1", two, None, false, true), Armed::Refused(_)), "an explicit watch never displaces");
    w.forget("a1");
    w.watch("a1", one, None, true, false);
    w.watch("a1", two, None, true, false);
    let urls: Vec<String> = w.list("a1").into_iter().map(|r| r.url).collect();
    assert_eq!(urls, [viewer_url(two)], "a publish displaces the oldest implicit watch");
    w.watch("a2", one, None, true, false);
    assert!(matches!(w.watch("a3", three, None, true, false), Armed::Refused(_)), "the process-wide cap");
}

#[tokio::test]
async fn the_comments_tool_manages_watches() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes::default());
    let auth = Arc::new(StaticCredential(Credential::Bearer("tok".into())));
    let (tools, watcher) =
        tools_watched(auth, FrameConfig { base_url: base.clone(), ..FrameConfig::default() }, notes, config(&base))
            .expect("tools");
    let comments = &tools[1];
    let call = |input: Value| run(comments, input);
    assert_eq!(call(json!({ "action": "watch" })).await, "This session holds no artifact watches.");
    let started = call(json!({ "action": "watch", "url": viewer_url(SLUG) })).await;
    assert!(started.starts_with(&format!("Watching {}: connecting", viewer_url(SLUG))), "{started}");
    until("connected", || server.open.load(Ordering::SeqCst) == 1).await;
    until("listed connected", || watcher.list("a1").first().is_some_and(|r| r.connected)).await;
    let listing = call(json!({ "action": "watch" })).await;
    assert!(listing.contains("— connected — auto-replies armed"), "{listing}");
    let stopped = call(json!({ "action": "watch", "url": viewer_url(SLUG), "on": false })).await;
    assert!(stopped.starts_with("Stopped watching"), "{stopped}");
    until("the socket to close", || server.open.load(Ordering::SeqCst) == 0).await;
}

async fn run(tool: &Arc<dyn Tool>, input: Value) -> String {
    let raw = serde_json::value::to_raw_value(&input).expect("raw");
    let (cancel, ext) = (CancellationToken::new(), Extensions::default());
    ext.insert(Workspace::new(std::env::temp_dir(), Vec::new(), std::env::temp_dir()));
    let ctx = ToolContext { agent_id: "a1", tool_use_id: "t1", cancel: &cancel, extensions: &ext, transcript: &[] };
    match tool.call(&raw, ctx).await.content {
        ToolResultContent::Text(text) => text,
        ToolResultContent::Blocks(_) => panic!("text expected"),
    }
}

/// A comment sent to Claude that a summon wakes: Claude Code answers it on
/// its own (reply-only composer, posted as answering the summons) and tells
/// the session; an artifact the session publishes from a file is handed over.
#[tokio::test]
async fn comments_sent_to_claude_are_auto_replied_or_handed_over() {
    let (base, server) = Server::new(false).start().await;
    *server.comments.lock().expect("c") = json!({ "names": { "u_1": "Ada" }, "threads": activated(&["t1"], false) });
    let notes = Arc::new(Notes { reply: Some("Blue matches the logo. Working on it.".into()), ..Notes::default() });
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    server.comments.lock().expect("c")["threads"] = json!([{ "id": "t1", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
        { "id": "c1", "author": { "account": "u_1", "role": "human" }, "text": "Why blue?",
          "to_claude_at": "2026-11-08T09:00:00Z" }] }]);
    server.send(json!({ "kind": "comment" }));
    until("the auto-reply note", || !notes.got.lock().expect("n").is_empty()).await;
    let (system, prompt) = notes.prompts.lock().expect("p")[0].clone();
    assert_eq!(system, eos_agent_artifacts::REPLY_SYSTEM);
    assert!(prompt.contains("[human, sent to you]\n") && prompt.contains("| Why blue?"), "{prompt}");
    let posted = server.posted.lock().expect("posted").clone();
    assert_eq!(
        posted,
        [("t1".to_owned(), json!({ "text": "Blue matches the logo. Working on it.", "answers_summon": true }))]
    );
    until("the summon status", || !server.statuses.lock().expect("s").is_empty()).await;
    let (thread, status) = server.statuses.lock().expect("s")[0].clone();
    assert_eq!(
        (thread.as_str(), &status["status"], &status["gesture_at"]),
        ("t1", &json!("working"), &json!("2026-11-08T09:00:00Z"))
    );
    assert!(status["sid"].as_str().is_some_and(|s| s.len() == 16) && status["seq"].as_u64().is_some());
    let (_, text, wake) = notes.take().remove(0);
    assert!(wake);
    assert!(
        text.contains(&format!(
            "<summary>Auto-replied to comment thread on Artifact: {}</summary>\nAuto-replied to thread t1 on artifact {} — a \
             reply only:",
            viewer_url(SLUG),
            viewer_url(SLUG)
        ))
            && text.contains("The auto-posted reply (comment c9) is already in the thread"),
        "{text}"
    );

    // Published from a local file: the session gets the thread instead.
    w.published_from(SLUG, std::path::Path::new("/work/page.html"));
    server.comments.lock().expect("c")["threads"][0]["comments"]
        .as_array_mut()
        .expect("c")
        .push(json!({ "id": "c2", "author": { "account": "u_1", "role": "human" }, "text": "And bigger?",
                "to_claude_at": "2026-11-08T09:05:00Z" }));
    server.send(json!({ "kind": "comment" }));
    until("the hand-over note", || !notes.got.lock().expect("n").is_empty()).await;
    let (_, text, wake) = notes.take().remove(0);
    assert!(wake);
    assert!(
        text.contains(
            "No automatic reply was posted and no automatic edit was attempted: this session publishes \
        the artifact from /work/page.html"
        ),
        "{text}"
    );
    assert_eq!(server.posted.lock().expect("posted").len(), 1);
    w.forget("agent-1");
}

#[tokio::test]
async fn a_reply_the_gate_refuses_is_not_posted_and_the_session_is_told_once() {
    let (base, server) = Server::new(false).start().await;
    server.comments.lock().expect("c")["threads"] = activated(&["t1"], false);
    let notes = Arc::new(Notes { reply: Some("Sure.".into()), refuse: true, ..Notes::default() });
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    for (i, at) in ["2026-11-08T09:00:00Z", "2026-11-08T09:01:00Z"].iter().enumerate() {
        server.comments.lock().expect("c")["threads"] = json!([{ "id": "t1", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
            { "id": format!("c{i}"), "author": { "account": "u_1", "role": "human" }, "text": "hi", "to_claude_at": at }] }]);
        server.send(json!({ "kind": "comment" }));
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let got = notes.take();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("Auto-reply is notify-only in this permission mode"), "{}", got[0].1);
    assert!(server.posted.lock().expect("posted").is_empty());
    let statuses = server.statuses.lock().expect("s").clone();
    assert!(statuses.iter().any(|(_, s)| s["status"] == "declined" && s["reason"] == "ask_mode"), "{statuses:?}");
    w.forget("agent-1");
}

/// Claude Code takes up at most so many threads an hour unprompted; past
/// that it says so once and tells the page `hourly_cap`.
#[tokio::test]
async fn the_hourly_cap_holds_threads_back_and_says_so_once() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes { reply: Some("On it.".into()), ..Notes::default() });
    server.comments.lock().expect("c")["threads"] = activated(&["t0", "t1", "t2"], false);
    let w = watcher(&base, &notes, WatchConfig { auto_turns_per_hour: 1, ..config(&base) });
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    for (i, at) in ["2026-11-08T09:00:00Z", "2026-11-08T09:01:00Z", "2026-11-08T09:02:00Z"].iter().enumerate() {
        server.comments.lock().expect("c")["threads"][i]["comments"] =
            json!([{ "id": format!("c{i}"), "author": { "account": "u_1" }, "text": "hi", "to_claude_at": at }]);
        server.send(json!({ "kind": "comment" }));
        until("a scan", || server.statuses.lock().expect("s").len() > i).await;
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    until("the first reply", || server.posted.lock().expect("p").len() == 1).await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    let got = notes.take();
    assert_eq!(server.posted.lock().expect("posted").len(), 1, "only the first thread is answered");
    let capped: Vec<_> = got.iter().filter(|(_, t, _)| t.contains("auto-reply held back (hourly cap)")).collect();
    assert_eq!(capped.len(), 1, "{got:?}");
    let reasons: Vec<Value> = server.statuses.lock().expect("s").iter().map(|(_, s)| s["reason"].clone()).collect();
    assert_eq!(reasons, [Value::Null, json!("hourly_cap"), json!("hourly_cap")]);
    w.forget("agent-1");
}

/// Claude Code gathers its automatic-reply notices on one artifact into one
/// notification once the burst settles.
#[tokio::test]
async fn auto_reply_notices_on_one_artifact_come_as_one() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes { reply: Some("Thanks.".into()), ..Notes::default() });
    let settle = (Duration::from_millis(300), Duration::from_secs(2));
    server.comments.lock().expect("c")["threads"] = activated(&["t1", "t2"], false);
    let w = watcher(&base, &notes, WatchConfig { notice_settle: settle, ..config(&base) });
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    server.comments.lock().expect("c")["threads"] = json!([
        { "id": "t1", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [{ "id": "c1", "author": { "account": "u_1" }, "text": "a",
          "to_claude_at": "2026-11-08T09:00:00Z" }] },
        { "id": "t2", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [{ "id": "c2", "author": { "account": "u_1" }, "text": "b",
          "to_claude_at": "2026-11-08T09:00:01Z" }] }
    ]);
    server.send(json!({ "kind": "comment" }));
    until("both replies", || server.posted.lock().expect("p").len() == 2).await;
    assert!(notes.take().is_empty(), "the burst is still settling");
    until("the gathered notice", || !notes.got.lock().expect("n").is_empty()).await;
    let got = notes.take();
    assert_eq!(got.len(), 1, "{got:?}");
    let text = &got[0].1;
    assert!(
        text.contains(&format!(
            "<summary>Auto-replied to 2 comment threads on Artifact: {}</summary>",
            viewer_url(SLUG)
        ))
    );
    assert!(text.contains("Auto-replied to thread t1 on artifact") && text.contains("\n\nAuto-replied to thread t2"));
    w.forget("agent-1");
}

/// An edit-capable thread is edited as Claude Code does it: the composer's
/// patch is applied to the live source and published under the thread's
/// edit grant, the summary reply posted and the thread resolved at the new
/// version; a patch that won't apply is asked again once, then answered.
#[tokio::test]
async fn edit_capable_threads_are_edited_automatically() {
    let (base, server) = Server::new(false).start().await;
    *server.comments.lock().expect("c") = json!({ "names": { "u_1": "Ada" }, "threads": activated(&["a1"], true) });
    let notes = Arc::new(Notes { cap: Some(64_000), ..Notes::default() });
    notes.script.lock().expect("s").push_back(
        json!({ "action": "edit", "edits": [{ "find": ">Plans<", "replace": ">Pricing plans<" }],
                "reply": "Renamed the heading to Pricing plans" })
        .to_string(),
    );
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    server.comments.lock().expect("c")["threads"] = json!([{ "id": "a1", "claude_capability": "edit", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
        { "id": "c1", "author": { "account": "u_1", "role": "human" }, "text": "Call it Pricing plans",
          "to_claude_at": "2026-11-08T09:00:00Z" }] }]);
    server.send(json!({ "kind": "comment" }));
    until("the auto-edit note", || !notes.got.lock().expect("n").is_empty()).await;

    let (system, prompt) = notes.prompts.lock().expect("p")[0].clone();
    assert!(system.starts_with("You decide and compose artifact comment-thread responses"));
    assert!(
        prompt.starts_with("The text between the <SU")
            && prompt.contains("is the CURRENT SOURCE of an artifact you maintain")
    );
    assert!(prompt.contains("You are an edit-capable composer for this thread"), "{prompt}");
    let deploys = server.deploys.lock().expect("d").clone();
    assert_eq!(deploys.len(), 1);
    let d = &deploys[0];
    assert_eq!((&d["slug"], &d["baseVersion"]), (&json!(SLUG), &json!("100-a")));
    assert_eq!(d["auto_edit_attribution"], json!({ "thread_id": "a1", "comment_id": "c1" }));
    assert_eq!(d["content"], "<!doctype html><html><body><h1 id=\"t\">Pricing plans</h1></body></html>");
    let calls = notes.calls.lock().expect("calls").clone();
    assert!(calls.iter().any(|(tool, input)| tool == "Artifact" && input["url"] == viewer_url(SLUG)), "{calls:?}");
    assert_eq!(
        server.posted.lock().expect("posted").clone(),
        [("a1".to_owned(), json!({ "text": "Renamed the heading to Pricing plans", "answers_summon": true }))]
    );
    assert_eq!(
        server.resolved.lock().expect("r").clone(),
        [("a1".to_owned(), json!({ "resolved": true, "after_version": "200-b" }))]
    );
    let (_, text, _) = notes.take().remove(0);
    assert!(
        text.contains(&format!(
            "<summary>Auto-edited Artifact: {} in response to a comment thread</summary>\nAuto-edited artifact {} in \
             response to thread a1 and posted a summary reply (comment c9). Review the change",
            viewer_url(SLUG),
            viewer_url(SLUG)
        )),
        "{text}"
    );

    // A patch that does not apply is asked again once, then answered with Claude Code's words.
    let miss =
        json!({ "action": "edit", "edits": [{ "find": "nowhere", "replace": "x" }], "reply": "Done" }).to_string();
    notes.script.lock().expect("s").extend([miss.clone(), miss]);
    server.comments.lock().expect("c")["threads"][0]["comments"]
        .as_array_mut()
        .expect("c")
        .push(json!({ "id": "c2", "author": { "account": "u_1", "role": "human" }, "text": "And bold?",
                "to_claude_at": "2026-11-08T10:00:00Z" }));
    server.send(json!({ "kind": "comment" }));
    until("the second note", || !notes.got.lock().expect("n").is_empty()).await;
    let prompts = notes.prompts.lock().expect("p").clone();
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].1.contains("Your previous patch was NOT applied: one edit's \"find\" text"), "{}", prompts[2].1);
    assert_eq!(server.deploys.lock().expect("d").len(), 1, "nothing more was published");
    let posted = server.posted.lock().expect("posted").clone();
    assert!(
        posted[1].1["text"].as_str().is_some_and(|t| t.starts_with("I could not apply the requested edit: my change"))
    );
    let (_, text, _) = notes.take().remove(0);
    assert!(
        text.contains("<summary>Auto-replied to comment thread on Artifact:")
            && text.contains("Auto-reply posted to thread a1 on artifact")
            && text.contains("a requested automatic edit could not be applied to the artifact's current source"),
        "{text}"
    );
    w.forget("agent-1");
}

/// Claude Code publishes an automatic edit as the session's own Artifact
/// call, so the session has that version afterwards: its next publish
/// builds on the edit instead of being refused as built on an older one.
#[tokio::test]
async fn an_automatic_edit_is_the_sessions_version_afterwards() {
    let (base, server) = Server::new(false).start().await;
    *server.comments.lock().expect("c") = json!({ "names": { "u_1": "Ada" }, "threads": activated(&["a1"], true) });
    let notes = Arc::new(Notes { cap: Some(64_000), ..Notes::default() });
    notes.script.lock().expect("s").push_back(
        json!({ "action": "edit", "edits": [{ "find": ">Plans<", "replace": ">Pricing<" }], "reply": "Renamed" })
            .to_string(),
    );
    let auth = Arc::new(StaticCredential(Credential::Bearer("tok".into())));
    let frame = FrameConfig {
        base_url: base.clone(),
        content_host: format!("{base}/_host/{{slug}}"),
        ..FrameConfig::default()
    };
    let notifier = Arc::clone(&notes) as Arc<dyn WatchNotifier>;
    let (tools, watcher) = tools_watched(auth, frame, notifier, config(&base)).expect("tools");
    let dir = std::env::temp_dir().join(format!("autoedit-session-{:x}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let ext = Extensions::default();
    ext.insert(Workspace::new(dir.clone(), Vec::new(), dir.clone()));
    let cancel = CancellationToken::new();
    let call = |tool: usize, input: Value| {
        let (tools, ext, cancel) = (&tools, &ext, &cancel);
        async move {
            let raw = serde_json::value::to_raw_value(&input).expect("raw");
            let ctx = ToolContext { agent_id: "a1", tool_use_id: "t1", cancel, extensions: ext, transcript: &[] };
            match tools[tool].call(&raw, ctx).await.content {
                ToolResultContent::Text(text) => text,
                ToolResultContent::Blocks(_) => panic!("text expected"),
            }
        }
    };
    let read = call(0, json!({ "action": "read", "url": viewer_url(SLUG) })).await;
    assert!(read.contains("100-a"), "{read}");
    call(1, json!({ "action": "watch", "url": viewer_url(SLUG) })).await;
    until("connected", || watcher.list("a1").first().is_some_and(|r| r.connected)).await;
    server.comments.lock().expect("c")["threads"] = json!([{ "id": "a1", "claude_capability": "edit", "claude_activated_at": "2026-11-08T08:00:00Z", "comments": [
        { "id": "c1", "author": { "account": "u_1" }, "text": "Call it Pricing", "to_claude_at": "2026-11-08T09:00:00Z" }] }]);
    server.send(json!({ "kind": "comment" }));
    until("the auto-edit note", || !notes.got.lock().expect("n").is_empty()).await;
    assert_eq!(server.deploys.lock().expect("d").len(), 1);

    std::fs::write(dir.join("page.html"), "<!doctype html><title>Plans</title><p>mine</p>").expect("write");
    let published =
        call(0, json!({ "action": "publish", "file_path": dir.join("page.html"), "url": viewer_url(SLUG) })).await;
    assert!(published.starts_with("Published "), "{published}");
    let deploys = server.deploys.lock().expect("d").clone();
    assert_eq!(deploys[1]["baseVersion"], "200-b", "built on the automatic edit");
    watcher.forget("a1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Claude Code's first sight of a thread: one that appears activated after
/// the first read is taken up as an activation (no summons answered); one
/// that is not activated is only learnt, its comment sent to Claude or not.
#[tokio::test]
async fn a_new_activated_thread_is_taken_up_as_an_activation() {
    let (base, server) = Server::new(false).start().await;
    let notes = Arc::new(Notes { reply: Some("Hello.".into()), ..Notes::default() });
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), true, false);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    server.comments.lock().expect("c")["threads"] = json!([
        { "id": "n1", "comments": [{ "id": "c1", "author": { "account": "u_1" }, "text": "not for Claude",
          "to_claude_at": "2026-11-08T09:00:00Z" }] },
        { "id": "a1", "claude_activated_at": "2026-11-08T07:00:00Z", "comments": [
          { "id": "c2", "author": { "account": "u_1" }, "text": "please look", "to_claude_at": "2026-11-08T07:00:00Z" }] }
    ]);
    server.send(json!({ "kind": "comment" }));
    until("the reply", || !server.posted.lock().expect("p").is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server.posted.lock().expect("p").clone(),
        [("a1".to_owned(), json!({ "text": "Hello." }))],
        "an activation, not an answer to the comment"
    );
    let statuses = server.statuses.lock().expect("s").clone();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].1["gesture_at"], "2026-11-08T07:00:00Z");
    w.forget("agent-1");
}

/// An explicit watch's first read picks up comments sent to Claude in the
/// last ten minutes (Claude Code's `pickUpRecentSummons`); a publish's does not.
#[tokio::test]
async fn an_explicit_watch_picks_up_recent_comments_sent_to_claude() {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("now").as_secs();
    let (days, rest) = (now / 86_400, now % 86_400);
    // Civil date from days (Howard Hinnant), for an RFC 3339 time a minute ago.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let rest = rest.saturating_sub(60);
    let at = format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest / 60 % 60, rest % 60);
    for explicit in [false, true] {
        let (base, server) = Server::new(false).start().await;
        server.comments.lock().expect("c")["threads"] = json!([{ "id": "t1", "claude_activated_at": at, "comments": [
            { "id": "c1", "author": { "account": "u_1" }, "text": "still there?", "to_claude_at": at }] }]);
        let notes = Arc::new(Notes { reply: Some("Yes.".into()), ..Notes::default() });
        let w = watcher(&base, &notes, config(&base));
        w.watch("agent-1", SLUG, Some("100-a"), !explicit, explicit);
        until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let posted = server.posted.lock().expect("p").clone();
        if explicit {
            assert_eq!(posted, [("t1".to_owned(), json!({ "text": "Yes.", "answers_summon": true }))]);
        } else {
            assert!(posted.is_empty(), "{posted:?}");
        }
        w.forget("agent-1");
    }
}

/// In plan mode Claude Code takes up no comment: the page hears
/// `plan_mode`, the session once that replies wait.
#[tokio::test]
async fn plan_mode_holds_replies_and_says_so_once() {
    let (base, server) = Server::new(false).start().await;
    server.comments.lock().expect("c")["threads"] = activated(&["t1"], false);
    let notes = Arc::new(Notes { reply: Some("Sure.".into()), plan: true, ..Notes::default() });
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    for (i, at) in ["2026-11-08T09:00:00Z", "2026-11-08T09:01:00Z"].iter().enumerate() {
        server.comments.lock().expect("c")["threads"][0]["comments"]
            .as_array_mut()
            .expect("c")
            .push(json!({ "id": format!("c{i}"), "author": { "account": "u_1" }, "text": "hi", "to_claude_at": at }));
        server.send(json!({ "kind": "comment" }));
        until("a status", || server.statuses.lock().expect("s").len() > i).await;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    let got = notes.take();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].1.contains(&format!(
            "Human comments sent to Claude are waiting on activated threads of artifact {} — auto-reply is paused \
             while in plan mode; use the ArtifactComments tool",
            viewer_url(SLUG)
        )),
        "{}",
        got[0].1
    );
    assert!(server.posted.lock().expect("p").is_empty() && notes.prompts.lock().expect("p").is_empty());
    let reasons: Vec<Value> = server.statuses.lock().expect("s").iter().map(|(_, s)| s["reason"].clone()).collect();
    assert_eq!(reasons, [json!("plan_mode"), json!("plan_mode")]);
    w.forget("agent-1");
}

/// Auto mode turning the unattended reply down: the page hears why, and the
/// session is told once, only for comments the user sent themselves.
#[tokio::test]
async fn an_auto_mode_refusal_is_said_once_for_the_users_own_comments() {
    let (base, server) = Server::new(false).start().await;
    server.comments.lock().expect("c")["threads"] = activated(&["t1"], false);
    let notes =
        Arc::new(Notes { reply: Some("Sure.".into()), verdict: Some(Probe::DeniedByAutoMode), ..Notes::default() });
    let w = watcher(&base, &notes, config(&base));
    w.watch("agent-1", SLUG, Some("100-a"), false, true);
    until("connected", || w.list("agent-1").first().is_some_and(|r| r.connected)).await;
    let sends = [("2026-11-08T09:00:00Z", false), ("2026-11-08T09:01:00Z", true), ("2026-11-08T09:02:00Z", true)];
    for (i, (at, own)) in sends.iter().enumerate() {
        server.comments.lock().expect("c")["threads"][0]["comments"]
            .as_array_mut()
            .expect("c")
            .push(json!({ "id": format!("c{i}"), "author": { "account": "u_1" }, "text": "hi", "to_claude_at": at,
                    "sent_by_viewer": own }));
        server.send(json!({ "kind": "comment" }));
        until("a status", || server.statuses.lock().expect("s").len() > i).await;
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    let got = notes.take();
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(got[0].1.contains("Auto mode did not approve an unattended auto-reply, so the reply was not posted"));
    let reasons: Vec<Value> = server.statuses.lock().expect("s").iter().map(|(_, s)| s["reason"].clone()).collect();
    assert!(reasons.len() == 3 && reasons.iter().all(|r| r == "denied_by_auto_mode"), "{reasons:?}");
    assert!(server.posted.lock().expect("p").is_empty());
    w.forget("agent-1");
}
