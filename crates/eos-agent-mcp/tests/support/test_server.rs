//! A small stdio MCP server for the integration tests: newline-delimited
//! JSON-RPC, with tools that exercise results, errors, sizes, cancellation,
//! list changes and child processes.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Default)]
struct State {
    extra_tool: bool,
    cancelled: HashSet<String>,
    cancelled_seen: usize,
}

type Out = Arc<Mutex<std::io::Stdout>>;
type Shared = Arc<Mutex<State>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn send(out: &Out, message: &Value) {
    let mut out = lock(out);
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}

fn text(t: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": t.into()}]})
}

fn failure(t: impl Into<String>) -> Value {
    json!({"isError": true, "content": [{"type": "text", "text": t.into()}]})
}

fn tool(name: &str, description: &str) -> Value {
    json!({"name": name, "description": description, "inputSchema": {"type": "object"}})
}

fn tools(state: &State) -> Value {
    let mut echo = tool("echo", "Echo the text back.");
    echo["annotations"] = json!({"readOnlyHint": true});
    echo["_meta"] = json!({"anthropic/searchHint": "say it back"});
    let mut big_ok = tool("big_ok", "Like big, allowed more.");
    big_ok["_meta"] = json!({"anthropic/maxResultSizeChars": 8000});
    let mut pid = tool("pid", "This server's pid.");
    pid["_meta"] = json!({"anthropic/alwaysLoad": true});
    let mut list = vec![
        echo,
        tool("fail", "Always fails."),
        tool("big", "Returns `chars` x's."),
        big_ok,
        pid,
        tool("spawn_child", "Starts a long sleep; returns its pid."),
        tool("slow", "Waits `ms`."),
        tool("cancelled", "How many calls were cancelled."),
        tool("add_tool", "Adds the `extra` tool."),
        tool("image", "Returns a picture."),
        tool("get.thing", "A name to normalize."),
    ];
    if state.extra_tool {
        list.push(tool("extra", "Added later."));
    }
    json!({"tools": list})
}

/// `None` when the reply is sent later (by `slow`).
fn call_tool(params: &Value, id: &Value, state: &Shared, out: &Out) -> Option<Value> {
    let args = &params["arguments"];
    let count = || usize::try_from(args["chars"].as_u64().unwrap_or(0)).unwrap_or(0);
    let result = match params["name"].as_str().unwrap_or_default() {
        "echo" => text(args["text"].as_str().unwrap_or_default()),
        "fail" => failure("boom"),
        "big" | "big_ok" => text("x".repeat(count())),
        "pid" => text(std::process::id().to_string()),
        "spawn_child" => match std::process::Command::new("sleep").arg("600").spawn() {
            Ok(child) => text(child.id().to_string()),
            Err(e) => failure(e.to_string()),
        },
        "slow" => {
            let (id, state, out) = (id.clone(), Arc::clone(state), Arc::clone(out));
            let ms = args["ms"].as_u64().unwrap_or(0);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                let mut s = lock(&state);
                if s.cancelled.remove(&id.to_string()) {
                    s.cancelled_seen += 1;
                } else {
                    drop(s);
                    send(&out, &json!({"jsonrpc": "2.0", "id": id, "result": text("done")}));
                }
            });
            return None;
        }
        "cancelled" => text(lock(state).cancelled_seen.to_string()),
        "add_tool" => {
            lock(state).extra_tool = true;
            send(out, &json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
            text("added")
        }
        "image" => json!({"content": [{"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}]}),
        "get.thing" | "extra" => text("ok"),
        other => return Some(json!({"error": {"code": -32602, "message": format!("Unknown tool: {other}")}})),
    };
    Some(json!({"result": result}))
}

fn read_resource(params: &Value) -> Value {
    let contents = match params["uri"].as_str().unwrap_or_default() {
        "test://hello" => json!([{"uri": "test://hello", "mimeType": "text/plain", "text": "hello world"}]),
        "test://blob" => json!([{"uri": "test://blob", "mimeType": "application/octet-stream", "blob": "AAEC"}]),
        uri => return json!({"error": {"code": -32002, "message": format!("Resource not found: {uri}")}}),
    };
    json!({"result": {"contents": contents}})
}

fn main() {
    let out: Out = Arc::new(Mutex::new(std::io::stdout()));
    let state: Shared = Arc::default();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        let method = message["method"].as_str().unwrap_or_default();
        let params = &message["params"];
        let Some(id) = message.get("id").cloned() else {
            if method == "notifications/cancelled" {
                lock(&state).cancelled.insert(params["requestId"].to_string());
            }
            continue;
        };
        let reply = match method {
            "initialize" => Some(json!({"result": {
                "protocolVersion": params["protocolVersion"],
                "capabilities": {"tools": {"listChanged": true}, "resources": {}},
                "serverInfo": {"name": "eos-test", "version": "1"},
                "instructions": "Use echo to repeat things.",
            }})),
            "ping" => Some(json!({"result": {}})),
            "tools/list" => Some(json!({"result": tools(&lock(&state))})),
            "tools/call" => call_tool(params, &id, &state, &out),
            "resources/list" => Some(json!({"result": {"resources": [
                {"uri": "test://hello", "name": "hello", "mimeType": "text/plain"},
                {"uri": "test://blob", "name": "blob"},
            ]}})),
            "resources/read" => Some(read_resource(params)),
            _ => Some(json!({"error": {"code": -32601, "message": format!("Method not found: {method}")}})),
        };
        if let Some(Value::Object(mut reply)) = reply {
            reply.insert("jsonrpc".into(), "2.0".into());
            reply.insert("id".into(), id);
            send(&out, &Value::Object(reply));
        }
    }
}
