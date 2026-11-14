//! What a host's permission gate is told and may answer.

mod common;

use std::sync::{Arc, Mutex};

use common::{Dirs, agent, all_text, builder, mock};
use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall};
use eos_agent_mock_api::Reply;
use serde_json::value::RawValue;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Asked {
    tool: String,
    agent: String,
    root: String,
    parent: Option<String>,
    started_by: Option<String>,
}

/// Notes every request; a Glob call is sent on with another pattern.
#[derive(Default)]
struct Noting(Mutex<Vec<Asked>>);

impl PermissionGate for Noting {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        self.0.lock().expect("lock").push(Asked {
            tool: call.name.to_owned(),
            agent: call.agent_id.to_owned(),
            root: call.root_id().to_owned(),
            parent: call.origin.map(|o| o.parent_id.to_string()),
            started_by: call.origin.and_then(|o| o.tool_use_id.as_deref().map(str::to_owned)),
        });
        let answer = if call.name == "Glob" {
            let input = RawValue::from_string(r#"{"pattern":"*.toml"}"#.to_owned()).expect("json");
            Permission::AllowWith(Arc::from(input))
        } else {
            Permission::Allow
        };
        Box::pin(std::future::ready(answer))
    }
}

#[tokio::test]
async fn the_gate_sees_a_subagents_root_and_parent_and_may_change_the_input() {
    let dirs = Dirs::new("gate-origin");
    dirs.write("project/notes.md", "notes");
    dirs.write("project/Cargo.toml", "[package]");
    let call = r#"{"description":"Find files","prompt":"List the markdown files","run_in_background":false}"#;
    let mock = mock([
        Reply::tool_use("toolu_a", "Agent", call),
        Reply::tool_use("toolu_g", "Glob", r#"{"pattern":"*.md"}"#),
        Reply::text("found them"),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(Noting::default());
    let sdk =
        builder(&mock, &dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("look around").await.expect("alive").expect("turn");

    let asked = gate.0.lock().expect("lock").clone();
    assert_eq!(asked.len(), 2, "{asked:?}");
    let root = agent.id().to_owned();
    assert_eq!(
        asked[0],
        Asked { tool: "Agent".into(), agent: root.clone(), root: root.clone(), parent: None, started_by: None }
    );
    assert_eq!(asked[1].tool, "Glob");
    assert_ne!(asked[1].agent, root, "the subagent asks under its own id");
    assert_eq!(asked[1].root, root);
    assert_eq!(asked[1].parent.as_deref(), Some(root.as_str()));
    assert_eq!(asked[1].started_by.as_deref(), Some("toolu_a"), "the Agent call that started it");

    // The tool ran with the gate's input; the conversation keeps the model's call.
    let requests = mock.requests();
    let after = all_text(&requests[2]);
    assert!(after.contains("Cargo.toml") && !after.contains("notes.md"), "{after}");
    assert!(after.contains("*.md"), "the call as the model wrote it");
}

/// Notes each call's declared modes; applies them or not, as told.
struct Modes {
    applies: bool,
    asked: Mutex<Vec<(String, Vec<eos_agent_core::DeclaredMode>)>>,
}

impl PermissionGate for Modes {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let modes = call.origin.map(|o| o.modes.to_vec()).unwrap_or_default();
        self.asked.lock().expect("lock").push((call.name.to_owned(), modes));
        Box::pin(std::future::ready(Permission::Allow))
    }

    fn applies_agent_modes(&self) -> bool {
        self.applies
    }
}

async fn planner_writes(applies: bool) -> (Arc<Modes>, Dirs, String) {
    let dirs = Dirs::new(if applies { "gate-modes-applied" } else { "gate-modes-kept" });
    dirs.write(
        "project/.claude/agents/planner.md",
        "---\nname: planner\ndescription: Plans\npermissionMode: plan\n---\nPlan.",
    );
    let call =
        r#"{"description":"Plan it","prompt":"Write the plan","subagent_type":"planner","run_in_background":false}"#;
    let write = r#"{"file_path":"plan.md","content":"steps"}"#;
    let mock = mock([
        Reply::tool_use("toolu_a", "Agent", call),
        Reply::tool_use("toolu_w", "Write", write),
        Reply::text("wrote it"),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(Modes { applies, asked: Mutex::default() });
    let sdk =
        builder(&mock, &dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("plan").await.expect("alive").expect("turn");
    let sub_saw = all_text(&mock.requests()[2]);
    (gate, dirs, sub_saw)
}

#[tokio::test]
async fn a_definitions_plan_mode_reaches_the_gate_with_each_call() {
    let (gate, dirs, _) = planner_writes(true).await;
    let asked = gate.asked.lock().expect("lock").clone();
    assert_eq!(asked[0], ("Agent".to_owned(), Vec::new()), "the main agent declares nothing");
    let declared = eos_agent_core::DeclaredMode {
        agent_type: "planner".into(),
        definition: Some(eos_agent_core::PermissionMode::Plan),
        spawn: None,
    };
    assert_eq!(asked[1], ("Write".to_owned(), vec![declared]));
    assert!(dirs.project().join("plan.md").exists(), "the gate applies modes; it allowed the write");
}

#[tokio::test]
async fn a_gate_that_does_not_apply_modes_still_gets_a_read_only_plan_subagent() {
    let (gate, dirs, sub_saw) = planner_writes(false).await;
    let asked = gate.asked.lock().expect("lock").clone();
    assert_eq!(asked.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["Agent"], "the write never reached it");
    assert!(sub_saw.contains("runs in plan mode"), "{sub_saw}");
    assert!(!dirs.project().join("plan.md").exists());
}
