//! The hub against a real stdio server (`tests/support/test_server.rs`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use eos_agent_core::{CancellationToken, Extensions, Tool, ToolContext, ToolOutput, Workspace};
use eos_agent_mcp::{McpHub, McpLimits, McpServerConfig, McpToolset, McpTransport};
use eos_agent_types::{ContentBlock, ToolResultContent};
use serde_json::Value;
use serde_json::value::RawValue;

fn server(name: &str) -> McpServerConfig {
    McpServerConfig::stdio(name, env!("CARGO_BIN_EXE_eos-mcp-test-server"), &[])
}

fn hub_with(edit: impl FnOnce(&mut McpLimits)) -> Arc<McpHub> {
    let mut limits = McpLimits { connect_timeout: Duration::from_secs(10), ..McpLimits::default() };
    edit(&mut limits);
    McpHub::new(limits)
}

fn find(set: &McpToolset, name: &str) -> Arc<dyn Tool> {
    set.tools().into_iter().find(|t| t.spec().name() == name).unwrap_or_else(|| panic!("no tool {name}"))
}

fn scratch() -> PathBuf {
    std::env::temp_dir().join(format!("eos-mcp-tests-{}", std::process::id()))
}

async fn call_with(set: &McpToolset, name: &str, input: &str, cancel: &CancellationToken) -> ToolOutput {
    let tool = find(set, name);
    let input = RawValue::from_string(input.to_owned()).expect("json");
    let extensions = Extensions::default();
    extensions.insert(Workspace::new(scratch(), vec![], scratch()));
    let ctx = ToolContext { agent_id: "a1", tool_use_id: "t1", cancel, extensions: &extensions, transcript: &[] };
    tool.call(&input, ctx).await
}

async fn call(set: &McpToolset, name: &str, input: &str) -> ToolOutput {
    call_with(set, name, input, &CancellationToken::new()).await
}

fn text(out: &ToolOutput) -> String {
    match &out.content {
        ToolResultContent::Text(t) => t.clone(),
        ToolResultContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect(),
    }
}

async fn pid(set: &McpToolset, server: &str) -> u32 {
    text(&call(set, &format!("mcp__{server}__pid"), "{}").await).parse().expect("pid")
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn gone(pid: u32) -> bool {
    for _ in 0..60 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_tools_with_claude_code_names_and_meta() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("test srv")]).await;
    assert!(set.failures().is_empty(), "{:?}", set.failures());
    let names: Vec<String> = set.tools().iter().map(|t| t.spec().name().to_owned()).collect();
    for expected in ["mcp__test_srv__echo", "mcp__test_srv__get_thing", "ListMcpResourcesTool", "ReadMcpResourceTool"] {
        assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
    }
    let echo = find(&set, "mcp__test_srv__echo");
    assert!(echo.spec().is_deferred());
    assert_eq!(echo.spec().search_hint(), Some("say it back"));
    let def: Value = serde_json::from_slice(echo.spec().json()).expect("json");
    assert_eq!(def["description"], "Echo the text back.");
    assert_eq!(def["input_schema"]["type"], "object");
    assert!(!find(&set, "mcp__test_srv__pid").spec().is_deferred(), "anthropic/alwaysLoad");
    let instructions = set.instructions();
    assert_eq!(instructions.len(), 1);
    assert_eq!((instructions[0].server.as_str(), &*instructions[0].text), ("test srv", "Use echo to repeat things."));
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_return_text_errors_and_images() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("t")]).await;
    let out = call(&set, "mcp__t__echo", r#"{"text":"hello"}"#).await;
    assert!(!out.is_error);
    assert_eq!(text(&out), "hello");
    let out = call(&set, "mcp__t__fail", "{}").await;
    assert!(out.is_error);
    assert_eq!(text(&out), "boom");
    let out = call(&set, "mcp__t__image", "{}").await;
    let ToolResultContent::Blocks(blocks) = &out.content else { panic!("blocks") };
    assert_eq!(blocks[0].kind(), Some("image"));
}

#[tokio::test(flavor = "multi_thread")]
async fn output_is_capped_unless_the_tool_allows_more() {
    let hub = hub_with(|l| l.max_output_tokens = 100);
    let set = hub.connected_toolset(&[server("t")]).await;
    let out = text(&call(&set, "mcp__t__big", r#"{"chars":1000}"#).await);
    assert!(out.starts_with(&"x".repeat(400)) && !out.starts_with(&"x".repeat(401)));
    assert!(out.contains("[OUTPUT TRUNCATED - exceeded 100 token limit]"));
    let out = text(&call(&set, "mcp__t__big_ok", r#"{"chars":1000}"#).await);
    assert_eq!(out, "x".repeat(1000), "anthropic/maxResultSizeChars raises the cap");
}

#[tokio::test(flavor = "multi_thread")]
async fn equal_configs_share_one_process_until_the_last_user_drops() {
    let hub = hub_with(|_| {});
    let a = hub.connected_toolset(&[server("t")]).await;
    let b = hub.connected_toolset(&[server("t")]).await;
    let pid_a = pid(&a, "t").await;
    assert_eq!(pid_a, pid(&b, "t").await);
    assert_eq!(hub.shared_servers(), 1);
    assert!(Arc::ptr_eq(&find(&a, "mcp__t__echo"), &find(&b, "mcp__t__echo")), "tool objects are shared too");

    drop(a);
    assert_eq!(pid(&b, "t").await, pid_a, "still serving the other agent");
    drop(b);
    assert!(gone(pid_a).await, "server outlived its last user");
    assert_eq!(hub.shared_servers(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn unshared_configs_get_their_own_process() {
    let hub = hub_with(|_| {});
    let mut config = server("t");
    config.shared = false;
    let a = hub.connected_toolset(std::slice::from_ref(&config)).await;
    let b = hub.connected_toolset(&[config]).await;
    let (pid_a, pid_b) = (pid(&a, "t").await, pid(&b, "t").await);
    assert_ne!(pid_a, pid_b);
    assert_eq!(hub.shared_servers(), 0);
    drop(a);
    assert!(gone(pid_a).await);
    assert!(alive(pid_b));
}

#[tokio::test(flavor = "multi_thread")]
async fn dropping_kills_the_whole_process_group() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("t")]).await;
    let server_pid = pid(&set, "t").await;
    let child_pid: u32 = text(&call(&set, "mcp__t__spawn_child", "{}").await).parse().expect("pid");
    assert!(alive(child_pid));
    drop(set);
    assert!(gone(server_pid).await);
    assert!(gone(child_pid).await, "the server's own child must not be orphaned");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_list_changes_reach_the_toolset() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("t")]).await;
    let before = set.version();
    assert_eq!(text(&call(&set, "mcp__t__add_tool", "{}").await), "added");
    for _ in 0..50 {
        if set.version() != before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_ne!(set.version(), before);
    assert_eq!(text(&call(&set, "mcp__t__extra", "{}").await), "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_call_notifies_the_server() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("t")]).await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let out = call_with(&set, "mcp__t__slow", r#"{"ms":600}"#, &cancel).await;
    assert!(out.is_error);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(text(&call(&set, "mcp__t__cancelled", "{}").await), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_tools_list_and_read() {
    let hub = hub_with(|_| {});
    let set = hub.connected_toolset(&[server("t")]).await;
    let listed: Value = serde_json::from_str(&text(&call(&set, "ListMcpResourcesTool", "{}").await)).expect("json");
    assert_eq!(listed[0]["uri"], "test://hello");
    assert_eq!(listed[0]["server"], "t");
    let out = call(&set, "ListMcpResourcesTool", r#"{"server":"nope"}"#).await;
    assert!(out.is_error && text(&out).starts_with("Server \"nope\" not found. Available servers: t"));

    let read: Value =
        serde_json::from_str(&text(&call(&set, "ReadMcpResourceTool", r#"{"server":"t","uri":"test://hello"}"#).await))
            .expect("json");
    assert_eq!(read["contents"][0]["text"], "hello world");
    let read: Value =
        serde_json::from_str(&text(&call(&set, "ReadMcpResourceTool", r#"{"server":"t","uri":"test://blob"}"#).await))
            .expect("json");
    let saved = read["contents"][0]["blobSavedTo"].as_str().expect("saved");
    assert_eq!(std::fs::read(saved).expect("blob"), [0, 1, 2]);
    let missing = text(&call(&set, "ReadMcpResourceTool", r#"{"server":"t","uri":"test://missing"}"#).await);
    assert!(missing.starts_with("Resource not found: test://missing"), "{missing}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_servers_are_reported_not_fatal() {
    let hub = hub_with(|l| l.connect_timeout = Duration::from_secs(5));
    let sse = McpServerConfig::new(
        "old",
        McpTransport::Sse { url: "http://127.0.0.1:9/sse".into(), headers: Default::default(), oauth: None },
    );
    let set = hub
        .connected_toolset(&[
            server("good"),
            McpServerConfig::stdio("missing", "/nonexistent/server", &[]),
            McpServerConfig::http("web", "http://127.0.0.1:9/mcp"),
            sse,
        ])
        .await;
    let failed: Vec<String> = set.failures().into_iter().map(|f| f.server).collect();
    assert_eq!(failed, ["missing", "web", "old"]);
    assert_eq!(set.servers(), ["good"]);
    assert_eq!(hub.shared_servers(), 1, "failed servers are not kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_connections_close_and_reconnect_on_use() {
    let hub = hub_with(|l| l.stdio_idle_timeout = Duration::from_millis(300));
    let set = hub.connected_toolset(&[server("t")]).await;
    let first = pid(&set, "t").await;
    assert!(gone(first).await, "idle server was not closed");
    let second = pid(&set, "t").await;
    assert_ne!(first, second);
}

/// A server that takes `delay` before it starts.
fn slow_server(name: &str, delay: &str) -> McpServerConfig {
    let script = format!("sleep {delay}; exec '{}'", env!("CARGO_BIN_EXE_eos-mcp-test-server"));
    McpServerConfig::stdio(name, "/bin/sh", &["-c", &script])
}

#[tokio::test(flavor = "multi_thread")]
async fn servers_connect_in_the_background() {
    let hub = hub_with(|_| {});
    let started = std::time::Instant::now();
    let set = hub.toolset(&[slow_server("slow", "0.8")]).await;
    assert!(started.elapsed() < Duration::from_millis(500), "the set came back without waiting");
    assert_eq!(set.pending(), ["slow"]);
    assert!(set.tools().is_empty() && set.servers().is_empty());
    assert_eq!(set.status()[0].state, eos_agent_mcp::McpServerState::Pending);
    let before = set.version();
    set.wait_for_pending(&["slow".to_owned()], Duration::from_secs(10)).await;
    assert!(set.pending().is_empty());
    assert_ne!(set.version(), before, "connecting changes the version");
    assert_eq!(set.servers(), ["slow"]);
    assert!(set.tools().iter().any(|t| t.spec().name() == "mcp__slow__echo"));
}

#[tokio::test(flavor = "multi_thread")]
async fn always_load_servers_are_waited_for() {
    let hub = hub_with(|_| {});
    let mut config = slow_server("eager", "0.3");
    config.always_load = true;
    let set = hub.toolset(&[config, slow_server("lazy", "2")]).await;
    assert_eq!(set.servers(), ["eager"], "the alwaysLoad server was waited for");
    assert_eq!(set.pending(), ["lazy"]);
}
