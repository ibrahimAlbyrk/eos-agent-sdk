//! What a host can choose per agent when it spawns it.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::mcp::McpMock;
use common::{Dirs, all_text, mock, sdk, sdk_with_model, system_text, tool_names};
use eos_agent_core::{AgentEvent, CompactionText, ModelError, TurnError, TurnOutcome};
use eos_agent_mock_api::Reply;
use eos_agent_sdk::{AgentOptions, McpServerConfig, McpServerState, SdkAgent, SystemPromptOverride};
use eos_agent_types::StopReason;

async fn until_connected(agent: &SdkAgent, server: &str) {
    for _ in 0..200 {
        if agent.mcp_status().iter().any(|s| s.name == server && s.state != McpServerState::Pending) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{server} did not connect");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_host_names_an_agents_mcp_servers() {
    let dirs = Dirs::new("opt-mcp");
    let configured = McpMock::start(&[("lookup", None)], None).await;
    let own = McpMock::start(&[("present", None)], None).await;
    configured.open();
    own.open();
    dirs.write(
        "home/.claude.json",
        &format!(r#"{{"mcpServers": {{"docs": {{"type": "http", "url": "{}"}}}}}}"#, configured.url),
    );
    let mock = mock([Reply::text("one"), Reply::text("two")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let host = || vec![McpServerConfig::http("eos", &own.url)];

    // Only the host's servers.
    let strict = AgentOptions::new(dirs.project()).mcp(false).mcp_servers(host());
    let (agent, _events) = sdk.spawn(strict).await.expect("spawn");
    until_connected(&agent, "eos").await;
    let names: Vec<String> = agent.mcp_status().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["eos"]);
    agent.send_text("hello").await.expect("alive").expect("turn");
    assert!(all_text(&mock.requests()[0]).contains("mcp__eos__present"));
    assert!(!all_text(&mock.requests()[0]).contains("mcp__docs__lookup"));

    // The host's servers beside the configured ones.
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp_servers(host())).await.expect("spawn");
    until_connected(&agent, "eos").await;
    until_connected(&agent, "docs").await;
    agent.send_text("hello").await.expect("alive").expect("turn");
    let text = all_text(&mock.requests()[1]);
    assert!(text.contains("mcp__eos__present") && text.contains("mcp__docs__lookup"), "{text}");
}

#[tokio::test]
async fn tool_search_can_be_turned_off_for_an_agent_and_its_subagents() {
    let dirs = Dirs::new("opt-search");
    let call = r#"{"description":"Helper","prompt":"Help","run_in_background":false}"#;
    let mock = mock([Reply::tool_use("toolu_a", "Agent", call), Reply::text("helped"), Reply::text("done")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).tool_search(false)).await.expect("spawn");
    agent.send_text("delegate").await.expect("alive").expect("turn");

    for request in &mock.requests()[..2] {
        let names = tool_names(request);
        assert!(!names.iter().any(|n| n == "ToolSearch" || n == "DeferredToolPlaceholder"), "{names:?}");
        assert!(names.iter().any(|n| n == "WebFetch") && names.iter().any(|n| n == "NotebookEdit"), "{names:?}");
        let deferred =
            request.json()["tools"].as_array().expect("tools").iter().any(|t| t.get("defer_loading").is_some());
        assert!(!deferred, "every tool is sent in full");
        assert!(!all_text(request).contains("deferred tools"));
    }
}

/// The host's own words for summarizing.
#[derive(Debug)]
struct Brief;

impl CompactionText for Brief {
    fn instruction(&self, focus: Option<&str>) -> String {
        format!("HOST: summarize briefly. {}", focus.unwrap_or_default())
    }

    fn seed(&self, summary: &str) -> String {
        format!("HOST SUMMARY: {summary}")
    }
}

/// A message this long is estimated at 60,000 tokens.
fn long_message() -> String {
    "word ".repeat(48_000)
}

#[tokio::test]
async fn the_host_sets_where_and_in_which_words_a_conversation_is_summarized() {
    let dirs = Dirs::new("opt-compact");
    let mock = mock([Reply::text("one"), Reply::text("the summary"), Reply::text("two")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).compact_at(50_000).compaction_text(Arc::new(Brief));
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    agent.send_text(long_message()).await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "a turn, the summary, the second turn");
    assert!(all_text(&requests[1]).contains("HOST: summarize briefly."));
    let after = requests[2].json();
    let roles: Vec<&str> =
        after["messages"].as_array().expect("messages").iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["user", "system"], "the summary took the conversation's place, the session notes after it");
    assert!(all_text(&requests[2]).contains("HOST SUMMARY: the summary"));
}

#[tokio::test]
async fn without_auto_compaction_only_the_host_summarizes() {
    let dirs = Dirs::new("opt-no-compact");
    let mock = mock([Reply::text("one"), Reply::text("two"), Reply::text("the summary"), Reply::text("three")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project()).mcp(false).compact_at(50_000).auto_compact(false);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    agent.send_text(long_message()).await.expect("alive").expect("turn");
    assert_eq!(mock.requests().len(), 2, "over the threshold, and not summarized");

    assert!(agent.compact(Some("the API names".into())).await.expect("alive").expect("compact"));
    agent.send_text("third").await.expect("alive").expect("turn");
    let requests = mock.requests();
    assert!(all_text(&requests[2]).contains("the API names"), "the host's focus is in the instruction");
    let roles: Vec<String> = requests[3].json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|m| m["role"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(roles, ["user", "user", "system"], "the summary, the new message, the session notes");
}

#[tokio::test]
async fn a_prompt_the_api_finds_too_long_is_summarized_once_and_sent_again() {
    let dirs = Dirs::new("opt-overflow");
    let replies = [Reply::text("one"), Reply::prompt_too_long(), Reply::text("the summary"), Reply::text("two")];
    let mock = mock(replies).await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, mut events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    let outcome = agent.send_text("second").await.expect("alive").expect("the turn recovers");
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let requests = mock.requests();
    assert_eq!(requests.len(), 4, "a turn, the refused request, the summary, the request again");
    let again = requests[3].json();
    let roles: Vec<&str> =
        again["messages"].as_array().expect("messages").iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["user", "system"], "the summary took the conversation's place, the session notes after it");
    assert!(all_text(&requests[3]).contains("the summary"));
    let mut compacted = 0;
    while let Ok(event) = events.try_recv() {
        compacted += usize::from(matches!(event, AgentEvent::Compacted { .. }));
    }
    assert_eq!(compacted, 1);
}

#[tokio::test]
async fn without_auto_compaction_a_prompt_too_long_fails_the_turn() {
    let dirs = Dirs::new("opt-overflow-host");
    let mock = mock([Reply::text("one"), Reply::prompt_too_long()]).await;
    let sdk = sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let options = AgentOptions::new(dirs.project()).mcp(false).auto_compact(false);
    let (agent, mut events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("first").await.expect("alive").expect("turn");
    let failed = agent.send_text("second").await.expect("alive").expect_err("too long");
    assert!(
        matches!(&failed, TurnError::Model(ModelError::ContextWindowExceeded(m)) if m.contains("prompt is too long")),
        "{failed:?}"
    );
    assert_eq!(mock.requests().len(), 2, "no summary was asked for: the host owns compaction");
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, AgentEvent::Compacting { .. } | AgentEvent::Compacted { .. }), "{event:?}");
    }
}

#[tokio::test]
async fn claude_md_can_be_left_out() {
    let dirs = Dirs::new("opt-claude-md");
    dirs.write("project/CLAUDE.md", "Rule: say alpha.");
    let call = r#"{"description":"Helper","prompt":"Help","run_in_background":false}"#;
    let mock = mock([
        Reply::text("with"),
        Reply::tool_use("toolu_a", "Agent", call),
        Reply::text("helped"),
        Reply::text("without"),
    ])
    .await;
    let sdk = sdk(&mock, &dirs).await;
    let (with, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    with.send_text("hello").await.expect("alive").expect("turn");
    let (without, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).claude_md(false)).await.expect("spawn");
    without.send_text("hello").await.expect("alive").expect("turn");

    let requests = mock.requests();
    assert!(all_text(&requests[0]).contains("Rule: say alpha."));
    assert!(!all_text(&requests[1]).contains("Rule: say alpha."));
    assert!(!all_text(&requests[2]).contains("Rule: say alpha."), "its subagents do without it too");
    assert!(all_text(&requests[1]).contains("# Environment"), "the rest of the context stays");
}

#[tokio::test]
async fn plugin_folders_add_their_skills_and_commands() {
    let dirs = Dirs::new("opt-plugins");
    dirs.write("plugins/eos/.claude-plugin/plugin.json", r#"{"name":"eos"}"#);
    dirs.write(
        "plugins/eos/skills/eos-plugin-authoring/SKILL.md",
        "---\ndescription: Build an Eos plugin\n---\nFollow the plugin API.",
    );
    let mock = mock([Reply::text("with"), Reply::text("without")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let plugins = vec![dirs.root.join("plugins/eos")];
    let (with, _events) =
        sdk.spawn(AgentOptions::new(dirs.project()).mcp(false).plugin_dirs(plugins)).await.expect("spawn");
    with.send_input("/eos:eos-plugin-authoring").await.expect("input").expect("turn");
    let (without, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    without.send_text("hello").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let text = all_text(&requests[0]);
    assert!(text.contains("- eos:eos-plugin-authoring: Build an Eos plugin"), "{text}");
    assert!(text.contains("Follow the plugin API."), "its command expands");
    assert!(!all_text(&requests[1]).contains("eos-plugin-authoring"), "another agent of the folder does not see it");
}

#[tokio::test]
async fn the_system_prompt_is_extended_or_replaced_and_tool_lists_narrow_the_tools() {
    let dirs = Dirs::new("opt-prompt");
    let mock = mock([Reply::text("a"), Reply::text("b"), Reply::text("c")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let spawn = |options: AgentOptions| {
        let sdk = sdk.clone();
        async move {
            let (agent, _events) = sdk.spawn(options.mcp(false)).await.expect("spawn");
            agent.send_text("hello").await.expect("alive").expect("turn");
        }
    };
    spawn(AgentOptions::new(dirs.project())).await;
    spawn(AgentOptions::new(dirs.project()).system_prompt(SystemPromptOverride::Append("HOST RULES".into()))).await;
    let only = vec!["Read".to_owned(), "Grep".to_owned(), "Bash".to_owned()];
    spawn(
        AgentOptions::new(dirs.project())
            .system_prompt(SystemPromptOverride::Replace("ONLY THIS".into()))
            .allowed_tools(only)
            .disallowed_tools(vec!["Bash".into()]),
    )
    .await;

    let requests = mock.requests();
    let (stock, appended, replaced) = (system_text(&requests[0]), system_text(&requests[1]), system_text(&requests[2]));
    assert_eq!(appended, format!("{stock}HOST RULES"), "appended after Claude Code's prompt");
    assert_eq!(replaced, "ONLY THIS");
    assert_eq!(tool_names(&requests[2]), ["Grep", "Read"]);
}
