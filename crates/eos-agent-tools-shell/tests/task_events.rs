//! A host that listens hears a background command's or a monitor's start,
//! output growth and end.

mod common;

use std::sync::{Arc, Mutex};

use common::{Harness, task_id, text, wait_until};
use eos_agent_core::{TaskEvent, TaskEventKind, TaskEvents};
use serde_json::json;

/// A harness whose task events are collected.
fn listening() -> (Harness, Arc<Mutex<Vec<TaskEvent>>>) {
    let h = Harness::new();
    let heard: Arc<Mutex<Vec<TaskEvent>>> = Arc::default();
    let into = Arc::clone(&heard);
    h.ext().insert(TaskEvents::new(move |event| {
        into.lock().expect("lock").push(event);
        Box::pin(async {})
    }));
    (h, heard)
}

async fn until_ended(heard: &Arc<Mutex<Vec<TaskEvent>>>) -> Vec<TaskEvent> {
    wait_until(|| heard.lock().expect("lock").iter().any(|e| matches!(e.kind, TaskEventKind::Ended { .. }))).await;
    heard.lock().expect("lock").clone()
}

#[tokio::test]
async fn a_background_command_reports_its_start_output_and_exit_code() {
    let (h, heard) = listening();
    let command = "echo working; sleep 1.3; echo more; exit 3";
    let out = h.call("Bash", json!({"command": command, "description": "Work", "run_in_background": true})).await;
    let id = task_id(text(&out));
    let file = h.scratch().join("tasks").join(format!("{id}.output")).display().to_string();

    let events = until_ended(&heard).await;
    assert!(events.iter().all(|e| e.task_id == id && e.tool_use_id.as_deref() == Some("toolu_0")));
    let TaskEventKind::Started { task_type, description, command: started, output_file, .. } = &events[0].kind else {
        panic!("first the start: {:?}", events[0]);
    };
    assert_eq!((task_type.as_str(), description.as_str()), ("local_bash", "Work"));
    assert_eq!(started.as_deref(), Some(command));
    assert_eq!(output_file.as_deref(), Some(file.as_str()));
    let sizes: Vec<u64> = events
        .iter()
        .filter_map(|e| match e.kind {
            TaskEventKind::Output { total_bytes } => Some(total_bytes),
            _ => None,
        })
        .collect();
    assert_eq!(sizes.first(), Some(&8), "`working\\n` first");
    assert_eq!(sizes.last(), Some(&13), "then `more\\n`");
    let TaskEventKind::Ended { status, exit_code, summary, .. } = &events.last().expect("an end").kind else {
        panic!("last the end");
    };
    assert_eq!((status.as_str(), *exit_code), ("failed", Some(3)));
    assert_eq!(summary, "Background command \"Work\" failed with exit code 3");
}

#[tokio::test]
async fn a_stopped_command_ends_as_stopped() {
    let (h, heard) = listening();
    let out = h.call("Bash", json!({"command": "sleep 30", "run_in_background": true})).await;
    let id = task_id(text(&out));
    h.call("TaskStop", json!({"task_id": id})).await;
    let events = until_ended(&heard).await;
    let TaskEventKind::Ended { status, exit_code, .. } = &events.last().expect("an end").kind else {
        panic!("last the end");
    };
    assert_eq!((status.as_str(), *exit_code), ("stopped", None));
}

#[tokio::test]
async fn a_monitor_reports_its_stream_and_end() {
    let (h, heard) = listening();
    h.call("Monitor", json!({"command": "echo one; echo two", "description": "feed", "timeout_ms": 30000})).await;
    let events = until_ended(&heard).await;
    let kinds: Vec<String> = events
        .iter()
        .map(|e| match &e.kind {
            TaskEventKind::Started { command, .. } => format!("started {}", command.as_deref().unwrap_or_default()),
            TaskEventKind::Output { total_bytes } => format!("output {total_bytes}"),
            TaskEventKind::Ended { status, exit_code, .. } => format!("ended {status} {exit_code:?}"),
            TaskEventKind::Progress { .. } => "progress".to_owned(),
            TaskEventKind::Event { text } => format!("event {text}"),
        })
        .collect();
    assert_eq!(kinds.first().map(String::as_str), Some("started echo one; echo two"));
    assert_eq!(kinds[kinds.len() - 2], "output 8", "{kinds:?}");
    assert_eq!(kinds.last().map(String::as_str), Some("ended completed Some(0)"));
}

#[tokio::test]
async fn a_monitor_reports_the_lines_it_tells_its_agent() {
    let (h, heard) = listening();
    let script = "echo one; sleep 1; echo two; sleep 1";
    h.call("Monitor", json!({"command": script, "description": "feed", "timeout_ms": 30000})).await;
    let events = until_ended(&heard).await;
    let told: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            TaskEventKind::Event { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(told, ["one", "two"], "each batch as it was delivered: {events:?}");
    assert!(matches!(events.last().map(|e| &e.kind), Some(TaskEventKind::Ended { .. })), "then the end");
}
