mod common;

use std::sync::Arc;

use common::{Agent, FakeModel, reply, text_of};
use eos_agent_core::Thinking;
use eos_agent_tools_web::{SearchBudget, SessionSearchBudget, WebSearchConfig, WebSearchTool};
use serde_json::json;

fn search_reply() -> Vec<eos_agent_types::StreamEvent> {
    reply(&[
        (r#"{"type":"text","text":""}"#, Some(r#"{"type":"text_delta","text":"Searching."}"#)),
        (
            r#"{"type":"server_tool_use","id":"srv1","name":"web_search","input":{}}"#,
            Some(r#"{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}"#),
        ),
        (
            r#"{"type":"web_search_tool_result","tool_use_id":"srv1","content":[{"type":"web_search_result","title":"Rust","url":"https://rust-lang.org","encrypted_content":"x"}]}"#,
            None,
        ),
        (r#"{"type":"text","text":""}"#, Some(r#"{"type":"text_delta","text":"Rust is a language."}"#)),
    ])
}

#[tokio::test]
async fn forces_the_server_tool_and_formats_results() {
    let model = Arc::new(FakeModel::new([search_reply()]));
    let tool = WebSearchTool::new(model.clone(), WebSearchConfig::new("claude-opus-5-5"));
    let agent = Agent::new("a1");
    let out = agent.call(&tool, json!({"query": "rust lang", "allowed_domains": ["rust-lang.org"]})).await;
    assert!(!out.is_error);
    assert_eq!(
        text_of(&out),
        "Web search results for query: \"rust lang\"\n\nSearching.\n\nLinks: \
[{\"title\":\"Rust\",\"url\":\"https://rust-lang.org\"}]\n\nRust is a language.\n\n\nREMINDER: You MUST include the \
sources above in your response to the user using markdown hyperlinks."
    );

    let req = model.request(0);
    assert_eq!(req.model, "claude-opus-5-5");
    assert_eq!(req.thinking, Some(Thinking::Disabled));
    let tools: serde_json::Value = serde_json::from_slice(req.tools.as_ref().expect("tools")).expect("json");
    assert_eq!(
        tools,
        json!([{"type":"web_search_20250305","name":"web_search","allowed_domains":["rust-lang.org"],"max_uses":8}])
    );
    let extra: serde_json::Value = serde_json::from_slice(req.extra.as_ref().expect("extra")).expect("json");
    assert_eq!(extra, json!({"tool_choice": {"type": "tool", "name": "web_search"}}));
    let system: serde_json::Value = serde_json::from_slice(&req.system).expect("json");
    assert!(system[1]["text"].as_str().expect("text").starts_with("You run a web search for another Claude model"));
    assert_eq!(common::prompt_of(&req), "Perform a web search for the query: rust lang");
}

#[tokio::test]
async fn a_model_that_always_thinks_is_asked_to_search_not_forced() {
    let model = Arc::new(FakeModel::new([search_reply()]));
    let mut config = WebSearchConfig::new("claude-opus-5-5");
    config.side.always_thinks = true;
    let tool = WebSearchTool::new(model.clone(), config);
    let out = Agent::new("a1").call(&tool, json!({"query": "rust lang"})).await;
    assert!(!out.is_error);

    let req = model.request(0);
    assert_eq!(req.thinking, None, "no thinking setting: the model refuses `disabled`");
    let extra: serde_json::Value = serde_json::from_slice(req.extra.as_ref().expect("extra")).expect("json");
    assert_eq!(extra, json!({"tool_choice": {"type": "auto"}}));
}

#[tokio::test]
async fn session_budget_is_shared_and_never_errors() {
    let model = Arc::new(FakeModel::new([search_reply()]));
    let tool = WebSearchTool::new(model.clone(), WebSearchConfig::new("m"));
    let shared = Arc::new(SearchBudget::new(1));
    let (a, b) = (Agent::new("a"), Agent::new("b"));
    a.extensions.insert(SessionSearchBudget(Arc::clone(&shared)));
    b.extensions.insert(SessionSearchBudget(shared));
    assert!(!a.call(&tool, json!({"query": "one"})).await.is_error);
    let out = b.call(&tool, json!({"query": "two"})).await;
    assert!(!out.is_error);
    assert!(text_of(&out).contains("Web search was not performed: this session's web search budget is used up"));
    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn rejects_both_domain_filters() {
    let tool = WebSearchTool::new(Arc::new(FakeModel::default()), WebSearchConfig::new("m"));
    let out = Agent::new("a")
        .call(&tool, json!({"query": "x y", "allowed_domains": ["a.com"], "blocked_domains": ["b.com"]}))
        .await;
    assert!(out.is_error);
    assert!(text_of(&out).contains("Cannot specify both allowed_domains and blocked_domains"));
}

#[test]
fn spec_matches_claude_code() {
    let tool = WebSearchTool::new(Arc::new(FakeModel::default()), WebSearchConfig::new("m"));
    let spec: serde_json::Value = serde_json::from_slice(eos_agent_core::Tool::spec(&tool).json()).expect("json");
    assert_eq!(spec["name"], "WebSearch");
    assert_eq!(spec["defer_loading"], true);
    assert_eq!(spec["input_schema"]["required"], json!(["query"]));
    assert!(
        spec["description"]
            .as_str()
            .expect("str")
            .contains("The current month is (provided in the conversation below)")
    );
}
