//! The legacy HTTP+SSE transport against a small server of that kind: the
//! GET stream names the message endpoint, POSTed requests are answered on
//! the stream, the config's headers ride every request.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use eos_agent_core::{CancellationToken, Extensions, ToolContext};
use eos_agent_mcp::{McpHub, McpLimits, McpServerConfig, McpTransport};
use eos_agent_types::ToolResultContent;
use futures_util::{Stream, StreamExt};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct Server {
    stream: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    keys: Arc<Mutex<Vec<String>>>,
}

async fn open(State(s): State<Server>, headers: HeaderMap) -> Response {
    if headers.get("x-key").and_then(|v| v.to_str().ok()) != Some("k1") {
        return StatusCode::FORBIDDEN.into_response();
    }
    let (tx, rx) = mpsc::channel::<String>(16);
    *s.stream.lock().expect("lock") = Some(tx);
    let first = futures_util::stream::once(async {
        Ok::<_, Infallible>(Event::default().event("endpoint").data("/messages?sessionId=1"))
    });
    let rest = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|m| (Ok::<_, Infallible>(Event::default().event("message").data(m)), rx))
    });
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> = Box::pin(first.chain(rest));
    Sse::new(events).into_response()
}

async fn message(State(s): State<Server>, headers: HeaderMap, Json(msg): Json<Value>) -> StatusCode {
    s.keys.lock().expect("lock").extend(headers.get("x-key").and_then(|v| v.to_str().ok()).map(str::to_owned));
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => json!({
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "legacy", "version": "1"},
        }),
        "tools/list" => json!({"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}),
        "tools/call" => {
            json!({"content": [{"type": "text", "text": format!("echo:{}", msg["params"]["arguments"]["text"].as_str().unwrap_or("-"))}]})
        }
        _ => json!({}),
    };
    let reply = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    let tx = s.stream.lock().expect("lock").clone();
    if let Some(tx) = tx {
        let _ = tx.send(reply).await;
    }
    StatusCode::ACCEPTED
}

#[tokio::test]
async fn a_legacy_sse_server_connects_and_answers() {
    let server = Server::default();
    let app = Router::new().route("/sse", get(open)).route("/messages", post(message)).with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });

    let hub = McpHub::new(McpLimits { connect_timeout: Duration::from_secs(10), ..McpLimits::default() });
    let mut config = McpServerConfig::http("legacy", &format!("{base}/sse"));
    config.transport = McpTransport::Sse {
        url: format!("{base}/sse"),
        headers: BTreeMap::from([("x-key".to_owned(), "k1".to_owned())]),
        oauth: None,
    };
    let set = hub.connected_toolset(&[config]).await;
    let tool = set.tools().into_iter().find(|t| t.spec().name() == "mcp__legacy__echo").expect("echo listed");
    let input = RawValue::from_string(r#"{"text":"hi"}"#.to_owned()).expect("json");
    let (cancel, extensions) = (CancellationToken::new(), Extensions::default());
    let ctx =
        ToolContext { agent_id: "a", tool_use_id: "t", cancel: &cancel, extensions: &extensions, transcript: &[] };
    let out = tool.call(&input, ctx).await;
    let text = match &out.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(b) => format!("{b:?}"),
    };
    assert!(text.contains("echo:hi"), "{text}");
    assert!(server.keys.lock().expect("lock").iter().all(|k| k == "k1"), "the config's headers ride every POST");
}
