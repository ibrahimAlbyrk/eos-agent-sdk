//! The Workflow tool through the SDK, against the mock API: a top-level
//! agent launches a script, its agents run as workflow subagents (one with a
//! structured answer), the run's events reach the host and its notification
//! wakes the agent.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::{Dirs, all_text, tool_names};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential, TaskEvent, TaskEventKind};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, Sdk};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const SCRIPT: &str = "export const meta = {name: 'demo-flow', description: 'Demo flow', phases: [{title: 'Find'}]}\n\
const found = await agent('Find the answer', {label: 'finder', phase: 'Find', schema: {type: 'object', properties: \
{answer: {type: 'string'}}, required: ['answer']}})\n\
const said = await agent(`Say ${found.answer}`)\n\
return {found, said}";

fn text_of(body: &Value) -> String {
    let mut out = String::new();
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => out.push_str(s),
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            Value::Object(m) => m.values().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&body["messages"], &mut out);
    out
}

fn system_of(body: &Value) -> String {
    body["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect()
}

fn tools_of(body: &Value) -> Vec<String> {
    body["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()
}

type Seen = Arc<std::sync::Mutex<Vec<Value>>>;

/// Answers by who is asking: the main agent launches the workflow, workflow
/// subagents answer (structured when they have StructuredOutput).
fn respond(seen: &Seen, body: &[u8]) -> Reply {
    let body: Value = serde_json::from_slice(body).unwrap_or_default();
    seen.lock().expect("lock").push(body.clone());
    let system = system_of(&body);
    let text = text_of(&body);
    let tools: Vec<&str> = body["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
    if system.contains("spawned by a workflow orchestration script") {
        if tools.contains(&"StructuredOutput") {
            return Reply::tool_use("toolu_so", "StructuredOutput", r#"{"answer":"42"}"#);
        }
        return Reply::text(if text.contains("Say 42") { "forty-two" } else { "?" });
    }
    if text.contains("<task-notification>") {
        return Reply::text("all done");
    }
    if text.contains("Workflow launched in background") {
        return Reply::text("started it");
    }
    Reply::tool_use("toolu_wf", "Workflow", &json!({ "script": SCRIPT }).to_string())
}

async fn sdk(mock: &MockApi, dirs: &Dirs, workflows: Option<bool>) -> Sdk {
    let mut builder = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"));
    if let Some(on) = workflows {
        builder = builder.workflows(on);
    }
    builder.build().await.expect("sdk")
}

fn is_subagent(r: &Value) -> bool {
    system_of(r).contains("spawned by a workflow orchestration script")
}

#[tokio::test]
async fn a_workflow_runs_through_the_sdk() {
    let dirs = Dirs::new("workflow-run");
    let recorded: Seen = Arc::default();
    let log = Arc::clone(&recorded);
    let mock =
        MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| respond(&log, b)).await.expect("mock");
    let requests = || recorded.lock().expect("lock").clone();
    let sdk = sdk(&mock, &dirs, None).await;
    let (tasks_tx, mut tasks) = mpsc::channel::<TaskEvent>(256);
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).task_events(tasks_tx)).await.expect("spawn");
    agent.send_text("use a workflow to find the answer").await.expect("alive").expect("turn");

    // settle() also waits for the run and the turn its notification wakes.
    common::settle(&agent).await;
    let mut seen = Vec::new();
    while let Ok(event) = tasks.try_recv() {
        seen.push(event);
    }
    assert!(matches!(seen.last().map(|e| &e.kind), Some(TaskEventKind::Ended { .. })), "the run ended");

    let requests = requests();
    let main = &requests[0];
    assert!(tools_of(main).iter().any(|t| t == "Workflow"), "top-level agents get Workflow");
    let description = main["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "Workflow").cloned())
        .expect("Workflow")["description"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(description.ends_with("The user can raise or remove it with \"Dynamic workflow size\" in /config."));
    assert!(description.contains("load the `workflow-authoring` skill"));

    let subagents: Vec<&Value> = requests.iter().filter(|r| is_subagent(r)).collect();
    assert_eq!(subagents.len(), 2, "two agents, the structured one ending its turn with the tool");
    for sub in &subagents {
        let tools = tools_of(sub);
        for denied in ["Workflow", "Agent", "AskUserQuestion", "SendUserMessage"] {
            assert!(!tools.iter().any(|t| t == denied), "{denied} offered to a workflow subagent");
        }
    }
    let finder = subagents.iter().find(|r| tools_of(r).iter().any(|t| t == "StructuredOutput")).expect("finder");
    assert!(system_of(finder).contains("You MUST call the StructuredOutput tool exactly once"));
    let schema = finder["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["name"] == "StructuredOutput").cloned())
        .expect("tool")["input_schema"]
        .clone();
    assert_eq!(schema["required"], json!(["answer"]));

    let note = requests.iter().map(text_of).find(|t| t.contains("<task-notification>")).expect("notification");
    assert!(note.contains("<status>completed</status>"), "{note}");
    assert!(note.contains(r#"<result>{"found":{"answer":"42"},"said":"forty-two"}</result>"#), "{note}");

    assert!(matches!(&seen[0].kind, TaskEventKind::Started { workflow_name: Some(n), .. } if n == "demo-flow"));
    assert_eq!(seen[0].agent_id, agent.id());
    let last_progress = seen
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            TaskEventKind::Progress { workflow_progress: Some(rows), .. } => Some(rows.clone()),
            _ => None,
        })
        .expect("progress rows");
    // Claude Code's key order, not sorted.
    assert!(last_progress.get().starts_with(r#"[{"type":"workflow_phase","index":1,"title":"Find"}"#));
    let rows: Value = serde_json::from_str(last_progress.get()).expect("json");
    let rows = rows.as_array().expect("rows");
    assert!(rows.iter().filter(|r| r["type"] == "workflow_agent").all(|r| r["state"] == "done"));

    // The run's files under the data dir, in Claude Code's layout.
    let session = project_folder(&dirs).join(agent.id());
    let records: Vec<_> = std::fs::read_dir(session.join("workflows")).expect("records").flatten().collect();
    assert!(records.iter().any(|e| e.file_name().to_string_lossy().ends_with(".json")));
    assert!(session.join("workflows/scripts").is_dir());
}

#[tokio::test]
async fn workflows_can_be_turned_off() {
    let dirs = Dirs::new("workflow-off");
    let mock = common::mock([Reply::text("hi")]).await;
    let sdk = sdk(&mock, &dirs, Some(false)).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");
    let tools = tool_names(&mock.requests()[0]);
    assert!(!tools.iter().any(|t| t == "Workflow"));
    assert!(!all_text(&mock.requests()[0]).contains("workflow-authoring"));
}

#[tokio::test]
async fn the_ultracode_keyword_loads_the_authoring_reference() {
    let dirs = Dirs::new("workflow-keyword");
    let mock = common::mock([Reply::text("ok")]).await;
    let sdk = sdk(&mock, &dirs, None).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("ultracode: audit this repo").await.expect("alive").expect("turn");
    let text = all_text(&mock.requests()[0]);
    assert!(text.contains("The user included the keyword \"ultracode\""), "{text}");
    assert!(text.contains("# Workflow authoring reference"));
}

/// The line `label` starts in the Workflow tool's launch text.
fn launch_line(text: &str, label: &str) -> Option<String> {
    let start = text.find(label)? + label.len();
    Some(text[start..].lines().next()?.trim().to_owned())
}

/// The tool results of the latest request, by tool_use id: (text, is_error).
fn results(body: &Value) -> Vec<(String, String, bool)> {
    let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
    last["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "tool_result")
        .map(|b| {
            let mut text = String::new();
            match &b["content"] {
                Value::String(s) => text.push_str(s),
                other => other.as_array().into_iter().flatten().for_each(|c| text += c["text"].as_str().unwrap_or("")),
            }
            (b["tool_use_id"].as_str().unwrap_or("").to_owned(), text, b["is_error"] == true)
        })
        .collect()
}

/// After the run: ListAgents, then Read and Edit the run's script, then Read
/// its journal, as a model following the launch text would.
fn respond_after_run(seen: &Seen, body: &[u8]) -> Reply {
    let parsed: Value = serde_json::from_slice(body).unwrap_or_default();
    let text = text_of(&parsed);
    if is_subagent(&parsed) || !text.contains("<task-notification>") {
        return respond(seen, body);
    }
    seen.lock().expect("lock").push(parsed);
    let script = launch_line(&text, "Script file: ").unwrap_or_default();
    let journal = format!("{}/journal.jsonl", launch_line(&text, "Transcript dir: ").unwrap_or_default());
    if !text.contains("toolu_la") {
        return Reply::tool_use("toolu_la", "ListAgents", "{}");
    }
    if !text.contains("toolu_rd") {
        return Reply::tool_use("toolu_rd", "Read", &json!({ "file_path": script }).to_string());
    }
    if !text.contains("toolu_ed") {
        let edit = json!({"file_path": script, "old_string": "Demo flow", "new_string": "Demo flow, edited"});
        return Reply::tool_use("toolu_ed", "Edit", &edit.to_string());
    }
    if !text.contains("toolu_jr") {
        return Reply::tool_use("toolu_jr", "Read", &json!({ "file_path": journal }).to_string());
    }
    if !text.contains("toolu_jw") {
        return Reply::tool_use("toolu_jw", "Write", &json!({ "file_path": journal, "content": "x" }).to_string());
    }
    Reply::text("all done")
}

/// Claude Code runs a workflow's agents outside its task list (ListAgents
/// never shows them) and lets the session read its run files and edit its
/// workflow scripts, which lie outside the working directories.
#[tokio::test]
async fn run_files_are_readable_and_agents_unlisted() {
    let dirs = Dirs::new("workflow-files");
    let recorded: Seen = Arc::default();
    let log = Arc::clone(&recorded);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |b| respond_after_run(&log, b))
        .await
        .expect("mock");
    let sdk = sdk(&mock, &dirs, None).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("use a workflow to find the answer").await.expect("alive").expect("turn");
    common::settle(&agent).await;

    let requests = recorded.lock().expect("lock").clone();
    let mut by_id = std::collections::HashMap::new();
    for request in &requests {
        for (id, text, error) in results(request) {
            by_id.insert(id, (text, error));
        }
    }
    let got = |id: &str| by_id.get(id).cloned().unwrap_or_else(|| panic!("no result for {id}"));
    assert_eq!(got("toolu_la"), ("No reachable agents.".to_owned(), false));
    let (read, error) = got("toolu_rd");
    assert!(!error && read.contains("export const meta"), "{read}");
    let (edit, error) = got("toolu_ed");
    assert!(!error, "{edit}");
    let session = project_folder(&dirs).join(agent.id());
    let scripts: Vec<_> = std::fs::read_dir(session.join("workflows/scripts")).expect("scripts").flatten().collect();
    let script = std::fs::read_to_string(scripts[0].path()).expect("script");
    assert!(script.contains("Demo flow, edited"), "{script}");
    let (journal, error) = got("toolu_jr");
    assert!(!error, "{journal}");
    let (write, error) = got("toolu_jw");
    assert!(error && write.contains("is in the agent engine's own files, which tools do not write."), "{write}");
}

/// Claude Code's folder for the project under the data dir.
fn project_folder(dirs: &Dirs) -> std::path::PathBuf {
    let cwd = std::fs::canonicalize(dirs.project()).expect("project");
    dirs.root.join("data").join(eos_agent_compat::project_dir_name(&cwd))
}

/// Every session of the project may be read (a resumed conversation's
/// earlier sessions); so may another project's, as any file outside the
/// working directories (the permission gate decides).
#[tokio::test]
async fn every_session_of_the_project_is_readable() {
    let dirs = Dirs::new("workflow-project-dir");
    let earlier = project_folder(&dirs).join("earlier-session/workflows/r1.json");
    std::fs::create_dir_all(earlier.parent().expect("dir")).expect("dir");
    std::fs::write(&earlier, "{\"status\":\"completed\"}").expect("write");
    let other = dirs.root.join("data/-elsewhere/s/workflows/r2.json");
    std::fs::create_dir_all(other.parent().expect("dir")).expect("dir");
    std::fs::write(&other, "{\"status\":\"elsewhere\"}").expect("write");
    let mock = MockApi::start([
        Reply::tool_use("r1", "Read", &json!({ "file_path": earlier }).to_string()),
        Reply::tool_use("r2", "Read", &json!({ "file_path": other }).to_string()),
        Reply::text("done"),
    ])
    .await
    .expect("mock");
    let sdk = sdk(&mock, &dirs, None).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("read them").await.expect("alive").expect("turn");
    let last = mock.requests().last().expect("requests").json().to_string();
    assert!(last.contains("completed"), "an earlier session of the project: {last}");
    assert!(last.contains("elsewhere\\\""), "another project's: {last}");
}
