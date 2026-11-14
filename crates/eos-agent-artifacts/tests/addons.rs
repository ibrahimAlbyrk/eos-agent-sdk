//! Comments, the shared database, assets, delete and pin against the stand-in.

mod common;

use common::{Agent, Mock, Reply, Tokens, source, tools_for};
use eos_agent_artifacts::viewer_url;
use serde_json::json;

const PAGE: &str = "0f8e2a9c-1b3d-4e5f-8a7b-6c5d4e3f2a1b";
const ASSET: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn comments_read_reply_and_resolve() {
    let mock = Mock::start().await;
    mock.on("GET", &format!("/api/frame/comments/{PAGE}?names=1"), |_| {
        Reply::json(200, json!({ "threads": [
            { "id": "t1", "anchor": { "label": "Header" }, "comments": [
                { "id": "c1", "author": { "role": "owner" }, "text": "Make it blue", "to_claude_at": "2026-11-01T00:00:00Z" }
            ]},
            { "id": "t2", "resolved_at": "2026-11-02T00:00:00Z", "comments": [
                { "id": "c2", "author": { "role": "viewer" }, "text": "Nice" }
            ]}
        ]}))
    });
    mock.on("POST", &format!("/api/frame/comments/{PAGE}/t1"), |r| {
        assert_eq!(r.json(), json!({ "text": "Done — it is blue now." }));
        Reply::json(200, json!({ "id": "c3" }))
    });
    mock.on("POST", &format!("/api/frame/comments/{PAGE}/t2"), |_| {
        Reply::json(403, json!({ "error": "not activated" }))
    });
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(200, json!({ "ver": "v9", "assetToken": "at" }))
    });
    mock.on("POST", &format!("/api/frame/comments/{PAGE}/t1/resolve"), |r| {
        assert_eq!(r.json(), json!({ "resolved": true, "after_version": "v9" }));
        Reply::json(200, json!({}))
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let url = viewer_url(PAGE);

    let (err, text) = agent.call(&tools[1], json!({ "action": "read", "url": url })).await;
    assert!(!err, "{text}");
    assert!(text.contains("Thread t1 — open — activated for Claude — on \"Header\""), "{text}");
    assert!(text.contains("Thread t2 — resolved — NOT activated") && text.contains("Make it blue"), "{text}");

    let (err, text) = agent
        .call(&tools[1], json!({ "action": "reply", "url": url, "thread_id": "t1", "text": "Done — it is blue now." }))
        .await;
    assert!(!err, "{text}");
    let (err, text) =
        agent.call(&tools[1], json!({ "action": "reply", "url": url, "thread_id": "t2", "text": "x" })).await;
    assert!(!err && text.contains("not activated"), "guidance, not an error: {text}");
    let (err, text) = agent.call(&tools[1], json!({ "action": "resolve", "url": url, "thread_id": "t1" })).await;
    assert!(!err, "{text}");
    let (_, text) = agent.call(&tools[1], json!({ "action": "watch", "url": url })).await;
    assert!(text.contains("can't hold artifact watches"), "{text}");
}

#[tokio::test]
async fn database_reads_writes_batches_and_me() {
    let mock = Mock::start().await;
    mock.on("POST", "/api/frame/db/agent", |r| {
        let body = r.json();
        assert_eq!(body["slug"], PAGE);
        match body["verb"].as_str().expect("verb") {
            "whoami" => Reply::json(200, json!({ "id": "u_abc" })),
            "get" => {
                assert_eq!(body["args"], json!({ "path": "boards/b1" }));
                Reply::json(200, json!({ "exists": true, "data": { "name": "Roadmap" }, "version": 3 }))
            }
            "query" => {
                assert_eq!(body["args"]["collection"], "data/users/u_abc/decks/cards");
                assert_eq!(body["args"]["where"], json!([{ "field": "n", "op": "gt", "value": 2 }]));
                assert_eq!(body["asLevel"], "view");
                Reply::json(
                    200,
                    json!({ "docs": [{ "id": "d1", "data": { "n": 3 }, "version": 1 }], "nextCursor": "k" }),
                )
            }
            "update" => {
                assert_eq!(body["args"], json!({ "path": "boards/b1", "data": { "name": "Plan" }, "ifVersion": 3 }));
                Reply::json(200, json!({ "version": 4 }))
            }
            "set" => Reply::json(409, json!({ "error": "version_mismatch", "current": 5 })),
            "batch" => {
                let ops = body["args"]["ops"].as_array().expect("ops");
                assert_eq!(ops[0], json!({ "op": "set", "path": "rows/r1", "data": { "a": 1 }, "replace": true }));
                assert_eq!(ops[1], json!({ "op": "delete", "path": "rows/r2", "ifVersion": 2 }));
                Reply::json(200, json!({ "applied": 2, "atomic": true }))
            }
            other => panic!("unexpected verb {other}"),
        }
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let url = viewer_url(PAGE);

    let (err, text) =
        agent.call(&tools[2], json!({ "action": "get", "url": url, "collection": "boards", "doc_id": "b1" })).await;
    assert!(!err && text.contains("Roadmap"), "{text}");
    let (err, text) = agent
        .call(
            &tools[2],
            json!({ "action": "query", "url": url, "collection": "data/users/me/decks/cards", "as_level": "view",
            "query": { "where": [["n", ">", 2]] } }),
        )
        .await;
    assert!(!err && text.contains("\"nextCursor\": \"k\""), "{text}");
    agent.write("doc.json", br#"{"name":"Plan"}"#);
    let (err, text) = agent
        .call(&tools[2], json!({ "action": "update", "url": url, "collection": "boards", "doc_id": "b1", "file_path": "doc.json", "if_version": 3 }))
        .await;
    assert!(!err && text.contains("\"version\":4"), "{text}");
    let (err, text) = agent
        .call(
            &tools[2],
            json!({ "action": "set", "url": url, "collection": "boards", "doc_id": "b1", "data": {}, "if_version": 1 }),
        )
        .await;
    assert!(err && text.contains("Nothing was written"), "{text}");
    let (err, text) = agent
        .call(
            &tools[2],
            json!({ "action": "batch", "url": url, "writes": [
                { "op": "set", "collection": "rows", "doc_id": "r1", "data": { "a": 1 } },
                { "op": "delete", "collection": "rows", "doc_id": "r2", "if_version": 2 }
            ]}),
        )
        .await;
    assert!(!err, "{text}");
    let (err, text) = agent.call(&tools[2], json!({ "action": "list", "url": url, "collection": "boards/b1" })).await;
    assert!(err && text.contains("document path"), "{text}");
}

#[tokio::test]
async fn database_reads_can_be_saved_as_files() {
    let mock = Mock::start().await;
    mock.on("POST", "/api/frame/db/agent", |_| {
        Reply::json(200, json!({ "docs": [{ "id": "d1", "data": { "n": 1 }, "version": 1 }, { "id": "d2", "data": {}, "version": 2 }] }))
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let (err, text) = agent
        .call(&tools[2], json!({ "action": "list", "url": viewer_url(PAGE), "collection": "rows", "out_dir": "dump" }))
        .await;
    assert!(!err, "{text}");
    let saved = std::fs::read_to_string(agent.cwd.join("dump/rows/d2.json")).expect("saved");
    assert!(saved.contains("\"version\": 2"));
}

#[tokio::test]
async fn assets_upload_stream_list_and_delete() {
    let mock = Mock::start().await;
    let png: Vec<u8> = (0..300_000u32).map(|i| (i % 7) as u8).collect();
    let expected = png.clone();
    mock.on("POST", &format!("/api/frame/blob/{PAGE}/agent-upload"), move |r| {
        assert_eq!(r.header("content-type"), "image/png");
        assert_eq!(&r.body[..], &expected[..]);
        Reply::json(200, json!({ "opaque_id": ASSET, "size_bytes": expected.len(), "content_type": "image/png" }))
    });
    mock.on("POST", &format!("/api/frame/blob/{PAGE}/agent-list"), |r| {
        assert_eq!(r.json(), json!({ "limit": 50, "after": "n1" }));
        Reply::json(200, json!({ "assets": [{ "opaque_id": ASSET }], "usage": { "files": 1 } }))
    });
    mock.on("POST", &format!("/api/frame/blob/{PAGE}/{ASSET}/agent-delete"), |_| {
        Reply::json(200, json!({ "deleted": true }))
    });
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    agent.write("logo.png", &png);
    let url = viewer_url(PAGE);

    let (err, text) = agent.call(&tools[0], json!({ "url": url, "file_path": "logo.png", "asset": true })).await;
    assert!(!err && text.contains(&format!("/_blob/{ASSET}")), "{text}");
    let (err, text) =
        agent.call(&tools[0], json!({ "action": "list", "scope": "assets", "url": url, "after": "n1" })).await;
    assert!(!err && text.contains(ASSET), "{text}");
    let (err, text) = agent.call(&tools[0], json!({ "action": "delete", "url": url, "path": ASSET })).await;
    assert!(!err && text.contains("Deleted the asset"), "{text}");
}

#[tokio::test]
async fn delete_pin_open_and_read_a_page() {
    let mock = Mock::start().await;
    mock.on("DELETE", &format!("/api/frame/{PAGE}"), |_| Reply::json(200, json!({})));
    mock.on("POST", &format!("/api/frame/favorite/{PAGE}"), |_| Reply::json(200, json!({})));
    mock.on("DELETE", &format!("/api/frame/favorite/{PAGE}"), |_| Reply::json(200, json!({})));
    mock.on("GET", &format!("/api/frame/{PAGE}?via=model_read"), |_| {
        Reply::json(
            200,
            json!({ "ver": "v3", "assetToken": "at", "title": "Shared plan", "perm": { "role": "viewer" } }),
        )
    });
    mock.on("GET", &source(PAGE, "v3", ""), |_| Reply::content("<h1>Hello</h1>", "text/html"));
    mock.on("GET", &source(PAGE, "v3", "css/site.css"), |_| Reply::content("body{}", "text/css"));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let url = viewer_url(PAGE);

    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url })).await;
    // Someone else's page: no model to summarize it here, so its content stays out.
    assert!(!err && !text.contains("<h1>Hello</h1>") && text.contains("cannot edit"), "{text}");
    let (err, text) = agent.call(&tools[0], json!({ "action": "read", "url": url, "path": "css/site.css" })).await;
    assert!(!err && text.contains("body{}"), "{text}");
    let (err, _) = agent.call(&tools[0], json!({ "action": "pin", "url": url })).await;
    assert!(!err);
    let (err, _) = agent.call(&tools[0], json!({ "action": "unpin", "url": url })).await;
    assert!(!err);
    let (err, text) = agent.call(&tools[0], json!({ "action": "open", "url": url })).await;
    assert!(!err && text.contains(&url), "{text}");
    let (err, text) = agent.call(&tools[0], json!({ "action": "delete", "url": url })).await;
    assert!(!err && text.contains("Deleted the artifact"), "{text}");
    assert_eq!(mock.find("DELETE", &format!("/api/frame/{PAGE}")).len(), 1);
}

#[tokio::test]
async fn quickstart_for_a_document_and_without_a_type() {
    let mock = Mock::start().await;
    mock.on("GET", "/api/frame/types?scopes=anthropic", |_| Reply::json(200, json!({ "types": [] })));
    let tools = tools_for(&mock, Tokens::new("tok", None));
    let agent = Agent::new();
    let (err, text) = agent.call(&tools[0], json!({ "action": "quickstart", "intent": "document" })).await;
    assert!(!err && text.contains("No first-party Claude Docs connector"), "{text}");
    let (err, text) = agent.call(&tools[0], json!({ "action": "quickstart", "intent": "slides" })).await;
    assert!(!err && text.contains("lists no Slides type"), "{text}");
}

#[test]
fn definitions_match_claude_code() {
    let mock_tokens = Tokens::new("tok", None);
    let tools = eos_agent_artifacts::tools(mock_tokens, eos_agent_artifacts::FrameConfig::default()).expect("tools");
    let names: Vec<&str> = tools.iter().map(|t| t.spec().name()).collect();
    assert_eq!(names, ["Artifact", "ArtifactComments", "ArtifactData"]);
    assert!(!tools[0].spec().is_deferred() && tools[1].spec().is_deferred() && tools[2].spec().is_deferred());
    let def: serde_json::Value = serde_json::from_slice(tools[0].spec().json()).expect("json");
    assert!(def["description"].as_str().is_some_and(|d| d.starts_with("The Artifact tool renders an HTML file")));
    assert_eq!(def["input_schema"]["properties"]["action"]["enum"].as_array().map(Vec::len), Some(8));
}
