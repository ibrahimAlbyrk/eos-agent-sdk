//! A host's own tools on an agent: sent, called, gated, limited and seen in
//! events like the built-in ones.

mod common;

use std::sync::{Arc, Mutex};

use common::{Dirs, all_text, builder, mock, tool_names};
use eos_agent_core::{
    AgentEvent, BoxFuture, Extensions, Permission, PermissionGate, Tool, ToolCall, ToolContext, ToolOutput, ToolSpec,
};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, HostTool, Sdk};
use eos_agent_types::ToolResultContent;
use serde_json::json;
use serde_json::value::RawValue;

/// The host's state for one session, as a host tool finds it.
struct Session {
    worker: &'static str,
}

/// Says which worker called it and with what.
struct WhoAmI(ToolSpec);

impl WhoAmI {
    fn tool(name: &str) -> Arc<dyn Tool> {
        Arc::new(Self(spec(name)))
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        "Reports the calling worker.",
        json!({"type": "object", "properties": {"note": {"type": "string"}}}),
    )
}

impl Tool for WhoAmI {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let worker = ctx.extensions.get::<Session>().map_or("nobody", |s| s.worker);
            let note = serde_json::from_str::<serde_json::Value>(input.get()).expect("json")["note"].to_string();
            ToolOutput::text(format!("worker={worker} note={note}"))
        })
    }
}

/// Answers with `self.1` characters, and details for the host.
struct Loud(ToolSpec, usize);

impl Tool for Loud {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    fn call<'a>(&'a self, _input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { ToolOutput::text("x".repeat(self.1)).with_details(&json!({"viewId": "v_123"})) })
    }
}

/// Notes what it was asked; refuses calls whose input says `"note":"no"`.
#[derive(Default)]
struct Noting(Mutex<Vec<String>>);

impl PermissionGate for Noting {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        self.0.lock().expect("lock").push(call.name.to_owned());
        let refused = call.input.get().contains(r#""note":"no""#);
        Box::pin(async move { if refused { Permission::Deny("The host said no.".into()) } else { Permission::Allow } })
    }
}

fn seeded(worker: &'static str) -> Extensions {
    let seed = Extensions::default();
    seed.insert(Session { worker });
    seed
}

async fn sdk_with_gate(mock: &MockApi, dirs: &Dirs) -> (Sdk, Arc<Noting>) {
    let gate = Arc::new(Noting::default());
    let sdk = builder(mock, dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    (sdk, gate)
}

#[tokio::test]
async fn a_host_tool_is_sent_called_gated_and_reaches_the_hosts_session() {
    let dirs = Dirs::new("host-tool");
    let mock = mock([
        Reply::tool_use("toolu_1", "mcp__worker__whoami", r#"{"note":"hi"}"#),
        Reply::tool_use("toolu_2", "mcp__worker__whoami", r#"{"note":"no"}"#),
        Reply::text("done"),
    ])
    .await;
    let (sdk, gate) = sdk_with_gate(&mock, &dirs).await;
    let options = AgentOptions::new(dirs.project())
        .mcp(false)
        .tools(vec![HostTool::new(WhoAmI::tool("mcp__worker__whoami"))])
        .extensions(seeded("w-7"));
    let (agent, mut events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("who am I").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let names = tool_names(&requests[0]);
    // MCP-named tools go after the built-in ones, as Claude Code orders them.
    assert_eq!(names.last().map(String::as_str), Some("mcp__worker__whoami"), "{names:?}");
    let sent = requests[0].json()["tools"].as_array().expect("tools").last().cloned().expect("tool");
    assert_eq!(sent["description"], "Reports the calling worker.");
    assert!(sent.get("defer_loading").is_none(), "sent in full unless its spec defers it");

    assert!(all_text(&requests[1]).contains(r#"worker=w-7 note="hi""#), "{}", all_text(&requests[1]));
    assert!(all_text(&requests[2]).contains("The host said no."));
    assert_eq!(*gate.0.lock().expect("lock"), ["mcp__worker__whoami", "mcp__worker__whoami"]);

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            AgentEvent::ToolUse { name, .. } => seen.push(format!("use {name}")),
            AgentEvent::ToolResult { output, .. } => seen.push(format!("result error={}", output.is_error)),
            _ => {}
        }
    }
    assert_eq!(
        seen,
        ["use mcp__worker__whoami", "result error=false", "use mcp__worker__whoami", "result error=true"],
        "its calls and results are in the agent's events"
    );
}

#[tokio::test]
async fn a_deferred_host_tool_waits_behind_tool_search() {
    let dirs = Dirs::new("host-deferred");
    let mock = mock([
        Reply::tool_use("toolu_s", "ToolSearch", r#"{"query":"geocode a street address","max_results":5}"#),
        Reply::tool_use("toolu_p", "mcp__worker__find_places", r#"{"note":"cafe"}"#),
        Reply::text("done"),
    ])
    .await;
    let (sdk, _gate) = sdk_with_gate(&mock, &dirs).await;
    let deferred: Arc<dyn Tool> =
        Arc::new(WhoAmI(spec("mcp__worker__find_places").deferred().with_search_hint("geocode a street address")));
    let tools = vec![HostTool::new(WhoAmI::tool("mcp__worker__present")), HostTool::new(deferred)];
    let options = AgentOptions::new(dirs.project()).mcp(false).tools(tools).extensions(seeded("w-1"));
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("find a cafe").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let first = tool_names(&requests[0]);
    assert!(first.iter().any(|n| n == "mcp__worker__present"), "the tool sent in full is declared at once");
    assert!(!first.iter().any(|n| n == "mcp__worker__find_places"), "the deferred one only once loaded: {first:?}");
    assert!(all_text(&requests[0]).contains("\nmcp__worker__find_places\n"), "but it is announced by name");

    let second = requests[1].json();
    let loaded =
        second["messages"].as_array().expect("messages").last().expect("result")["content"][0]["content"][0].clone();
    assert_eq!(loaded, json!({"type": "tool_reference", "tool_name": "mcp__worker__find_places"}), "found by its hint");
    let declared = second["tools"].as_array().expect("tools").iter().find(|t| t["name"] == "mcp__worker__find_places");
    assert_eq!(declared.expect("declared once loaded")["defer_loading"], true);
    assert!(all_text(&requests[2]).contains("worker=w-1"));
}

#[tokio::test]
async fn a_main_agent_only_tool_stays_with_the_agent_it_was_given_to() {
    let dirs = Dirs::new("host-main-only");
    let sub = r#"{"description":"Helper","prompt":"Help","run_in_background":false}"#;
    let fork = r#"{"description":"Fork","prompt":"Try it","subagent_type":"fork","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_a", "Agent", sub),
        Reply::tool_use("toolu_c", "mcp__worker__shared", r#"{"note":"from the subagent"}"#),
        Reply::text("helped"),
        Reply::tool_use("toolu_f", "Agent", fork),
        Reply::tool_use("toolu_x", "mcp__orchestrator__spawn_worker", r#"{"note":"from the fork"}"#),
        Reply::text("refused"),
        Reply::text("done"),
    ])
    .await;
    let (sdk, gate) = sdk_with_gate(&mock, &dirs).await;
    let tools = vec![
        HostTool::new(WhoAmI::tool("mcp__orchestrator__spawn_worker")).main_agent_only(),
        HostTool::new(WhoAmI::tool("mcp__worker__shared")),
    ];
    let options = AgentOptions::new(dirs.project()).mcp(false).tools(tools).extensions(seeded("w-9"));
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("delegate").await.expect("alive").expect("turn");

    let requests = mock.requests();
    let (main, child, forked) = (tool_names(&requests[0]), tool_names(&requests[1]), tool_names(&requests[4]));
    assert!(main.iter().any(|n| n == "mcp__orchestrator__spawn_worker"));
    assert!(child.iter().any(|n| n == "mcp__worker__shared"), "a subagent gets the host's tools by the usual rule");
    assert!(!child.iter().any(|n| n == "mcp__orchestrator__spawn_worker"), "but not a main-agent-only one");
    assert!(all_text(&requests[2]).contains("worker=w-9"), "and finds the same session in its extensions");
    assert_eq!(forked, main, "a fork keeps its parent's tool list");
    let refused = all_text(&requests[5]);
    assert!(refused.contains("mcp__orchestrator__spawn_worker is only available to the main agent."), "{refused}");
    let asked = gate.0.lock().expect("lock").clone();
    assert_eq!(asked, ["Agent", "mcp__worker__shared", "Agent"], "the refusal comes before the host's gate is asked");
}

#[tokio::test]
async fn host_tools_follow_the_tool_lists_and_the_result_limit() {
    let dirs = Dirs::new("host-limits");
    let mock = mock([Reply::tool_use("toolu_big", "mcp__worker__dump", "{}"), Reply::text("done")]).await;
    let (sdk, _gate) = sdk_with_gate(&mock, &dirs).await;
    let tools = vec![
        HostTool::new(Arc::new(Loud(spec("mcp__worker__dump"), 150_000))),
        HostTool::new(WhoAmI::tool("mcp__worker__hidden")),
        // Takes the place of the built-in tool of its name.
        HostTool::new(WhoAmI::tool("AskUserQuestion")),
    ];
    let options = AgentOptions::new(dirs.project())
        .mcp(false)
        .tools(tools)
        .disallowed_tools(vec!["mcp__worker__hidden".into(), "Write".into()]);
    let (agent, mut events) = sdk.spawn(options).await.expect("spawn");
    agent.send_text("dump").await.expect("alive").expect("turn");

    let request = &mock.requests()[0];
    let names = tool_names(request);
    assert!(!names.iter().any(|n| n == "mcp__worker__hidden" || n == "Write"), "{names:?}");
    assert_eq!(names.iter().filter(|n| *n == "AskUserQuestion").count(), 1);
    let ask =
        request.json()["tools"].as_array().expect("tools").iter().find(|t| t["name"] == "AskUserQuestion").cloned();
    assert_eq!(ask.expect("sent")["description"], "Reports the calling worker.");

    let result = loop {
        match events.recv().await.expect("events") {
            AgentEvent::ToolResult { output, .. } => break output,
            _ => continue,
        }
    };
    let ToolResultContent::Text(text) = &result.content else { panic!("text") };
    assert!(text.starts_with("<persisted-output>\nOutput too large (146.5KB). Full output saved to: "), "{text}");
    assert!(text.len() < 4_000);
    let details = result.details.as_ref().expect("what the tool told the host comes with the result");
    assert_eq!(details.get(), r#"{"viewId":"v_123"}"#);
}
