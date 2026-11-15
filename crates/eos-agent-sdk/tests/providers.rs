//! Agents on a provider the host configured (a server that speaks the Chat
//! Completions API), against the mock API: a tool turn, a conversation that
//! moves there from Claude and back, the provider's model list.

mod common;

use std::sync::Arc;

use common::{Dirs, builder, mock};
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, ChatCompat, ProviderConfig, Sdk, SdkError};
use serde_json::{Value, json};

fn provider(mock: &MockApi) -> ProviderConfig {
    ProviderConfig {
        id: "deep".into(),
        base_url: mock.base_url(),
        chat_path: Some("/v1/chat/completions".into()),
        auth: Arc::new(StaticCredential(Credential::ApiKey("k-deep".into()))),
        key_header: None,
        headers: vec![("x-title".into(), "Eos".into())],
        compat: ChatCompat { replay_reasoning: true, ..ChatCompat::default() },
        context_window: Some(64_000),
        max_tokens: Some(4_000),
    }
}

async fn sdk(mock: &MockApi, dirs: &Dirs) -> Sdk {
    let sdk = builder(mock, dirs).model("claude-opus-5-5").build().await.expect("sdk");
    sdk.set_provider(provider(mock)).expect("the provider");
    sdk
}

#[tokio::test]
async fn an_agent_on_a_configured_provider_runs_a_tool_turn() {
    let dirs = Dirs::new("provider-agent");
    dirs.write("project/notes.txt", "the answer is 42\n");
    let read = json!({"file_path": dirs.project().join("notes.txt")}).to_string();
    let mock = mock([Reply::chat_call("call_1", "Read", &read), Reply::chat_text("", "It says 42.")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project()).model("deep/chat-1").mcp(false);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("what do the notes say?").await.expect("alive").expect("turn");

    let sent = mock.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.path == "/v1/chat/completions"));
    let header = |name: &str| sent[0].headers.get(name).and_then(|v| v.to_str().ok());
    assert_eq!(header("authorization"), Some("Bearer k-deep"));
    assert_eq!(header("x-title"), Some("Eos"));
    let first = sent[0].json();
    assert_eq!(first["model"], "chat-1", "the provider gets the model as it names it");
    assert_eq!(first["max_tokens"], 4_000);
    let system = first["messages"][0]["content"].as_str().unwrap_or_default();
    assert!(system.starts_with("You are Eos, a coding agent."), "{}", &system[..80.min(system.len())]);
    let names: Vec<&str> = first["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|t| t["function"]["name"].as_str()).collect())
        .unwrap_or_default();
    assert!(names.contains(&"Read") && names.contains(&"Bash") && !names.contains(&"ToolSearch"), "{names:?}");

    let second = sent[1].json();
    let messages = second["messages"].as_array().expect("messages");
    let call = messages.iter().find(|m| m["role"] == "assistant").expect("the call");
    assert_eq!(call["tool_calls"][0]["id"], "call_1");
    let result = messages.iter().find(|m| m["role"] == "tool").expect("the result");
    assert_eq!(result["tool_call_id"], "call_1");
    assert!(result["content"].as_str().is_some_and(|c| c.contains("the answer is 42")), "{result}");
    assert!(!messages.iter().skip(1).any(|m| m["role"] == "system"), "notes go in user messages");
}

#[tokio::test]
async fn a_conversation_moves_to_a_configured_provider_and_back() {
    let dirs = Dirs::new("provider-switch");
    let mock = mock([
        Reply::thought("Greet.", "CLAUDE-SIG", "Hello from Claude."),
        Reply::chat_text("Carry on.", "Hello from Deep."),
        Reply::text("Claude again."),
        Reply::chat_text("", "Deep again."),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("one").await.expect("alive").expect("turn");
    agent.set_model("deep/chat-1").await.expect("onto the provider");
    agent.send_text("two").await.expect("alive").expect("turn");
    agent.set_model("claude-opus-5-5").await.expect("back onto Claude");
    agent.send_text("three").await.expect("alive").expect("turn");
    agent.set_model("deep/chat-1").await.expect("onto the provider again");
    agent.send_text("four").await.expect("alive").expect("turn");

    let sent = mock.requests();
    let paths: Vec<&str> = sent.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/v1/messages", "/v1/chat/completions", "/v1/messages", "/v1/chat/completions"]);
    let to_deep = sent[1].json()["messages"].to_string();
    assert!(to_deep.contains("Hello from Claude.") && !to_deep.contains("CLAUDE-SIG") && !to_deep.contains("Greet."));
    let to_claude = sent[2].json()["messages"].to_string();
    assert!(to_claude.contains("CLAUDE-SIG") && to_claude.contains("Hello from Deep."), "{to_claude}");
    assert!(!to_claude.contains("Carry on."), "the provider's reasoning stays with it");
    let again = sent[3].json();
    let own: Vec<&Value> = again["messages"].as_array().expect("messages").iter().collect();
    let replayed = own.iter().find(|m| m["content"] == "Hello from Deep.").expect("its own reply");
    assert_eq!(replayed["reasoning_content"], "Carry on.", "its own reasoning goes back to it");
}

#[tokio::test]
async fn a_providers_models_are_listed_and_a_removed_provider_runs_nothing() {
    let dirs = Dirs::new("provider-models");
    let mock = mock([]).await;
    mock.set_models(json!({"object": "list", "data": [{"id": "chat-1"}, {"id": "reasoner-1"}]}));
    let sdk = sdk(&mock, &dirs).await;
    let models = sdk.provider_models("deep").await.expect("the provider").expect("listed");
    assert_eq!(models, ["chat-1", "reasoner-1"]);
    assert!(sdk.provider_models("nobody").await.is_none());
    assert_eq!(sdk.context_window("deep/chat-1"), 64_000);

    assert!(sdk.remove_provider("deep"));
    let refused = sdk.spawn(AgentOptions::new(dirs.project()).model("deep/chat-1").mcp(false)).await;
    assert!(matches!(refused, Err(SdkError::NoProvider { .. })), "{:?}", refused.map(|_| ()));
}
