//! The Workflow definition against what Claude Code 2.1.319 sent (the capture
//! in `fixtures/captured`, kept out of git: skipped without it).

use std::sync::Arc;

use eos_agent_core::{BoxFuture, SubagentInfo, SubagentRequest, SubagentStart, Subagents, Tool};
use eos_agent_workflow::{WorkflowConfig, WorkflowTool};
use serde_json::Value;

struct Nobody;

impl Subagents for Nobody {
    fn start(&self, _: SubagentRequest) -> BoxFuture<'_, Result<SubagentStart, String>> {
        Box::pin(async { Err("none".into()) })
    }
    fn send<'a>(&'a self, _: &'a str, _: &'a str, _: String) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn list<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Vec<SubagentInfo>> {
        Box::pin(async { Vec::new() })
    }
}

#[test]
fn definition_matches_claude_code_byte_for_byte() {
    let capture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/captured/0003-v1_messages/request.json");
    let Ok(text) = std::fs::read_to_string(capture) else {
        eprintln!("no capture at {capture}; skipped");
        return;
    };
    let request: Value = serde_json::from_str(&text).expect("capture json");
    let theirs = request["body"]["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["name"] == "Workflow"))
        .cloned()
        .expect("Claude Code sent Workflow");
    let tool = WorkflowTool::new(WorkflowConfig::new(std::env::temp_dir()), Arc::new(Nobody));
    let ours: Value = serde_json::from_slice(tool.spec().json()).expect("our json");
    assert_eq!(ours["name"], theirs["name"]);
    assert_eq!(ours["description"], theirs["description"], "description");
    assert_eq!(ours["input_schema"], theirs["input_schema"], "input schema");
    // Not deferred: Claude Code sends it in full.
    assert!(theirs.get("defer_loading").is_none() && ours.get("defer_loading").is_none());
}
