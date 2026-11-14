//! Resuming a conversation from its transcript keeps what the agent had read.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Dirs, mock};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_engine::JsonlStore;
use eos_agent_mock_api::{MockApi, Recorded, Reply};
use eos_agent_sdk::{AgentOptions, Sdk};
use serde_json::Value;

async fn sdk(mock: &MockApi, dirs: &Dirs, store: &JsonlStore) -> Sdk {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .store(Arc::new(store.clone()))
        .build()
        .await
        .expect("sdk")
}

/// The last tool result the model was sent: its text and whether it failed.
fn last_result(request: &Recorded) -> (String, bool) {
    let messages = request.json()["messages"].as_array().cloned().unwrap_or_default();
    let block = messages
        .iter()
        .rev()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default().into_iter().rev())
        .find(|b| b["type"] == "tool_result")
        .expect("a tool result");
    let text = match &block["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    (text, block["is_error"] == Value::Bool(true))
}

/// One agent reads `notes.txt`; `between` runs after its turn; a second
/// agent resumed from the transcript then edits the file. Returns the edit's
/// result and the transcript's lines.
async fn read_then_resume_and_edit(name: &str, between: impl FnOnce(&Dirs)) -> ((String, bool), Vec<Value>, String) {
    let dirs = Dirs::new(name);
    dirs.write("project/notes.txt", "codename: BLUEFIN\nowner: ada\n");
    let path = dirs.project().join("notes.txt");
    let read = format!(r#"{{"file_path":"{}"}}"#, path.display());
    let edit = format!(
        r#"{{"file_path":"{}","old_string":"codename: BLUEFIN","new_string":"codename: MARLIN"}}"#,
        path.display()
    );
    let mock = mock([
        Reply::tool_use("toolu_read", "Read", &read),
        Reply::text("read it"),
        Reply::tool_use("toolu_edit", "Edit", &edit),
        Reply::text("edited"),
    ])
    .await;
    let store = JsonlStore::new(dirs.root.join("transcripts"));

    let first = sdk(&mock, &dirs, &store).await;
    let (agent, _events) = first.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("read the notes").await.expect("alive").expect("turn");
    let id = agent.id().to_owned();
    drop(agent);
    drop(first);
    tokio::time::sleep(Duration::from_millis(30)).await;
    between(&dirs);

    // A new process: only the transcript remembers the read.
    let history = store.load(&id).await.expect("load");
    let lines: Vec<Value> = std::fs::read_to_string(store.path(&id))
        .expect("transcript")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    let second = sdk(&mock, &dirs, &store).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).id(id).history(history);
    let (resumed, _events) = second.spawn(options).await.expect("spawn");
    resumed.send_text("now rename the codename").await.expect("alive").expect("turn");
    let requests = mock.requests();
    (last_result(&requests[3]), lines, common::all_text(&requests[2]))
}

#[tokio::test]
async fn a_resumed_agent_may_edit_a_file_it_read_before() {
    let ((text, is_error), lines, sent) = read_then_resume_and_edit("resume-read", |_| {}).await;
    assert!(!is_error, "the edit went through: {text}");
    assert!(!sent.contains("changed on disk"), "an unchanged file is not reported");
    assert!(text.contains("has been updated successfully"), "{text}");

    // The transcript keeps each line's time and each response's usage.
    assert!(lines.iter().all(|l| l["timestamp"].as_str().is_some_and(|t| t.ends_with('Z'))));
    let assistant: Vec<&Value> = lines.iter().filter(|l| l["role"] == "assistant").collect();
    assert!(!assistant.is_empty() && assistant.iter().all(|l| l["usage"].is_object()), "{assistant:?}");
    assert!(lines.iter().filter(|l| l["role"] == "user").all(|l| l.get("usage").is_none()));
}

#[tokio::test]
async fn a_resumed_agent_must_read_again_a_file_that_changed() {
    let ((text, is_error), _, sent) = read_then_resume_and_edit("resume-changed", |dirs| {
        dirs.write("project/notes.txt", "codename: TUNA\nowner: ada\n");
    })
    .await;
    // The read is remembered as older than the file: the next request says
    // the file changed, and the edit built on the old text does not apply.
    assert!(sent.contains("notes.txt changed on disk since you last read it"), "{sent}");
    assert!(is_error, "the edit was refused: {text}");
    assert!(text.contains("String to replace not found"), "{text}");
}

#[tokio::test]
async fn resumed_requests_send_the_same_bytes() {
    let dirs = Dirs::new("resume-bytes");
    let mock = mock([Reply::text("one"), Reply::text("two"), Reply::text("two again")]).await;
    let store = JsonlStore::new(dirs.root.join("transcripts"));
    let first = sdk(&mock, &dirs, &store).await;
    let (agent, _events) = first.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    agent.send_text("second").await.expect("alive").expect("turn");
    let id = agent.id().to_owned();
    drop(agent);

    let history = store.load(&id).await.expect("load");
    let (resumed, _events) =
        first.spawn(AgentOptions::new(dirs.project()).mcp(false).id(id).history(history)).await.expect("spawn");
    resumed.send_text("second").await.expect("alive").expect("turn");
    let requests = mock.requests();
    let messages = |r: &Recorded| {
        let mut all = r.json()["messages"].as_array().cloned().unwrap_or_default();
        for m in &mut all {
            for b in m["content"].as_array_mut().into_iter().flatten() {
                b.as_object_mut().map(|o| o.remove("cache_control"));
            }
        }
        all
    };
    let (live, again) = (messages(&requests[1]), messages(&requests[2]));
    // The conversation as the second turn sent it, then the reply and the new
    // prompt: no usage or timestamp fields reach the model.
    assert_eq!(again.len(), live.len() + 2);
    assert_eq!(live[..], again[..live.len()]);
    assert!(again.iter().all(|m| m.get("usage").is_none() && m.get("timestamp").is_none()));
}
