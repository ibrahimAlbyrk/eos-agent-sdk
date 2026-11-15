//! The `artifact-capabilities` skill follows the contract the session's
//! target artifact is pinned to, as Claude Code's does.

mod common;

use std::sync::Arc;

use common::{Agent, Mock, Reply, Tokens, frame_config, source};
use eos_agent_artifacts::{ArtifactAuth, ArtifactSession, WatchConfig, WatchNotifier, kit, viewer_url};
use eos_agent_core::BoxFuture;
use serde_json::json;

const PAGE: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";
const OTHER: &str = "44444444-5555-4666-8777-888888888888";
const GONE: &str = "55555555-6666-4777-8888-999999999999";

struct Quiet;
impl WatchNotifier for Quiet {
    fn notify<'a>(&'a self, _agent: &'a str, _text: String, _wake: bool) -> BoxFuture<'a, bool> {
        Box::pin(std::future::ready(true))
    }
}

const PINNED: &str = "_This guidance is pinned to runtime contract 0.2.50 \u{2014} the contract the target artifact";

#[tokio::test]
async fn the_skill_follows_the_target_artifacts_pinned_contract() {
    let mock = Mock::start().await;
    for v in ["latest", "0.2.96", "0.2.50"] {
        let version = if v == "latest" { "0.2.96" } else { v };
        mock.on("GET", &format!("/api/frame/contract/{v}"), move |_| {
            Reply::json(200, json!({ "version": version, "capabilities": ["db"] }))
        });
    }
    mock.on("GET", "/api/frame/contract/0.2.96/prompt", |_| Reply::content("GUIDE 96", "text/markdown"));
    mock.on("GET", "/api/frame/contract/0.2.50/prompt", |_| Reply::content("GUIDE 50", "text/markdown"));
    mock.on("POST", "/api/frame/deploy/direct", |_| Reply::json(200, json!({ "slug": PAGE, "version": "v1" })));
    mock.on("GET", &format!("/api/frame/read/{OTHER}"), |_| Reply::json(200, json!({ "contract": "0.2.50" })));
    for slug in [OTHER, GONE] {
        mock.on("GET", &format!("/api/frame/{slug}?via=model_read"), |_| {
            Reply::json(200, json!({ "ver": "v1", "assetToken": "at", "perm": { "role": "owner" } }))
        });
        mock.on("GET", &source(slug, "v1", ""), |_| Reply::content("<p>live</p>", "text/html"));
    }
    let dir = std::env::temp_dir().join(format!("eos-pins-{}", std::process::id()));
    let kit = kit(
        Some(Tokens::new("tok", None)),
        frame_config(&mock),
        Arc::new(Quiet),
        WatchConfig::default(),
        dir.clone(),
        None,
    )
    .expect("kit");
    let artifact = &kit.tools[0];
    let agent = Agent::new();
    agent.ext().insert(ArtifactSession("session-1".into()));
    let skill = || kit.capabilities.render_for(Some((agent.ext(), "a1")));

    // No target yet: the latest contract.
    let text = skill().await;
    assert!(text.contains("GUIDE 96") && !text.contains("pinned"), "{text}");

    // A publish pinned to 0.2.50 makes that artifact the target.
    agent.write("p.html", b"<p>db</p>");
    let (err, out) = agent
        .call(artifact, json!({ "file_path": "p.html", "capabilities": { "db": {} }, "contract": "0.2.50" }))
        .await;
    assert!(!err, "{out}");
    let text = skill().await;
    assert!(text.contains(&format!("{PINNED} `{PAGE}` currently runs. A carry-forward republish keeps this pin._")));
    assert!(text.contains("GUIDE 50") && !text.contains("GUIDE 96"), "{text}");
    assert!(
        text.find("**Available capabilities:**").expect("roster") < text.find(PINNED).expect("pin"),
        "the pin line follows the roster"
    );

    // A subagent of the same session sees the same target.
    let sub = Agent::new();
    sub.ext().insert(ArtifactSession("session-1".into()));
    assert!(kit.capabilities.render_for(Some((sub.ext(), "sub"))).await.contains(PINNED));

    // Reading another artifact this account owns makes it the target; its
    // pin is read back once.
    let (err, out) = agent.call(artifact, json!({ "action": "read", "url": viewer_url(OTHER) })).await;
    assert!(!err, "{out}");
    assert!(skill().await.contains(&format!("{PINNED} `{OTHER}`")));
    assert!(skill().await.contains(&format!("{PINNED} `{OTHER}`")));
    assert_eq!(mock.find("GET", &format!("/api/frame/read/{OTHER}")).len(), 1, "the pin is remembered");

    // An artifact the read-back knows nothing of has no pin, and is asked once.
    let (err, _) = agent.call(artifact, json!({ "action": "read", "url": viewer_url(GONE) })).await;
    assert!(!err);
    let text = skill().await;
    assert!(text.contains("GUIDE 96") && !text.contains("pinned"), "{text}");
    skill().await;
    assert_eq!(mock.find("GET", &format!("/api/frame/read/{GONE}")).len(), 1);

    // Another session has its own target.
    let other = Agent::new();
    other.ext().insert(ArtifactSession("session-2".into()));
    assert!(!kit.capabilities.render_for(Some((other.ext(), "b1"))).await.contains("pinned"));
    let _ = std::fs::remove_dir_all(dir);
}

/// Each agent's calls go out on the sign-in its extensions carry; an agent
/// with none uses the one the tools were built with, or is told to sign in.
#[tokio::test]
async fn an_agent_calls_on_its_own_sign_in() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/frames?limit=25", |_| Reply::json(200, json!({ "frames": [] })));
    let dir = std::env::temp_dir().join(format!("eos-own-sign-in-{}", std::process::id()));
    let build = |auth| kit(auth, frame_config(&mock), Arc::new(Quiet), WatchConfig::default(), dir.clone(), None);
    let shared = build(Some(Tokens::new("shared", None))).expect("kit");
    let unsigned = build(None).expect("kit");
    let bearers = || -> Vec<String> {
        mock.find("GET", "/api/frame/frames").iter().map(|r| r.header("authorization").to_owned()).collect()
    };

    let own = Agent::new();
    own.ext().insert(ArtifactAuth(Tokens::new("own", None)));
    let plain = Agent::new();
    for (kit, agent) in [(&shared, &own), (&shared, &plain), (&unsigned, &own)] {
        let (err, text) = agent.call(&kit.tools[0], json!({ "action": "list" })).await;
        assert!(!err, "{text}");
    }
    assert_eq!(bearers(), ["Bearer own", "Bearer shared", "Bearer own"]);

    let (err, text) = plain.call(&unsigned.tools[0], json!({ "action": "list" })).await;
    assert!(err && text.contains("needs a Claude sign-in"), "{text}");
    assert_eq!(bearers().len(), 3, "nothing is sent without a sign-in");
}
