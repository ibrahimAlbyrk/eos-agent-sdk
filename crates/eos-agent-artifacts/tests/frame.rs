//! The tools against a local stand-in for the frame API.

mod common;

use base64::Engine;
use common::{Agent, Mock, Reply, Tokens, source, tools_for};
use eos_agent_artifacts::viewer_url;
use serde_json::json;

const PAGE: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";
const DESIGN_TYPE: &str = "11111111-2222-4333-8444-555555555555";
const DS_TYPE: &str = "22222222-3333-4444-8555-666666666666";
const DS_ONE: &str = "33333333-4444-4555-8666-777777777777";
const MADE: &str = "44444444-5555-4666-8777-888888888888";

fn deployed(slug: &str, version: &str, seq: u64) -> Reply {
    Reply::json(200, json!({ "slug": slug, "version": version, "seq": seq }))
}

#[tokio::test]
async fn sends_the_frame_headers_and_lists_artifacts() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/frames?limit=25", |_| {
        Reply::json(
            200,
            json!({ "frames": [
                { "slug": PAGE, "title": "Q3 Plan", "updatedAt": "2026-11-01T10:00:00Z" },
                { "slug": "not-a-uuid", "title": "dropped" },
                { "slug": MADE, "title": "Gone", "softDeleted": true, "updatedAt": "2026-11-01T10:00:00Z" }
            ]}),
        )
    });
    let tools = tools_for(&mock, Tokens::new("tok-1", None));
    let agent = Agent::new();
    let (err, text) = agent.call(&tools[0], json!({ "action": "list" })).await;
    assert!(!err, "{text}");
    assert!(text.contains(&viewer_url(PAGE)) && text.contains("Q3 Plan"), "{text}");
    assert!(!text.contains("dropped") && !text.contains("Gone"), "{text}");

    let req = &mock.requests()[0];
    assert_eq!(req.header("authorization"), "Bearer tok-1");
    assert_eq!(req.header("anthropic-beta"), "oauth-2025-04-20");
    assert_eq!(req.header("x-frame-cp"), "go");
    assert_eq!(req.header("x-frame-surface"), "code");
    assert_eq!(req.header("x-frame-platform"), "cli");
    assert_eq!(req.header("x-frame-session-id"), "session_test");
    assert!(req.header("user-agent").starts_with("eos-agent/"));
    assert!(req.headers.get("x-frame-client-version").is_none(), "no Claude Code version is claimed");
    assert!(req.header("accept").starts_with("application/json"));
}

#[tokio::test]
async fn a_refused_token_is_renewed_and_retried_once() {
    let mock = Mock::start().await;
    mock.once("GET", "/api/frame/frames?limit=25", |_| Reply::json(401, json!({ "error": "expired" })));
    mock.on("GET", "/api/frame/frames?limit=25", |r| {
        assert_eq!(r.header("authorization"), "Bearer tok-2");
        Reply::json(200, json!({ "frames": [] }))
    });
    let tokens = Tokens::new("tok-1", Some("tok-2"));
    let tools = tools_for(&mock, tokens.clone());
    let (err, text) = Agent::new().call(&tools[0], json!({ "action": "list" })).await;
    assert!(!err, "{text}");
    let auths: Vec<String> = mock.requests().iter().map(|r| r.header("authorization").to_owned()).collect();
    assert_eq!(auths, ["Bearer tok-1", "Bearer tok-2"]);
    assert_eq!(tokens.refusals.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_token_that_cannot_be_renewed_is_not_retried() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/frames?limit=25", |_| Reply::json(401, json!({ "error": "expired" })));
    let tools = tools_for(&mock, Tokens::new("tok-1", None));
    let (err, text) = Agent::new().call(&tools[0], json!({ "action": "list" })).await;
    assert!(err && text.contains("sign in again"), "{text}");
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn design_flow_quickstart_create_fill_and_read() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/types?scopes=anthropic", |_| {
        Reply::json(200, json!({ "types": [
            { "slug": DESIGN_TYPE, "title": "Design", "description": "A canvas design", "listings": [{ "tier": "core" }] },
            { "slug": DS_TYPE, "title": "Design System", "listings": [{ "tier": "core" }] }
        ]}))
    });
    mock.on("GET", &format!("/api/frame/types/{DS_TYPE}/instances?limit=200"), |_| {
        Reply::json(200, json!({ "instances": [{ "slug": DS_ONE, "title": "Acme DS", "default": "org" }] }))
    });
    mock.on("POST", &format!("/api/frame/types/{DESIGN_TYPE}/create"), |r| {
        assert_eq!(r.json(), json!({ "target_type_version": null, "title": "Launch poster" }));
        Reply::json(200, json!({ "slug": MADE, "version": "v1", "title": "Launch poster" }))
    });
    mock.on("GET", &format!("/api/frame/types/{DESIGN_TYPE}/files/SKILL.md"), |_| {
        Reply::json(200, json!({ "path": "SKILL.md", "text": "Write project/design.json, then publish it." }))
    });
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed(MADE, "v2", 2));
    // Before the first fill, only the type's own page is published.
    mock.once("GET", &format!("/api/frame/{MADE}?via=model_read"), |_| {
        Reply::json(
            200,
            json!({ "ver": "v1", "assetToken": "at", "perm": { "role": "owner" }, "typeInstance": true,
            "typeSlug": DESIGN_TYPE, "files": [{ "path": "index.html", "src": { "slug": DESIGN_TYPE, "ver": "t1" } }] }),
        )
    });
    mock.on("GET", &format!("/api/frame/{MADE}?via=model_read"), |_| {
        Reply::json(
            200,
            json!({ "ver": "v2", "assetToken": "at", "title": "Launch poster", "perm": { "role": "owner" },
            "typeInstance": true, "files": ["project/design.json"] }),
        )
    });
    mock.on("GET", &source(MADE, "v2", "project/design.json"), |r| {
        assert_eq!(r.header("authorization"), "", "the content host never gets the sign-in");
        Reply::content("{\"layers\":[]}", "application/json")
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();

    let (err, quick) = agent.call(&tools[0], json!({ "action": "quickstart", "intent": "design" })).await;
    assert!(!err, "{quick}");
    assert!(
        quick.contains(&viewer_url(DESIGN_TYPE)) && quick.contains("Acme DS") && quick.contains("(default)"),
        "{quick}"
    );

    let (err, created) = agent
        .call(
            &tools[0],
            json!({ "type_url": viewer_url(DESIGN_TYPE), "title": "Launch poster", "auto_open": "after_first_write" }),
        )
        .await;
    assert!(!err, "{created}");
    assert!(created.contains(&viewer_url(MADE)) && created.contains("Write project/design.json"), "{created}");

    agent.write("site/project/design.json", br#"{"title":"Launch","note":"line1\nline2"}"#);
    let (err, filled) = agent
        .call(&tools[0], json!({ "url": viewer_url(MADE), "file_path": "site/project/design.json", "root": "site" }))
        .await;
    assert!(!err, "{filled}");
    assert!(filled.contains("Version 2"), "{filled}");
    let body = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert_eq!(body["slug"], MADE);
    assert_eq!(body["baseVersion"], "v1");
    assert!(body.get("content").is_none());
    assert!(body.get("files").is_none(), "the frame API takes `manifest`, never `files`");
    assert_eq!(body["mode"], "patch", "a typed artifact's data files change only what they name");
    let file = &body["manifest"]["project/design.json"];
    assert_eq!(file["contentType"], "application/json");
    assert_eq!(file["content"], r#"{"title":"Launch","note":"line1\nline2"}"#);

    let (err, read) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(MADE) })).await;
    assert!(!err, "{read}");
    assert!(read.contains("owner") && read.contains("project/design.json"), "{read}");
    assert!(agent.scratch.join("artifacts").join(MADE).join("project/design.json").exists());
}

#[tokio::test]
async fn publishes_a_page_with_files_then_updates_it_pinned_to_the_version() {
    let mock = Mock::start().await;
    mock.once("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v1", 1));
    mock.once("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v2", 2));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("out/page.html", b"<html><head><title> Q3   Plan </title></head><body>\"hi\"</body></html>");
    agent.write("out/site.css", b"body{color:red}");
    let png: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    agent.write("out/logo.png", &png);

    let (err, text) = agent
        .call(&tools[0], json!({ "file_path": "out/page.html", "icon": "chart", "files": { "site.css": "out/site.css", "img/logo.png": "out/logo.png" } }))
        .await;
    assert!(!err, "{text}");
    assert!(text.contains(&viewer_url(PAGE)), "{text}");
    let first = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert_eq!(first["title"], "Q3 Plan");
    assert_eq!(first["icon"], "chart");
    assert!(first.get("slug").is_none() && first.get("baseVersion").is_none());
    assert!(first.get("files").is_none() && first.get("content").is_none(), "{first}");
    assert_eq!(first["mode"], "replace", "a new artifact's whole file set");
    assert!(first["manifest"]["index.html"]["content"].as_str().is_some_and(|c| c.contains("\"hi\"")));
    assert_eq!(first["manifest"]["site.css"]["contentType"], "text/css");
    let logo = first["manifest"]["img/logo.png"]["content"].as_str().expect("base64");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(logo).expect("decode"), png);

    // The same file again updates the same artifact, pinned to what it saw.
    let (err, text) = agent.call(&tools[0], json!({ "file_path": "out/page.html" })).await;
    assert!(!err, "{text}");
    let second = mock.find("POST", "/api/frame/deploy/direct")[1].json();
    assert_eq!(second["slug"], PAGE);
    assert_eq!(second["baseVersion"], "v1");
    assert!(second["content"].as_str().is_some_and(|c| c.starts_with("<html>")), "a lone page goes as `content`");
}

/// The call and its result as a transcript holds them.
fn exchange(input: serde_json::Value, (is_error, text): (bool, String)) -> Vec<eos_agent_core::StoredMessage> {
    use eos_agent_types::{ContentBlock, Role, ToolResultContent};
    let call = ContentBlock::ToolUse {
        id: "toolu_1".into(),
        name: "Artifact".into(),
        input: serde_json::value::to_raw_value(&input).expect("raw"),
    };
    let result = ContentBlock::ToolResult {
        tool_use_id: "toolu_1".into(),
        content: Some(ToolResultContent::Text(text)),
        is_error,
    };
    vec![
        eos_agent_core::StoredMessage::new(Role::Assistant, &[call]).expect("call"),
        eos_agent_core::StoredMessage::new(Role::User, &[result]).expect("result"),
    ]
}

#[tokio::test]
async fn a_resumed_conversation_keeps_the_versions_it_saw() {
    let mock = Mock::start().await;
    mock.once("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v1", 1));
    mock.once("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v2", 2));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let mut agent = Agent::new();
    agent.write("p.html", b"<title>Plan</title><p>one</p>");
    let first = json!({ "file_path": "p.html", "icon": "chart" });
    let result = agent.call(&tools[0], first.clone()).await;
    assert!(!result.0, "{}", result.1);

    agent.resume(&tools[0], &exchange(first, result)).await;
    agent.write("p.html", b"<title>Plan</title><p>two</p>");
    let (err, text) = agent.call(&tools[0], json!({ "file_path": "p.html" })).await;
    assert!(!err, "the same file still updates its artifact: {text}");
    let second = mock.find("POST", "/api/frame/deploy/direct")[1].json();
    assert_eq!(second["slug"], PAGE);
    assert_eq!(second["baseVersion"], "v1", "pinned to the version the conversation saw");
    assert!(mock.find("GET", &format!("/api/frame/{PAGE}")).is_empty(), "no refusal read the live copy");
}

#[tokio::test]
async fn publishing_to_an_unseen_artifact_hands_back_the_live_version() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v7", "assetToken": "at", "title": "Plan", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &source(PAGE, "v7", ""), |_| Reply::content("<p>live</p>", "text/html"));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    let (err, text) = agent.call(&tools[0], json!({ "url": viewer_url(PAGE), "file_path": "p.html" })).await;
    assert!(err && text.contains("v7"), "{text}");
    assert!(mock.find("POST", "/api/frame/deploy/direct").is_empty());
    let saved = agent.scratch.join("artifacts").join(PAGE).join("index.html");
    assert_eq!(std::fs::read_to_string(saved).expect("saved"), "<p>live</p>");
}

#[tokio::test]
async fn a_conflict_refreshes_the_live_copy() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v1", "assetToken": "at", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &source(PAGE, "v1", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("POST", "/api/frame/deploy/direct", |_| {
        Reply::json(
            409,
            json!({ "conflict": true, "live": "v2", "createdAt": "2026-11-08T07:50:45Z", "liveEntry": "publish" }),
        )
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    let (err, _) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE) })).await;
    assert!(!err);
    let (err, text) = agent.call(&tools[0], json!({ "url": viewer_url(PAGE), "file_path": "p.html" })).await;
    assert!(err && text.contains("changed since") && text.contains("index.html"), "{text}");
    assert!(text.contains("version v2 was published at 2026-11-08T07:50:45Z"), "{text}");
}

/// A file to publish may be anywhere the process can read, as for Read: the
/// permission gate decides, not the working directories.
#[tokio::test]
async fn files_outside_the_workspace_are_published() {
    let mock = Mock::start().await;
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v1", 1));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let outside = agent.dir.join("outside.html");
    std::fs::write(&outside, "<p>from elsewhere</p>").expect("write");
    let (err, text) = agent.call(&tools[0], json!({ "file_path": outside.to_string_lossy() })).await;
    assert!(!err, "{text}");
    let deploy = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert!(deploy.to_string().contains("from elsewhere"), "{deploy}");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, agent.cwd.join("link.html")).expect("symlink");
        let (err, text) = agent.call(&tools[0], json!({ "file_path": "link.html" })).await;
        assert!(!err, "{text}");
    }
}

#[tokio::test]
async fn a_large_file_is_staged_through_upload() {
    let mock = Mock::start().await;
    mock.on("POST", "/api/frame/deploy/prepare", |r| {
        let shas = r.json()["shas"].clone();
        Reply::json(200, json!({ "slug": PAGE, "missing": shas }))
    });
    mock.on("POST", "/api/frame/upload", |_| Reply::json(200, json!({})));
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v1", 1));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>video below</p>");
    let video: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
    agent.write("clip.mp4", &video);
    let (err, text) =
        agent.call(&tools[0], json!({ "file_path": "p.html", "files": { "clip.mp4": "clip.mp4" } })).await;
    assert!(!err, "{text}");

    let sha = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(&video))
    };
    let upload = mock.find("POST", "/api/frame/upload")[0].json();
    assert_eq!(upload["slug"], PAGE);
    let sent = upload["files"][0]["content"].as_str().expect("content");
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(sent).expect("b64"), video);
    let deploy = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert_eq!(deploy["slug"], PAGE);
    assert_eq!(deploy["manifest"]["clip.mp4"], json!({ "sha256": sha, "contentType": "video/mp4" }));
    assert!(deploy["manifest"]["index.html"]["content"].is_string());
    assert_eq!(deploy["mode"], "replace");
}

// Shapes confirmed against the real frame API on 2026-11-08 (see the crate docs).

#[tokio::test]
async fn reads_come_from_the_content_host_without_the_sign_in() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v3", "assetToken": "at", "perm": { "role": "owner" } }))
    });
    // No stored source: the served page is read instead.
    mock.on("GET", &source(PAGE, "v3", ""), |_| Reply::json(404, json!({})));
    mock.on("GET", &format!("/_host/{PAGE}/_f/v3/?__frame_t=at"), |_| Reply::content("<p>served</p>", "text/html"));
    mock.on("GET", &source(PAGE, "v3", "gone.css"), |_| Reply::json(404, json!({})));
    mock.on("GET", &format!("/_host/{PAGE}/_f/v3/gone.css?__frame_t=at"), |_| Reply::json(404, json!({})));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE) })).await;
    assert!(!err && text.contains("<p>served</p>"), "{text}");
    for req in mock.find("GET", "/_host/") {
        assert_eq!(req.header("authorization"), "", "the content host never sees the bearer");
        assert_eq!(req.header("anthropic-beta"), "");
    }
    let (err, text) =
        agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE), "path": "gone.css" })).await;
    assert!(!err && text.contains("no file at that path"), "{text}");

    // `contentVia: frame` reads through the API with the asset token.
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v3", "assetToken": "at", "contentVia": "frame", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &format!("/api/frame/content/{PAGE}/"), |r| {
        assert_eq!(r.header("x-frame-asset-token"), "at");
        Reply::content("<p>via api</p>", "text/html")
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE) })).await;
    assert!(!err && text.contains("<p>via api</p>"), "{text}");
}

#[tokio::test]
async fn a_typed_read_leaves_out_the_types_files() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{MADE}?via=model_read"), |_| {
        Reply::json(
            200,
            json!({ "ver": "v4", "assetToken": "at", "perm": { "role": "owner" }, "typeInstance": true,
                "typeSlug": DESIGN_TYPE, "files": [
                    { "path": "artifact-type/app.js", "src": { "slug": DESIGN_TYPE, "ver": "t1" } },
                    { "path": "project/canvas.json", "contentType": "application/json" }
                ] }),
        )
    });
    mock.on("GET", &source(MADE, "v4", "project/canvas.json"), |_| Reply::content("{}", "application/json"));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(MADE) })).await;
    assert!(!err && text.contains("Its own files: project/canvas.json."), "{text}");
    assert!(mock.find("GET", "/_host/").iter().all(|r| !r.uri.contains("app.js")), "the type's code is not fetched");
}

#[tokio::test]
async fn declared_capabilities_carry_the_contract() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/contract/latest", |_| {
        Reply::json(200, json!({ "version": "0.2.96", "capabilities": ["assets", "db", "user"] }))
    });
    mock.on("GET", "/api/frame/contract/0.2.50", |_| {
        Reply::json(200, json!({ "version": "0.2.50", "capabilities": ["db"] }))
    });
    mock.on("GET", &format!("/api/frame/read/{PAGE}"), |_| Reply::json(200, json!({ "contract": "0.2.50" })));
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v1", "assetToken": "at", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &source(PAGE, "v1", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v2", 2));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>db</p>");

    // A first publish declaring capabilities gets the latest contract.
    let (err, text) = agent
        .call(&tools[0], json!({ "file_path": "p.html", "icon": "list", "capabilities": { "db": {}, "user": {} } }))
        .await;
    assert!(!err, "{text}");
    let first = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert_eq!(first["contract"], "0.2.96");
    assert_eq!(first["capabilities"], json!({ "db": {}, "user": {} }));

    // An update keeps the artifact's stored contract, and checks the names against it.
    let (err, _) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE) })).await;
    assert!(!err);
    let (err, text) = agent
        .call(&tools[0], json!({ "url": viewer_url(PAGE), "file_path": "p.html", "capabilities": { "assets": {} } }))
        .await;
    assert!(err && text.contains("unknown capability assets") && text.contains("0.2.50"), "{text}");
    assert_eq!(mock.find("POST", "/api/frame/deploy/direct").len(), 1, "nothing published");
    let (err, text) = agent
        .call(&tools[0], json!({ "url": viewer_url(PAGE), "file_path": "p.html", "capabilities": { "db": {} } }))
        .await;
    assert!(!err, "{text}");
    assert_eq!(mock.find("POST", "/api/frame/deploy/direct")[1].json()["contract"], "0.2.50");
}

#[tokio::test]
async fn an_update_with_files_is_a_pinned_patch() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v5", "assetToken": "at", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &source(PAGE, "v5", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed(PAGE, "v6", 6));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>page</p>");
    agent.write("a.css", b"p{}");
    let (err, _) = agent.call(&tools[0], json!({ "action": "read", "url": viewer_url(PAGE) })).await;
    assert!(!err);
    let (err, text) = agent
        .call(
            &tools[0],
            json!({ "url": viewer_url(PAGE), "file_path": "p.html", "files": { "a.css": "a.css", "old.css": null } }),
        )
        .await;
    assert!(!err, "{text}");
    let body = mock.find("POST", "/api/frame/deploy/direct")[0].json();
    assert_eq!((&body["mode"], &body["baseVersion"], &body["slug"]), (&json!("patch"), &json!("v5"), &json!(PAGE)));
    assert_eq!(body["manifest"]["old.css"], serde_json::Value::Null);
    assert!(body["manifest"].get("old.css").is_some());
}
