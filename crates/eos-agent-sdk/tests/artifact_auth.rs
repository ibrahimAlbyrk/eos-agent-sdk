//! The artifact tools can run on their own credential: a model billed on an
//! API key, artifacts published through a Claude sign-in.

mod common;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use common::{Dirs, all_text, mock, tool_names};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{BoxFuture, Credential, Extensions, ModelError, StaticCredential, TokenSource};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, ArtifactAuth, FrameConfig, Sdk, SdkBuilder, WatchConfig};
use serde_json::json;

/// A frame API that records the `authorization` header of every request.
async fn start_frame() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let app = Router::new()
        .route(
            "/api/frame/frames",
            get(|State(seen): State<Arc<Mutex<Vec<String>>>>, headers: HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("none");
                seen.lock().expect("seen").push(auth.to_owned());
                axum::Json(json!({ "frames": [{ "slug": "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b", "title": "Mine" }] }))
            }),
        )
        .with_state(Arc::clone(&seen));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base, seen)
}

fn builder(model: &MockApi, dirs: &Dirs, frame: &str) -> SdkBuilder {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("model-key".into()))))
        .base_url(model.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .artifact_api(FrameConfig { base_url: frame.to_owned(), ..FrameConfig::default() }, WatchConfig::default())
}

fn bearer(token: &str) -> Arc<dyn TokenSource> {
    Arc::new(StaticCredential(Credential::Bearer(token.into())))
}

type Source = Arc<dyn TokenSource>;

/// A sign-in store that cannot be read.
struct Unreadable;

impl TokenSource for Unreadable {
    fn credential(&self) -> BoxFuture<'_, Result<Credential, ModelError>> {
        Box::pin(async { Err(ModelError::Auth("no sign-in here".into())) })
    }
}

#[tokio::test]
async fn api_key_model_publishes_through_the_artifact_sign_in() {
    let dirs = Dirs::new("artifact-auth");
    let (frame, seen) = start_frame().await;
    let model = mock([Reply::tool_use("toolu_l", "Artifact", r#"{"action":"list"}"#), Reply::text("one")]).await;
    let sdk = builder(&model, &dirs, &frame).artifact_auth(bearer("sign-in")).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("list my artifacts").await.expect("alive").expect("turn");

    let requests = model.requests();
    assert!(tool_names(&requests[0]).iter().any(|n| n == "Artifact"), "on: the artifact credential is a sign-in");
    for request in &requests {
        assert_eq!(request.headers.get("x-api-key").and_then(|v| v.to_str().ok()), Some("model-key"));
        assert!(request.headers.get("authorization").is_none(), "the sign-in never reaches the model");
    }
    assert_eq!(*seen.lock().expect("seen"), ["Bearer sign-in"]);
    assert!(all_text(&requests[1]).contains("Mine"));
}

/// Agents of one process on different sign-ins: each one's artifact calls
/// go out on its own, and one without any is told to sign in.
#[tokio::test]
async fn each_agent_publishes_through_its_own_sign_in() {
    let dirs = Dirs::new("artifact-auth-per-agent");
    let (frame, seen) = start_frame().await;
    let list = || Reply::tool_use("toolu_l", "Artifact", r#"{"action":"list"}"#);
    let model = mock([list(), Reply::text("a"), list(), Reply::text("b"), list(), Reply::text("c")]).await;
    let sdk = builder(&model, &dirs, &frame).artifacts(true).build().await.expect("sdk");
    for own in [Some("first"), Some("second"), None] {
        let mut options = AgentOptions::new(dirs.project()).mcp(false);
        if let Some(token) = own {
            let seed = Extensions::default();
            seed.insert(ArtifactAuth(bearer(token)));
            options = options.extensions(seed);
        }
        let (agent, _events) = sdk.spawn(options).await.expect("spawn");
        agent.send_text("list my artifacts").await.expect("alive").expect("turn");
    }
    assert_eq!(*seen.lock().expect("seen"), ["Bearer first", "Bearer second"]);
    assert!(all_text(&model.requests()[5]).contains("needs a Claude sign-in"));
}

#[tokio::test]
async fn the_artifact_credential_decides_whether_the_tools_are_on() {
    let dirs = Dirs::new("artifact-auth-off");
    let (frame, _) = start_frame().await;
    // (the artifact credential, `artifacts(…)`, on)
    let cases: [(Option<Source>, Option<bool>, bool); 5] = [
        (None, None, false),
        (Some(Arc::new(StaticCredential(Credential::ApiKey("k2".into())))), None, false),
        (Some(Arc::new(Unreadable)), None, false),
        (Some(Arc::new(Unreadable)), Some(true), true),
        (Some(bearer("s")), Some(false), false),
    ];
    for (i, (own, forced, on)) in cases.into_iter().enumerate() {
        let model = mock([Reply::text("hi")]).await;
        let mut b = builder(&model, &dirs, &frame);
        if let Some(own) = own {
            b = b.artifact_auth(own);
        }
        if let Some(forced) = forced {
            b = b.artifacts(forced);
        }
        let sdk = b.build().await.expect("an unreadable artifact credential is not fatal");
        let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
        agent.send_text("hi").await.expect("alive").expect("turn");
        let has = tool_names(&model.requests()[0]).iter().any(|n| n == "Artifact");
        assert_eq!(has, on, "case {i}");
    }
}

/// A frame API serving one runtime contract.
async fn start_contract() -> String {
    let app = Router::new()
        .route(
            "/api/frame/contract/latest",
            get(|| async {
                axum::Json(json!({ "version": "0.2.96", "capabilities": ["db", "permissions"], "claude": true, "core": ["permissions"] }))
            }),
        )
        .route("/api/frame/contract/0.2.96/prompt", get(|| async { "Runtime contract 0.2.96\n\nUse the db." }))
        .route("/api/frame/contract/0.2.96/claude.d.ts", get(|| async { "declare const claude: Claude;" }))
        .route("/api/frame/contract/0.2.96/db.d.ts", get(|| async { "interface Db {}" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    base
}

#[tokio::test]
async fn the_artifact_tools_bring_the_skills_their_descriptions_name() {
    let dirs = Dirs::new("artifact-skills");
    let frame = start_contract().await;
    let model = mock([
        Reply::tool_use("toolu_d", "Skill", r#"{"skill":"artifact-design"}"#),
        Reply::tool_use("toolu_c", "Skill", r#"{"skill":"artifact-capabilities"}"#),
        Reply::text("ready"),
    ])
    .await;
    let sdk = builder(&model, &dirs, &frame).artifact_auth(bearer("sign-in")).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("make a poll page").await.expect("alive").expect("turn");

    let requests = model.requests();
    let first = all_text(&requests[0]);
    assert!(
        first.contains("\n- artifact-design: Design guidance and fundamentals for Artifacts. - Load before writing")
    );
    assert!(first.contains("\n- artifact-diagramming: Diagramming know-how for Artifacts"));
    assert!(first.contains("\n- artifact-capabilities: Runtime capabilities a published Artifact page can be granted"));
    let loaded = all_text(&requests[1]);
    assert!(loaded.contains("Launching skill: artifact-design"));
    assert!(loaded.contains("## Page contract \u{2014} read before your first publish"));
    assert!(loaded.contains("## Fundamentals for every artifact"));
    let caps = all_text(&requests[2]);
    assert!(caps.contains("# Artifact runtime capabilities"), "{caps}");
    assert!(caps.contains("**Available capabilities:** `db` \u{2014} the complete set"));
    assert!(caps.contains("Read the steps below with that substitution.\n\nRuntime contract 0.2.96\n\nUse the db."));
    let base = caps.split("Base directory for this skill: ").nth(1).and_then(|r| r.lines().next()).expect("base dir");
    let defs = std::fs::read_to_string(std::path::Path::new(base).join("0.2.96/db.d.ts")).expect("written");
    assert_eq!(defs, "interface Db {}");
}
