//! A chat Claude Code recorded carries on as an agent here.

mod common;

use common::{Dirs, mock, sdk};
use eos_agent_compat::{ClaudePaths, ClaudeSession, transcript_path};
use eos_agent_mock_api::Reply;
use eos_agent_sdk::AgentOptions;
use serde_json::json;

#[tokio::test]
async fn a_claude_code_session_continues_from_its_transcript() {
    let dirs = Dirs::new("import");
    let entry = |kind: &str, uuid: &str, parent: Option<&str>, message: serde_json::Value| {
        json!({"type": kind, "uuid": uuid, "parentUuid": parent, "isSidechain": false, "sessionId": "sess-1",
               "cwd": dirs.project(), "message": message})
    };
    let assistant = |block: serde_json::Value| json!({"id": "msg_1", "role": "assistant", "model": "claude-opus-5-5", "content": [block]});
    let lines = [
        entry("user", "u1", None, json!({"role": "user", "content": "what is in notes.txt?"})),
        entry("assistant", "a1", Some("u1"), assistant(json!({"type": "text", "text": "Reading it."}))),
        entry(
            "assistant",
            "a2",
            Some("a1"),
            assistant(json!({"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "notes.txt"},
                             "caller": {"type": "direct"}})),
        ),
        entry(
            "user",
            "u2",
            Some("a2"),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "codename: MARLIN"}]}),
        ),
    ];
    let path = transcript_path(&ClaudePaths::for_home(&dirs.home()), &dirs.project(), "sess-1");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).expect("write");

    let session = ClaudeSession::read(&path).expect("read");
    assert_eq!(session.model.as_deref(), Some("claude-opus-5-5"));
    let mock = mock([Reply::text("It holds the codename MARLIN."), Reply::text("yes")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).id("sess-1").history(session.messages);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");

    // The session was cut off after the tool's result: the turn is carried on as it stands.
    agent.resume().await.expect("alive").expect("turn").expect("a turn to carry on");
    agent.send_text("are you sure?").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let first = requests[0].json();
    let messages = first["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["content"][0]["text"], "what is in notes.txt?");
    assert_eq!(
        messages[1]["content"],
        json!([
            {"type": "text", "text": "Reading it."},
            {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"file_path": "notes.txt"}},
        ])
    );
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(requests[1].json()["messages"].as_array().expect("messages").len(), 5);
}
