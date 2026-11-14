mod common;

use std::path::PathBuf;

use common::{Harness, text};
use eos_agent_tools_shell::ShellSnapshots;
use serde_json::json;

fn monitor_id(result: &str) -> String {
    let at = result.find("(task ").expect("a task id") + 6;
    result[at..].chars().take_while(char::is_ascii_alphanumeric).collect()
}

#[tokio::test]
async fn stdout_lines_become_events() {
    let h = Harness::new();
    let out = h
        .call(
            "Monitor",
            json!({"command": "echo one; echo two; sleep 0.6; echo three", "description": "test feed", "timeout_ms": 30000}),
        )
        .await;
    let id = monitor_id(text(&out));
    assert_eq!(
        text(&out),
        format!(
            "Monitor started (task {id}, timeout 30000ms). You will be notified on each event. Keep working \u{2014} \
             do not poll or sleep. Events may arrive while you are waiting for the user \u{2014} an event is not their \
             reply."
        )
    );
    let notes = h.wait_reminders(2).await;
    assert_eq!(
        notes[0],
        format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<summary>Monitor event: \"test feed\"</summary>\n\
             <event>one\ntwo</event>\n</task-notification>"
        )
    );
    let output_file = h.scratch().join("tasks").join(format!("{id}.output"));
    let last = notes.last().expect("an exit notice");
    assert!(
        last.starts_with(&format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<tool-use-id>toolu_0</tool-use-id>\n<output-file>{}</output-file>\n\
             <status>completed</status>\n<summary>Monitor \"test feed\" stream ended</summary>",
            output_file.display()
        )),
        "{last}"
    );
    assert!(notes.join("\n").contains("<event>three</event>"));
    assert_eq!(std::fs::read_to_string(output_file).expect("output"), "one\ntwo\nthree\n");
}

#[tokio::test]
async fn reports_scripts_that_fail_or_say_nothing() {
    let h = Harness::new();
    h.call("Monitor", json!({"command": "echo oops >&2", "description": "quiet", "timeout_ms": 30000})).await;
    let notes = h.wait_reminders(1).await;
    assert!(notes[0].contains(
        "<status>completed</status>\n<summary>Monitor \"quiet\" ended without producing output (exit 0)</summary>"
    ));

    h.call("Monitor", json!({"command": "exit 4", "description": "broken", "timeout_ms": 30000})).await;
    let notes = h.wait_reminders(1).await;
    assert!(notes[0].contains("<status>failed</status>\n<summary>Monitor \"broken\" script failed (exit 4)</summary>"));
}

#[tokio::test]
async fn times_out_and_can_be_stopped() {
    let h = Harness::new();
    let out = h
        .call(
            "Monitor",
            json!({"command": "while true; do sleep 0.2; done", "description": "spin", "timeout_ms": 1000}),
        )
        .await;
    let id = monitor_id(text(&out));
    let notes = h.wait_reminders(1).await;
    assert_eq!(
        notes,
        vec![format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<summary>Monitor event: \"spin\"</summary>\n\
             <event>[Monitor timed out \u{2014} re-arm if needed.]</event>\n</task-notification>"
        )]
    );

    let out = h
        .call(
            "Monitor",
            json!({"command": "sleep 60", "description": "forever", "timeout_ms": 1000, "persistent": true}),
        )
        .await;
    assert!(text(&out).contains("persistent \u{2014} runs until TaskStop or session end"));
    let id = monitor_id(text(&out));
    let stop = h.call("TaskStop", json!({"task_id": id})).await;
    assert!(!stop.is_error, "{}", text(&stop));
    assert!(text(&stop).contains("Successfully stopped task"));
}

#[tokio::test]
async fn validates_its_input() {
    let h = Harness::new();
    let out = h.call("Monitor", json!({"description": "x", "timeout_ms": 5000})).await;
    assert_eq!(text(&out), "<tool_use_error>InputValidationError: exactly one of command or ws</tool_use_error>");
    let out = h.call("Monitor", json!({"command": "true", "description": "x", "timeout_ms": 10})).await;
    assert!(out.is_error);
    let out = h.call("Monitor", json!({"command": "true", "description": "x", "timeout_ms": 7_200_000})).await;
    assert!(text(&out).contains("timeout_ms must be \u{2264} 3600000"));
}

#[tokio::test]
async fn snapshots_capture_the_shell_once_and_are_deleted_with_their_owner() {
    let dir = std::env::temp_dir().join(format!("eos-snapshots-{}", fastrand::u64(..)));
    let budget = tokio::sync::Semaphore::new(1);
    let snapshots = ShellSnapshots::new(dir.clone());
    let mut made: Vec<PathBuf> = Vec::new();
    for shell in ["/bin/zsh", "/bin/bash"] {
        let path = snapshots.get(shell.as_ref(), &budget).await.expect("snapshot created");
        let content = std::fs::read_to_string(&path).expect("snapshot file");
        assert!(content.contains("unalias -a"), "{content}");
        assert!(content.contains("export PATH="), "{content}");
        assert_eq!(snapshots.get(shell.as_ref(), &budget).await, Some(path.clone()));
        made.push(path);
    }
    drop(snapshots);
    assert!(made.iter().all(|p| !p.exists()));
    let _ = std::fs::remove_dir_all(dir);
}
