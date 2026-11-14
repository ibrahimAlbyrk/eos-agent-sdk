//! A Streamable HTTP MCP server for tests: it answers `initialize` only once
//! the test lets it, so a test decides when the server is done connecting.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::watch;

#[derive(Clone)]
struct Shared {
    open: watch::Receiver<bool>,
    tools: Arc<Mutex<Vec<Value>>>,
    instructions: Option<String>,
}

pub struct McpMock {
    pub url: String,
    open: watch::Sender<bool>,
    tools: Arc<Mutex<Vec<Value>>>,
}

impl McpMock {
    /// `tools`: `(name, meta)`; `meta` goes in the tool's `_meta`.
    pub async fn start(tools: &[(&str, Option<Value>)], instructions: Option<&str>) -> Self {
        let (open, rx) = watch::channel(false);
        let tools: Vec<Value> = tools.iter().map(|(name, meta)| tool(name, meta.clone())).collect();
        let tools = Arc::new(Mutex::new(tools));
        let shared = Shared { open: rx, tools: Arc::clone(&tools), instructions: instructions.map(str::to_owned) };
        let app = Router::new()
            .route(
                "/mcp",
                post(mcp).get(|| async { StatusCode::METHOD_NOT_ALLOWED }).delete(|| async { StatusCode::OK }),
            )
            .with_state(shared);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}/mcp", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, open, tools }
    }

    /// Lets `initialize` answer: the server connects.
    pub fn open(&self) {
        let _ = self.open.send(true);
    }

    /// Replaces the tool list with these definitions.
    pub fn set_raw_tools(&self, tools: Vec<Value>) {
        *self.tools.lock().expect("lock") = tools;
    }

    /// Replaces the tool list (seen on the next `tools/list`).
    pub fn set_tools(&self, tools: &[(&str, Option<Value>)]) {
        *self.tools.lock().expect("lock") = tools.iter().map(|(name, meta)| tool(name, meta.clone())).collect();
    }
}

/// A tool definition as the server lists it.
pub fn tool(name: &str, meta: Option<Value>) -> Value {
    let mut tool = json!({
        "name": name,
        "description": format!("The {name} tool."),
        "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}},
    });
    if let Some(meta) = meta {
        tool["_meta"] = meta;
    }
    tool
}

async fn mcp(State(s): State<Shared>, Json(msg): Json<Value>) -> Response {
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => {
            let mut open = s.open.clone();
            let _ = open.wait_for(|o| *o).await;
            let mut result = json!({
                "protocolVersion": msg["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "1"},
            });
            if let Some(text) = &s.instructions {
                result["instructions"] = json!(text);
            }
            result
        }
        "tools/list" => json!({"tools": s.tools.lock().expect("lock").clone()}),
        "tools/call" => json!({"content": [{"type": "text", "text": format!("called {}", msg["params"]["name"])}]}),
        other => {
            return Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": other}}))
                .into_response();
        }
    };
    ([("mcp-session-id", "s1")], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response()
}
