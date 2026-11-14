//! MCP servers connect in the background, as in Claude Code: the first
//! request does not wait for them, the context says they are still
//! connecting, and their tools and instructions are announced once they
//! connect.

mod common;

use std::time::Duration;

use common::mcp::McpMock;
use common::{Dirs, mock};
use eos_agent_mock_api::{Recorded, Reply};
use eos_agent_sdk::{AgentOptions, McpServerState, SdkAgent};
use serde_json::Value;

fn configure(dirs: &Dirs, servers: &[(&str, &McpMock)]) {
    let entries: Vec<String> =
        servers.iter().map(|(name, s)| format!(r#""{name}": {{"type": "http", "url": "{}"}}"#, s.url)).collect();
    dirs.write("home/.claude.json", &format!(r#"{{"mcpServers": {{{}}}}}"#, entries.join(", ")));
}

async fn until_connected(agent: &SdkAgent, server: &str) {
    for _ in 0..200 {
        if agent.mcp_status().iter().any(|s| s.name == server && s.state != McpServerState::Pending) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("{server} did not connect");
}

/// The text of each `role: "system"` message of a request.
fn system_messages(request: &Recorded) -> Vec<String> {
    let body = request.json();
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|m| m["role"] == "system")
        .map(|m| match &m["content"] {
            Value::String(s) => s.clone(),
            blocks => blocks
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect()
}

const STILL_CONNECTING: &str = "The following MCP servers are still connecting \u{2014} their tools (typically named \
mcp__<server>__*) are not yet available but will appear shortly:\ndocs\n\nIf the user's request might be served by one \
of these servers (even if they didn't name it explicitly), call ToolSearch with a relevant keyword \u{2014} ToolSearch \
will wait for connecting servers and search their tools once available. Do not report a capability as unavailable \
without first searching.";

#[tokio::test(flavor = "multi_thread")]
async fn tools_and_instructions_are_announced_once_the_server_connects() {
    let dirs = Dirs::new("mcp-background");
    let server = McpMock::start(&[("lookup", None)], Some("Use lookup to find things.")).await;
    configure(&dirs, &[("docs", &server)]);
    let mock = mock([Reply::text("one"), Reply::text("two")]).await;
    let sdk = common::sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");

    let first = &mock.requests()[0];
    let context = &system_messages(first)[0];
    assert!(context.contains(&format!("WebSearch\n\n{STILL_CONNECTING}")), "{context}");
    assert!(!context.contains("# MCP Server Instructions"));
    assert!(!common::tool_names(first).iter().any(|n| n.starts_with("mcp__")));

    server.open();
    until_connected(&agent, "docs").await;
    agent.send_text("again").await.expect("alive").expect("turn");
    let second = &mock.requests()[1];
    let notes = system_messages(second);
    assert_eq!(notes.len(), 2, "the context, then one note: {notes:?}");
    assert_eq!(
        notes[1],
        "The following deferred tools are now available via ToolSearch. Their schemas are NOT loaded \u{2014} calling \
         them directly will fail with InputValidationError. Use ToolSearch with query \"select:<name>[,<name>...]\" to \
         load tool schemas before calling them:\nmcp__docs__lookup\n\n# MCP Server Instructions\n\nThe following MCP \
         servers have provided instructions for how to use their tools and resources:\n\n## docs\nUse lookup to find \
         things."
    );
    // As Claude Code: a deferred tool is declared once ToolSearch loads it.
    assert!(!common::tool_names(second).iter().any(|n| n == "mcp__docs__lookup"));
}

#[tokio::test(flavor = "multi_thread")]
async fn subagents_start_with_the_tools_connected_since() {
    // (Their notes name the tool; ToolSearch would declare it.)
    let dirs = Dirs::new("mcp-background-subagent");
    let server = McpMock::start(&[("lookup", None)], None).await;
    configure(&dirs, &[("docs", &server)]);
    let call =
        r#"{"description":"look","prompt":"Look it up","subagent_type":"general-purpose","run_in_background":false}"#;
    let mock =
        mock([Reply::text("one"), Reply::tool_use("toolu_a", "Agent", call), Reply::text("sub"), Reply::text("done")])
            .await;
    let sdk = common::sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");
    server.open();
    until_connected(&agent, "docs").await;
    agent.send_text("delegate").await.expect("alive").expect("turn");
    let sub = mock
        .requests()
        .into_iter()
        .find(|r| common::all_text(r).contains("Look it up"))
        .expect("the subagent's request");
    assert!(system_messages(&sub)[0].contains("\nmcp__docs__lookup"), "{:?}", system_messages(&sub));
    assert!(!system_messages(&sub)[0].contains("still connecting"));
}

/// Claude Code's ToolSearch waits for servers still connecting, and a tool it
/// loaded is not announced again.
#[tokio::test(flavor = "multi_thread")]
async fn tool_search_waits_for_connecting_servers() {
    let dirs = Dirs::new("mcp-background-search");
    let server = std::sync::Arc::new(McpMock::start(&[("lookup", None)], None).await);
    configure(&dirs, &[("docs", &server)]);
    let mock = std::sync::Arc::new(
        mock([Reply::tool_use("toolu_s", "ToolSearch", r#"{"query":"lookup"}"#), Reply::text("found")]).await,
    );
    let sdk = common::sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    let opener = {
        let requests = std::sync::Arc::clone(&mock);
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            while requests.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            server.open();
        })
    };
    agent.send_text("find lookup").await.expect("alive").expect("turn");
    opener.await.expect("opener");
    let second = &mock.requests()[1];
    let body = second.json();
    let result = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result")
        .expect("the search result");
    assert_eq!(result["content"][0], serde_json::json!({"type": "tool_reference", "tool_name": "mcp__docs__lookup"}));
    assert!(common::tool_names(second).iter().any(|n| n == "mcp__docs__lookup"));
    assert_eq!(system_messages(second).len(), 1, "the loaded tool is not announced: {:?}", system_messages(second));
}

/// The blocks of the request's last `role: "system"` message that are not text.
fn tool_blocks(request: &Recorded) -> Vec<Value> {
    let body = request.json();
    let last = body["messages"].as_array().expect("messages").iter().rev().find(|m| m["role"] == "system").cloned();
    last.and_then(|m| m["content"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b["type"] != "text")
        .collect()
}

/// With Claude Code's tool-change betas a tool sent in full that arrives
/// later comes in a `tool_addition` block, the `tools` array stays as it
/// was, a changed definition is sent again and a tool that went away is
/// withdrawn with a `tool_removal` block.
#[tokio::test(flavor = "multi_thread")]
async fn late_tools_come_in_tool_addition_blocks() {
    let dirs = Dirs::new("mcp-tool-addition");
    let pinned = Some(serde_json::json!({"anthropic/alwaysLoad": true}));
    let server = McpMock::start(&[("lookup", None), ("pinned", pinned.clone())], None).await;
    configure(&dirs, &[("docs", &server)]);
    let mock = mock([Reply::text("1"), Reply::text("2"), Reply::text("3"), Reply::text("4")]).await;
    let sdk = inline_sdk(&mock, &dirs, true).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");
    server.open();
    until_connected(&agent, "docs").await;
    agent.send_text("again").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let betas = requests[1].headers["anthropic-beta"].to_str().expect("ascii").to_owned();
    assert!(betas.contains("mid-conversation-tool-changes-2026-07-01,inline-tools-2026-09-15"), "{betas}");
    assert_eq!(
        common::tool_names(&requests[0]),
        common::tool_names(&requests[1]),
        "the deferred tool is not loaded, the pinned one is defined in the conversation"
    );
    assert!(!common::tool_names(&requests[1]).iter().any(|n| n == "mcp__docs__pinned"));
    let note = system_messages(&requests[1]).pop().expect("note");
    assert!(
        note.starts_with(
            "The following tools just became available and are ready to use:\nmcp__docs__pinned\n\nThe following \
             deferred tools are now available via ToolSearch."
        ),
        "{note}"
    );
    let blocks = tool_blocks(&requests[1]);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["type"], "tool_addition");
    assert_eq!(blocks[0]["tool"]["type"], "tool_definition");
    assert_eq!(blocks[0]["tool"]["definition"]["name"], "mcp__docs__pinned");
    assert!(blocks[0]["tool"]["definition"].get("defer_loading").is_none());
    assert!(blocks[0]["cache_control"].is_object(), "the newest message carries the cache marker");

    // A new definition is sent again.
    let mut changed = common::mcp::tool("pinned", pinned);
    changed["description"] = serde_json::json!("Pinned, now better.");
    server.set_raw_tools(vec![common::mcp::tool("lookup", None), changed]);
    agent.mcp_reconnect("docs").await.expect("reconnect");
    agent.send_text("three").await.expect("alive").expect("turn");
    let third = &mock.requests()[2];
    let note = system_messages(third).pop().expect("note");
    assert_eq!(
        note,
        "The following tools have updated definitions, which replace the earlier ones from here on:\nmcp__docs__pinned"
    );
    let blocks = tool_blocks(third);
    assert_eq!(blocks[0]["tool"]["definition"]["description"], "Pinned, now better.");

    // A tool that goes away is withdrawn.
    server.set_tools(&[("lookup", None)]);
    agent.mcp_reconnect("docs").await.expect("reconnect");
    agent.send_text("four").await.expect("alive").expect("turn");
    let fourth = &mock.requests()[3];
    let note = system_messages(fourth).pop().expect("note");
    assert!(
        note.starts_with("The following tools are no longer available. Do not call them:\nmcp__docs__pinned\n\n"),
        "{note}"
    );
    let blocks = tool_blocks(fourth);
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": "mcp__docs__pinned"}, "cache_control": blocks[0]["cache_control"]})
    );
}

/// An SDK on Opus 5.5 with Claude Code's inline tools on or off.
async fn inline_sdk(mock: &eos_agent_mock_api::MockApi, dirs: &Dirs, on: bool) -> eos_agent_sdk::Sdk {
    use std::sync::Arc;
    eos_agent_sdk::Sdk::builder()
        .auth(Arc::new(eos_agent_core::StaticCredential(eos_agent_core::Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("claude-opus-5-5")
        .claude_paths(eos_agent_compat::ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .inline_tools(on)
        .build()
        .await
        .expect("sdk")
}

/// With inline tools off (Claude Code's `tengu_brisk_meadow` off) a late
/// tool sent in full is added by reference: the block names it, the `tools`
/// array declares it deferred, and when it goes away it stays declared
/// (Claude Code withdraws by block only what it defined by value).
#[tokio::test(flavor = "multi_thread")]
async fn late_tools_are_added_by_reference_with_inline_tools_off() {
    let dirs = Dirs::new("mcp-tool-reference");
    let pinned = Some(serde_json::json!({"anthropic/alwaysLoad": true}));
    let server = McpMock::start(&[("pinned", pinned)], None).await;
    configure(&dirs, &[("docs", &server)]);
    let mock = mock([Reply::text("1"), Reply::text("2"), Reply::text("3")]).await;
    let sdk = inline_sdk(&mock, &dirs, false).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");
    server.open();
    until_connected(&agent, "docs").await;
    agent.send_text("again").await.expect("alive").expect("turn");

    let second = &mock.requests()[1];
    let betas = second.headers["anthropic-beta"].to_str().expect("ascii").to_owned();
    assert!(betas.contains("mid-conversation-tool-changes-2026-07-01") && !betas.contains("inline-tools"), "{betas}");
    let blocks = tool_blocks(second);
    assert_eq!(blocks[0]["tool"], serde_json::json!({"type": "tool_reference", "name": "mcp__docs__pinned"}));
    let body = second.json();
    let declared = body["tools"].as_array().expect("tools").iter().find(|t| t["name"] == "mcp__docs__pinned").cloned();
    assert_eq!(declared.expect("declared")["defer_loading"], true);

    server.set_tools(&[]);
    agent.mcp_reconnect("docs").await.expect("reconnect");
    agent.send_text("three").await.expect("alive").expect("turn");
    let third = &mock.requests()[2];
    assert!(tool_blocks(third).iter().all(|b| b["type"] != "tool_removal"), "{:?}", tool_blocks(third));
    assert!(common::tool_names(third).iter().any(|n| n == "mcp__docs__pinned"), "kept declared");
}

/// As Claude Code: when the API refuses inline tools the request goes again
/// by reference, and when it refuses tool changes, with the tool in the
/// `tools` array; the next agents keep away from a refused beta header.
#[tokio::test(flavor = "multi_thread")]
async fn refused_tool_change_betas_are_retried_without_them() {
    let dirs = Dirs::new("mcp-tool-refused");
    let pinned = Some(serde_json::json!({"anthropic/alwaysLoad": true}));
    let server = McpMock::start(&[("pinned", pinned)], None).await;
    configure(&dirs, &[("docs", &server)]);
    let refuse = |message: &str| Reply::Status {
        status: 400,
        headers: Vec::new(),
        body: serde_json::json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
            .to_string(),
    };
    let mock = mock([
        Reply::text("1"),
        refuse("messages.2.content.1: Inline tool definitions are not available on this platform"),
        refuse("Unexpected value(s) `mid-conversation-tool-changes-2026-07-01` for the `anthropic-beta` header"),
        Reply::text("2"),
        Reply::text("3"),
    ])
    .await;
    let sdk = inline_sdk(&mock, &dirs, true).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    agent.send_text("hello").await.expect("alive").expect("turn");
    server.open();
    until_connected(&agent, "docs").await;
    agent.send_text("again").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let betas = |r: &Recorded| r.headers["anthropic-beta"].to_str().expect("ascii").to_owned();
    assert!(betas(&requests[1]).contains("inline-tools-2026-09-15"));
    assert_eq!(tool_blocks(&requests[1])[0]["tool"]["type"], "tool_definition");
    // By reference: no definition in the conversation, the tool in the array.
    assert!(!betas(&requests[2]).contains("inline-tools") && betas(&requests[2]).contains("mid-conversation"));
    assert!(tool_blocks(&requests[2]).is_empty(), "{:?}", tool_blocks(&requests[2]));
    assert!(common::tool_names(&requests[2]).iter().any(|n| n == "mcp__docs__pinned"));
    // Without either beta.
    assert!(!betas(&requests[3]).contains("mid-conversation"), "{}", betas(&requests[3]));
    assert!(common::tool_names(&requests[3]).iter().any(|n| n == "mcp__docs__pinned"));
    let pinned = requests[3].json()["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|t| t["name"] == "mcp__docs__pinned")
        .cloned();
    assert!(pinned.expect("declared").get("defer_loading").is_none(), "in full");
    assert!(system_messages(&requests[3]).iter().all(|m| !m.contains("just became available")));

    // The header refusal holds for the next agent.
    let (next, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    next.send_text("hi").await.expect("alive").expect("turn");
    assert!(!betas(&mock.requests()[4]).contains("mid-conversation"));
}

/// As Claude Code's subagents (which refresh their tool pool), a running
/// subagent hears of an MCP server that connects after it started.
#[tokio::test(flavor = "multi_thread")]
async fn running_subagents_follow_servers_that_connect() {
    let dirs = Dirs::new("mcp-background-running-subagent");
    let server = std::sync::Arc::new(McpMock::start(&[("lookup", None)], None).await);
    configure(&dirs, &[("docs", &server)]);
    let call =
        r#"{"description":"look","prompt":"Look it up","subagent_type":"general-purpose","run_in_background":false}"#;
    let Reply::Stream(mut slow) = Reply::tool_use("toolu_g", "Glob", r#"{"pattern":"*.none"}"#) else {
        unreachable!("a stream")
    };
    slow.insert(0, eos_agent_mock_api::Step::Delay(Duration::from_millis(1500)));
    let mock = std::sync::Arc::new(
        mock([Reply::tool_use("toolu_a", "Agent", call), Reply::Stream(slow), Reply::text("sub"), Reply::text("done")])
            .await,
    );
    let sdk = common::sdk_with_model(&mock, &dirs, "claude-opus-5-5").await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project())).await.expect("spawn");
    let opener = {
        let requests = std::sync::Arc::clone(&mock);
        let server = std::sync::Arc::clone(&server);
        tokio::spawn(async move {
            while requests.requests().len() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            server.open();
        })
    };
    agent.send_text("delegate").await.expect("alive").expect("turn");
    opener.await.expect("opener");
    let requests = mock.requests();
    assert!(common::all_text(&requests[1]).contains("Look it up"));
    let notes = system_messages(&requests[2]);
    assert!(notes.iter().any(|n| n.contains("ToolSearch") && n.contains("\nmcp__docs__lookup")), "{notes:?}");
}
