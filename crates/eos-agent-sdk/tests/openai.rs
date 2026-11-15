//! Agents on OpenAI models through the SDK, against the mock API: a GPT
//! agent's tool turn, a Claude parent with a GPT subagent and back, a GPT
//! parent asking for a Claude model it has no credential for.

mod common;

use std::sync::Arc;

use common::{Dirs, all_text, settle};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential, StoredMessage};
use eos_agent_mock_api::{MockApi, Output, Recorded, Reply};
use eos_agent_sdk::{AgentOptions, ModelChange, Sdk};
use eos_agent_types::{ContentBlock, Role};
use serde_json::{Value, json};

fn catalog() -> Value {
    json!({"models": [
        {"slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "context_window": 272000,
         "default_reasoning_level": "medium", "supported_reasoning_levels": ["low", "medium", "high", "xhigh"]},
        {"slug": "gpt-6-luna", "display_name": "GPT-6 Luna", "visibility": "list"},
    ]})
}

async fn mock(replies: impl IntoIterator<Item = Reply>) -> MockApi {
    let mock = MockApi::start(replies).await.expect("mock");
    mock.set_models(catalog());
    mock
}

/// An SDK with a ChatGPT credential, and a Claude one when `claude`.
async fn sdk(mock: &MockApi, dirs: &Dirs, claude: bool) -> Sdk {
    let mut builder = Sdk::builder()
        .openai(Arc::new(StaticCredential(Credential::Bearer("plan-token".into()))))
        .openai_base_url(format!("{}/v1", mock.base_url()))
        .base_url(mock.base_url())
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"));
    if claude {
        builder = builder.auth(Arc::new(StaticCredential(Credential::ApiKey("k".into())))).model("claude-opus-5-5");
    }
    builder.build().await.expect("sdk")
}

/// Every string in a Responses request's input.
fn input_text(r: &Recorded) -> String {
    fn collect(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(items) => items.iter().for_each(|i| collect(i, out)),
            Value::Object(map) => map.values().for_each(|i| collect(i, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    collect(&r.json()["input"], &mut out);
    out
}

fn instructions(r: &Recorded) -> String {
    r.json()["instructions"].as_str().unwrap_or_default().to_owned()
}

fn function_names(r: &Recorded) -> Vec<String> {
    let body = r.json();
    let tools = body["tools"][0]["tools"].as_array().cloned().unwrap_or_default();
    tools.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect()
}

/// The content of every assistant message a Messages request carries.
fn replies(r: &Recorded) -> Vec<Value> {
    let body = r.json();
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    messages.into_iter().filter(|m| m["role"] == "assistant").map(|m| m["content"].clone()).collect()
}

#[tokio::test]
async fn a_conversation_moves_between_providers_and_each_gets_its_own_reasoning_back() {
    let dirs = Dirs::new("openai-switch");
    let mock = mock([
        Reply::thought("Greet.", "CLAUDE-SIG", "Hello from Claude."),
        Reply::responses(
            &[Output::reasoning("rs_1", "Carry on.", "ENC"), Output::text("Hello from GPT.")],
            (900, 0, 20),
        ),
        Reply::text("Claude again."),
        Reply::responses_text("GPT again."),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, true).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).id("root-1");
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("one").await.expect("alive").expect("turn");

    assert!(!agent.set_model_with("gpt", ModelChange::default()).await.expect("onto OpenAI"), "it fits: no summary");
    assert_eq!(agent.model().as_deref(), Some("gpt-6-sol"));
    agent.send_text("two").await.expect("alive").expect("turn");
    agent.set_model("claude-opus-5-5").await.expect("back onto Claude");
    agent.send_text("three").await.expect("alive").expect("turn");
    agent.set_model("gpt").await.expect("onto OpenAI again");
    agent.send_text("four").await.expect("alive").expect("turn");

    let sent = mock.requests();
    let paths: Vec<&str> = sent.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/v1/messages", "/v1/responses", "/v1/messages", "/v1/responses"]);

    // OpenAI reads what Claude said, without Claude's reasoning.
    let to_gpt = input_text(&sent[1]);
    assert!(to_gpt.contains("Hello from Claude.") && to_gpt.contains("one"), "{to_gpt}");
    assert!(!to_gpt.contains("CLAUDE-SIG") && !to_gpt.contains("Greet."), "{to_gpt}");
    assert!(instructions(&sent[1]).starts_with("You are Eos, a coding agent running on an OpenAI GPT model."));

    // Claude gets its own reply as it wrote it, and GPT's without GPT's reasoning.
    let to_claude = replies(&sent[2]);
    assert_eq!(to_claude.len(), 2);
    assert_eq!(to_claude[0][0]["type"], "thinking");
    assert_eq!(to_claude[0][0]["signature"], "CLAUDE-SIG");
    assert_eq!(to_claude[1].as_array().map(Vec::len), Some(1), "{}", to_claude[1]);
    assert_eq!(to_claude[1][0]["text"], "Hello from GPT.");
    assert!(!sent[2].body.windows(5).any(|w| w == b"oai1."), "no OpenAI reasoning reaches Claude");

    // Back on OpenAI, its reasoning of the second turn goes with the conversation again.
    let items = sent[3].json()["input"].as_array().cloned().unwrap_or_default();
    let reasoning: Vec<&Value> = items.iter().filter(|i| i["type"] == "reasoning").collect();
    assert_eq!(reasoning.len(), 1, "{items:?}");
    assert_eq!(reasoning[0]["id"], "rs_1");
    assert_eq!(reasoning[0]["encrypted_content"], "ENC");
    assert!(!input_text(&sent[3]).contains("CLAUDE-SIG"));
}

#[tokio::test]
async fn a_conversation_recorded_before_replies_named_their_provider_still_moves() {
    let dirs = Dirs::new("openai-switch-legacy");
    let mock = mock([Reply::text("Carrying on.")]).await;
    let sdk = sdk(&mock, &dirs, true).await;
    let said = [
        ContentBlock::Thinking { thinking: "Earlier.".into(), signature: "oai1.rs_9.ENC".into() },
        ContentBlock::Text { text: "From GPT, long ago.".into(), citations: None },
    ];
    let history = vec![StoredMessage::user_text("one"), StoredMessage::new(Role::Assistant, &said).expect("message")];
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).history(history)).await.expect("spawn");
    agent.send_text("two").await.expect("alive").expect("turn");

    let sent = mock.requests();
    assert_eq!(sent[0].path, "/v1/messages");
    let earlier = replies(&sent[0]);
    assert_eq!(earlier[0].as_array().map(Vec::len), Some(1), "{}", earlier[0]);
    assert_eq!(earlier[0][0]["text"], "From GPT, long ago.");
}

#[tokio::test]
async fn a_gpt_agent_runs_a_tool_turn() {
    let dirs = Dirs::new("openai-agent");
    dirs.write("project/notes.txt", "the answer is 42\n");
    let read = json!({"file_path": dirs.project().join("notes.txt")}).to_string();
    let mock = mock([
        Reply::responses(
            &[Output::reasoning("rs_1", "Read it.", "ENC"), Output::call("call_1", "Read", &read)],
            (900, 0, 20),
        ),
        Reply::responses_text("It says 42."),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, false).await;
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).model("gpt").mcp(false).id("root-1")).await.expect("spawn");
    agent.send_text("what do the notes say?").await.expect("alive").expect("turn");

    let sent = mock.requests();
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|r| r.path == "/v1/responses"));
    let first = sent[0].json();
    assert_eq!(first["model"], "gpt-6-sol", "`gpt` is the account's first listed model");
    assert_eq!(first["reasoning"]["effort"], "medium", "the catalog's default level");
    assert_eq!(first["prompt_cache_key"], "root-1");
    assert_eq!(first["store"], false);
    assert!(instructions(&sent[0]).starts_with("You are Eos, a coding agent running on an OpenAI GPT model."));
    assert!(instructions(&sent[0]).contains("There is no apply_patch"));
    let names = function_names(&sent[0]);
    for tool in ["Read", "Edit", "Write", "Bash", "Grep", "Glob", "Agent", "WebFetch", "TaskCreate"] {
        assert!(names.iter().any(|n| n == tool), "{tool} in {names:?}");
    }
    assert!(!names.iter().any(|n| n == "ToolSearch" || n == "WebSearch"), "no tool search, no Claude-only search");
    assert!(!sent[0].body.windows(13).any(|w| w == b"defer_loading"), "every tool in full");
    let context = input_text(&sent[0]);
    assert!(context.contains("what do the notes say?"));
    assert!(context.contains("GPT-6 Sol"), "the session context names the model");

    let second = sent[1].json();
    let items = second["input"].as_array().expect("input");
    let reasoning = items.iter().find(|i| i["type"] == "reasoning").expect("reasoning carried over");
    assert_eq!(reasoning["encrypted_content"], "ENC");
    let output = items.iter().find(|i| i["type"] == "function_call_output").expect("tool result");
    assert!(output["output"].as_str().is_some_and(|o| o.contains("the answer is 42")), "{output}");
}

#[tokio::test]
async fn a_claude_parent_hands_work_to_a_gpt_subagent() {
    let dirs = Dirs::new("openai-sub");
    dirs.write(
        "project/.claude/agents/gpt-helper.md",
        "---\nname: gpt-helper\ndescription: d\nmodel: gpt-6-luna\n---\nHelp out.",
    );
    let call = r#"{"description":"Ask GPT","prompt":"Say hi","subagent_type":"gpt-helper","run_in_background":false}"#;
    let mock =
        mock([Reply::tool_use("toolu_1", "Agent", call), Reply::responses_text("helper says hi"), Reply::text("done")])
            .await;
    let sdk = sdk(&mock, &dirs, true).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).id("root-2")).await.expect("spawn");
    agent.send_text("delegate").await.expect("alive").expect("turn");
    settle(&agent).await;

    let sent = mock.requests();
    let paths: Vec<&str> = sent.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(paths, ["/v1/messages", "/v1/responses", "/v1/messages"]);
    let child = sent[1].json();
    assert_eq!(child["model"], "gpt-6-luna");
    let key = child["prompt_cache_key"].as_str().expect("key").to_owned();
    assert!(key.starts_with('a') && key != "root-2", "a GPT subagent keys its own cache: {key}");
    assert_eq!(sent[1].headers["session-id"], key.as_str(), "and routes by it");
    let child_prompt = instructions(&sent[1]);
    assert!(child_prompt.starts_with("You are Eos, an agent running on an OpenAI GPT model"));
    assert!(child_prompt.contains("Help out.") && child_prompt.contains("# Tools"));
    assert!(child.get("metadata").is_none() && child.get("context_management").is_none());
    assert!(input_text(&sent[1]).contains("Say hi"));
    assert!(all_text(&sent[2]).contains("helper says hi"), "the report comes back to the Claude parent");
    assert!(sent[2].json().get("context_management").is_some(), "the parent keeps Claude's request shape");
}

#[tokio::test]
async fn a_gpt_parent_without_claude_keeps_its_own_model_for_subagents() {
    let dirs = Dirs::new("openai-fallback");
    let call = r#"{"description":"Look","prompt":"Look around","subagent_type":"general-purpose","model":"haiku","run_in_background":false}"#;
    let mock = mock([
        Reply::responses_call("call_a", "Agent", call),
        Reply::responses_text("looked"),
        Reply::responses_text("all done"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, false).await;
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).model("gpt-6-sol").mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    let sent = mock.requests();
    assert!(sent.iter().all(|r| r.path == "/v1/responses"), "no Claude credential, so no Claude request");
    assert_eq!(sent[1].json()["model"], "gpt-6-sol");
    assert!(
        sdk.spawn(AgentOptions::new(dirs.project()).model("haiku").mcp(false)).await.is_err(),
        "a Claude agent needs a Claude credential"
    );
}

/// Each GPT subagent keys its own prompt cache (and session header) by its
/// id, stable across its requests; the parent keeps its own.
#[tokio::test]
async fn gpt_subagents_key_their_own_cache() {
    let dirs = Dirs::new("openai-keys");
    dirs.write("project/notes.txt", "hello\n");
    let call = |d: &str| {
        json!({"description": d, "prompt": "Read notes.txt", "subagent_type": "general-purpose",
               "run_in_background": false})
        .to_string()
    };
    let read = json!({"file_path": dirs.project().join("notes.txt")}).to_string();
    let mock = mock([
        Reply::responses_call("call_a", "Agent", &call("One")),
        Reply::responses_call("call_r", "Read", &read),
        Reply::responses_text("one read it"),
        Reply::responses_call("call_b", "Agent", &call("Two")),
        Reply::responses_text("two done"),
        Reply::responses_text("all done"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs, false).await;
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).model("gpt").mcp(false).id("root-3")).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    let sent = mock.requests();
    let keys: Vec<String> =
        sent.iter().map(|r| r.json()["prompt_cache_key"].as_str().unwrap_or("").to_owned()).collect();
    for (r, key) in sent.iter().zip(&keys) {
        assert_eq!(r.headers["session-id"], key.as_str());
    }
    assert_eq!([&keys[0], &keys[3], &keys[5]], ["root-3"; 3], "the parent's key stays its own");
    assert_eq!(keys[1], keys[2], "one subagent, one key across its requests");
    assert!(keys[1] != "root-3" && keys[4] != "root-3" && keys[1] != keys[4], "{keys:?}");
}

/// A GPT agent (no ToolSearch) hears about failed MCP servers as Claude
/// Code tells a session without ToolSearch, with no deferred-tools list.
#[tokio::test]
async fn a_gpt_agent_hears_about_failed_mcp_servers() {
    let dirs = Dirs::new("openai-mcp-failed");
    dirs.write(
        "home/.claude.json",
        r#"{"mcpServers": {"broken": {"command": "/nonexistent/eos-mcp-test-server", "alwaysLoad": true}}}"#,
    );
    let mock = mock([Reply::responses_text("ok")]).await;
    let sdk = sdk(&mock, &dirs, false).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).model("gpt")).await.expect("spawn");
    agent.send_text("hi").await.expect("alive").expect("turn");
    let context = input_text(&mock.requests()[0]);
    assert!(
        context.contains(
            "The following MCP servers are configured but failed to connect \u{2014} their tools (typically named \
             mcp__<server>__*) are unavailable for this session:\nbroken: \""
        ),
        "{context}"
    );
    assert!(!context.contains("deferred tools"), "{context}");
}
