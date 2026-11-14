//! A publish never replaces or removes a published file this session has not
//! seen as it is now, unless the call names it in `overwrite_unread` (and
//! then only one never read, never one that changed after it was read).

mod common;

use common::{Agent, Mock, Reply, Tokens, source, tools_for};
use eos_agent_artifacts::{ArtifactSession, viewer_url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PAGE: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn deployed(version: &str, seq: u64) -> Reply {
    Reply::json(200, json!({ "slug": PAGE, "version": version, "seq": seq }))
}

fn boot(ver: &str, files: Value) -> Reply {
    Reply::json(
        200,
        json!({ "ver": ver, "assetToken": "at", "title": "Plan", "perm": { "role": "owner" }, "files": files }),
    )
}

/// A multi-file artifact at v3 whose `data.css` this session never saw.
fn live_with_unseen_css(mock: &Mock) {
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        boot(
            "v3",
            json!([
                { "path": "index.html", "contentType": "text/html", "sha256": sha(b"<p>live</p>") },
                { "path": "data.css", "contentType": "text/css", "size": 9, "sha256": sha(b"p{color:0}") }
            ]),
        )
    });
    mock.on("GET", &source(PAGE, "v3", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("GET", &source(PAGE, "v3", "data.css"), |_| Reply::content("p{color:0}", "text/css"));
}

#[tokio::test]
async fn an_unread_file_is_refused_until_read_or_acknowledged() {
    let mock = Mock::start().await;
    live_with_unseen_css(&mock);
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed("v4", 4));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    agent.write("data.css", b"p{color:1}");
    let url = viewer_url(PAGE);
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url })).await;
    assert!(!err, "{text}");

    let publish = json!({ "url": url, "file_path": "p.html", "files": { "data.css": "data.css" } });
    let (err, text) = agent.call(&tools[0], publish.clone()).await;
    assert!(err, "{text}");
    assert!(text.starts_with("Nothing was published or removed"), "{text}");
    assert!(text.contains("not read as published or listed by you in this session:\n- \"data.css\""), "{text}");
    assert!(text.contains("`overwrite_unread`"), "{text}");
    assert!(mock.find("POST", "/api/frame/deploy/direct").is_empty(), "nothing was sent");

    // Named in `overwrite_unread` (spelled with `./`): it goes through.
    let mut acknowledged = publish.clone();
    acknowledged["overwrite_unread"] = json!(["./data.css"]);
    let (err, text) = agent.call(&tools[0], acknowledged).await;
    assert!(!err, "{text}");
    assert_eq!(mock.find("POST", "/api/frame/deploy/direct").len(), 1);
}

#[tokio::test]
async fn reading_the_file_first_lets_the_publish_through_also_after_a_resume() {
    let mock = Mock::start().await;
    live_with_unseen_css(&mock);
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed("v4", 4));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let mut agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    agent.write("data.css", b"p{color:1}");
    let url = viewer_url(PAGE);
    let page = json!({ "action": "read", "url": url });
    let page_result = agent.call(&tools[0], page.clone()).await;
    let file = json!({ "action": "read", "url": url, "path": "data.css" });
    let file_result = agent.call(&tools[0], file.clone()).await;
    assert!(!file_result.0 && file_result.1.contains("at version v3"), "{}", file_result.1);

    let mut history = exchange("toolu_1", page, page_result);
    history.extend(exchange("toolu_2", file, file_result));
    agent.resume(&tools[0], &history).await;
    let publish = json!({ "url": url, "file_path": "p.html", "files": { "data.css": "data.css" } });
    let (err, text) = agent.call(&tools[0], publish).await;
    assert!(!err, "the resumed session still knows it read data.css: {text}");
}

#[tokio::test]
async fn a_file_changed_after_it_was_read_is_refused_even_when_acknowledged() {
    let mock = Mock::start().await;
    mock.once("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        boot("v3", json!([{ "path": "index.html" }, { "path": "data.css", "sha256": sha(b"p{color:0}") }]))
    });
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        boot("v5", json!([{ "path": "index.html" }, { "path": "data.css", "size": 10, "sha256": sha(b"p{color:9}") }]))
    });
    mock.on("GET", &source(PAGE, "v3", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("GET", &source(PAGE, "v3", "data.css"), |_| Reply::content("p{color:0}", "text/css"));
    mock.on("GET", &source(PAGE, "v5", ""), |_| Reply::content("<p>live</p>", "text/html"));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    agent.write("data.css", b"p{color:1}");
    let url = viewer_url(PAGE);
    // The read of the file pins it at v3; the page read that follows sees v5
    // (someone else published since).
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url, "path": "data.css" })).await;
    assert!(!err, "{text}");
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url })).await;
    assert!(!err, "{text}");

    let publish = json!({ "url": url, "file_path": "p.html", "files": { "data.css": "data.css" },
        "overwrite_unread": ["data.css"], "force": true });
    let (err, text) = agent.call(&tools[0], publish).await;
    assert!(err, "{text}");
    assert!(text.contains("Changed since you last saw them:\n- \"data.css\": you last saw it at version v3; someone has since replaced it (version v5)"), "{text}");
    assert!(text.contains("`force` does not override this."), "{text}");
    assert!(mock.find("POST", "/api/frame/deploy/direct").is_empty());
}

#[tokio::test]
async fn own_publishes_count_as_seen_and_another_session_has_not_seen_them() {
    let mock = Mock::start().await;
    mock.once("POST", "/api/frame/deploy/direct", |_| deployed("v1", 1));
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed("v2", 2));
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        boot("v1", json!([{ "path": "index.html" }, { "path": "site.css", "sha256": sha(b"body{color:red}") }]))
    });
    mock.on("GET", &source(PAGE, "v1", ""), |_| Reply::content("<p>one</p>", "text/html"));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>one</p>");
    agent.write("site.css", b"body{color:red}");
    let (err, text) =
        agent.call(&tools[0], json!({ "file_path": "p.html", "files": { "site.css": "site.css" } })).await;
    assert!(!err, "{text}");
    agent.write("site.css", b"body{color:blue}");
    let (err, text) =
        agent.call(&tools[0], json!({ "file_path": "p.html", "files": { "site.css": "site.css" } })).await;
    assert!(!err, "its own publish is what it saw: {text}");

    // Another session (its own tree) read the page but never saw site.css.
    let other = Agent::new();
    other.ext().insert(ArtifactSession("other".into()));
    other.write("p.html", b"<p>two</p>");
    other.write("site.css", b"body{color:green}");
    let url = viewer_url(PAGE);
    let (err, text) = other.call(&tools[0], json!({ "action": "read", "url": url })).await;
    assert!(!err, "{text}");
    let (err, text) =
        other.call(&tools[0], json!({ "url": url, "file_path": "p.html", "files": { "site.css": "site.css" } })).await;
    assert!(err && text.contains("- \"site.css\""), "{text}");
}

#[tokio::test]
async fn a_single_page_and_new_paths_need_no_read() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v3", "assetToken": "at", "title": "Plan", "perm": { "role": "owner" } }))
    });
    mock.on("GET", &source(PAGE, "v3", ""), |_| Reply::content("<p>live</p>", "text/html"));
    mock.on("POST", "/api/frame/deploy/direct", |_| deployed("v4", 4));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("p.html", b"<p>mine</p>");
    agent.write("new.css", b"p{}");
    let url = viewer_url(PAGE);
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url })).await;
    assert!(!err, "{text}");
    let (err, text) =
        agent.call(&tools[0], json!({ "url": url, "file_path": "p.html", "files": { "new.css": "new.css" } })).await;
    assert!(!err, "{text}");
}

fn exchange(id: &str, input: Value, (is_error, text): (bool, String)) -> Vec<eos_agent_core::StoredMessage> {
    use eos_agent_types::{ContentBlock, Role, ToolResultContent};
    let call = ContentBlock::ToolUse {
        id: id.into(),
        name: "Artifact".into(),
        input: serde_json::value::to_raw_value(&input).expect("raw"),
    };
    let result =
        ContentBlock::ToolResult { tool_use_id: id.into(), content: Some(ToolResultContent::Text(text)), is_error };
    vec![
        eos_agent_core::StoredMessage::new(Role::Assistant, &[call]).expect("call"),
        eos_agent_core::StoredMessage::new(Role::User, &[result]).expect("result"),
    ]
}
