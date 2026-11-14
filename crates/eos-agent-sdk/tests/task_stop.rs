//! TaskStop on the model's side for background subagents: each handle the
//! tools hand the model for one (the agentId of the Agent result, which is
//! also the `<task-id>` of its notifications; the name it was spawned with;
//! the `[ref]` ListAgents prints, bare or as `name [ref]`) stops it, as
//! TaskStop's own description promises for named background agents.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Dirs, builder};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::AgentOptions;
use serde_json::Value;

type Seen = Arc<Mutex<Vec<Value>>>;

const SCOUTS: [&str; 4] = ["scout-id", "scout-name", "scout-ref", "scout-named-ref"];

fn texts(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|v| texts(v, out)),
        Value::Object(m) => m.values().for_each(|v| texts(v, out)),
        _ => {}
    }
}

fn blocks(body: &Value) -> impl Iterator<Item = &Value> {
    body["messages"].as_array().into_iter().flatten().filter_map(|m| m["content"].as_array()).flatten()
}

fn tool_results(body: &Value) -> usize {
    blocks(body).filter(|b| b["type"] == "tool_result").count()
}

/// The ids in the Agent results, in spawn order.
fn agent_ids(text: &str) -> Vec<String> {
    let ids = text.split("agentId: ").skip(1).map(|s| s.chars().take_while(char::is_ascii_alphanumeric).collect());
    let mut ids: Vec<String> = ids.collect();
    ids.dedup();
    ids
}

/// The `[ref]` ListAgents printed for `name`.
fn listed_ref(text: &str, name: &str) -> String {
    let at = text.rfind(&format!("{name} [")).expect("listed") + name.len() + 2;
    text[at..].chars().take_while(|c| *c != ']').collect()
}

fn stop(n: usize, task_id: &str) -> Reply {
    let input = serde_json::json!({ "task_id": task_id }).to_string();
    Reply::tool_use(&format!("toolu_stop{n}"), "TaskStop", &input)
}

/// The parent: four named background scouts, TaskStop loaded, the listing,
/// then one TaskStop per handle. A scout's turn takes long enough to be
/// running throughout.
fn respond(body: &Value) -> Reply {
    let mut text = String::new();
    texts(&body["messages"], &mut text);
    if !text.contains("PARENT-START") {
        return Reply::text("scouting").slowed(Duration::from_secs(30));
    }
    let step = tool_results(body);
    if let Some(name) = SCOUTS.get(step) {
        let input = serde_json::json!({
            "description": format!("Scout {name}"),
            "prompt": "CHILD-START look around",
            "subagent_type": "general-purpose",
            "name": name,
            "run_in_background": true,
        });
        return Reply::tool_use(&format!("toolu_agent{step}"), "Agent", &input.to_string());
    }
    match step {
        4 => Reply::tool_use("toolu_search", "ToolSearch", r#"{"query":"select:TaskStop","max_results":5}"#),
        5 => Reply::tool_use("toolu_list", "ListAgents", "{}"),
        6 => stop(6, &agent_ids(&text)[0]),
        7 => stop(7, "scout-name"),
        8 => stop(8, &listed_ref(&text, "scout-ref")),
        9 => stop(9, &format!("scout-named-ref [{}]", listed_ref(&text, "scout-named-ref"))),
        _ => Reply::text("stopped them all"),
    }
}

/// What TaskStop answered to the call `toolu_stop{n}`.
fn stop_result(body: &Value, n: usize) -> String {
    let id = format!("toolu_stop{n}");
    let block = blocks(body).find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id.as_str());
    let mut out = String::new();
    texts(&block.expect("answered")["content"], &mut out);
    out
}

#[tokio::test]
async fn task_stop_stops_a_background_subagent_by_every_handle_the_model_was_given() {
    let dirs = Dirs::new("taskstop-subagent");
    let seen: Seen = Arc::default();
    let log = Arc::clone(&seen);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| {
        let body: Value = serde_json::from_slice(b).unwrap_or_default();
        let reply = respond(&body);
        log.lock().expect("lock").push(body);
        reply
    })
    .await
    .expect("mock");
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    tokio::time::timeout(Duration::from_secs(20), agent.send_text("PARENT-START"))
        .await
        .expect("the parent's turn ends")
        .expect("alive")
        .expect("turn");

    let seen = seen.lock().expect("lock").clone();
    let last = seen
        .iter()
        .filter(|b| tool_results(b) >= 10 && b.to_string().contains("PARENT-START"))
        .last()
        .expect("the parent made every call");
    let handles = ["its agentId (= task-id)", "its name", "its ListAgents ref", "`name [ref]`"];
    let failed: Vec<String> = handles
        .iter()
        .zip(6..10)
        .map(|(h, n)| (h, stop_result(last, n)))
        .filter(|(_, a)| !a.contains("Successfully stopped task"))
        .map(|(h, a)| format!("by {h}: {}", a.trim()))
        .collect();
    assert!(failed.is_empty(), "TaskStop did not stop a running background subagent:\n{}", failed.join("\n"));
    assert_eq!(agent.running_subagents(), 0, "every scout was stopped");
}
