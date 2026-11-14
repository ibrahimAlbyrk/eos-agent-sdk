mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Harness, alive, pid_from, task_id, text, wait_until};
use eos_agent_core::{BackgroundTask, BackgroundTasks, CancellationToken, TaskStatus};
use serde_json::json;

#[tokio::test]
async fn background_command_notifies_on_completion() {
    let h = Harness::new();
    let out = h
        .call(
            "Bash",
            json!({"command": "echo working; exit 0", "description": "Say working", "run_in_background": true}),
        )
        .await;
    let id = task_id(text(&out));
    let file = h.scratch().join("tasks").join(format!("{id}.output"));
    assert_eq!(
        text(&out),
        format!(
            "Command running in background with ID: {id}. Output is being written to: {}. You will be notified when \
             it completes \u{2014} do not poll that file or sleep while waiting. Read the file only if the user asks \
             for progress, or if you need a line it prints before you can go on, such as a server's ready line or \
             port.",
            file.display()
        )
    );
    let notes = h.wait_reminders(1).await;
    assert_eq!(
        notes,
        vec![format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<tool-use-id>toolu_0</tool-use-id>\n<output-file>{}</output-file>\n\
             <status>completed</status>\n<summary>Background command \"Say working\" completed (exit code 0)</summary>\n\
             </task-notification>",
            file.display()
        )]
    );
    assert_eq!(std::fs::read_to_string(&file).expect("output file"), "working\n");

    let out = h.call("Bash", json!({"command": "exit 7", "run_in_background": true})).await;
    let notes = h.wait_reminders(1).await;
    assert!(notes[0].contains(&format!("<task-id>{}</task-id>", task_id(text(&out)))));
    assert!(
        notes[0].contains(
            "<status>failed</status>\n<summary>Background command \"exit 7\" failed with exit code 7</summary>"
        )
    );

    h.call("Bash", json!({"command": "grep x /dev/null", "description": "Find x", "run_in_background": true})).await;
    let notes = h.wait_reminders(1).await;
    assert!(
        notes[0].contains("<summary>Background command \"Find x\" completed (exit code 1: No matches found)</summary>")
    );
}

#[tokio::test]
async fn background_deadline_stops_the_command() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"command": "sleep 5", "run_in_background": true, "timeout": 300})).await;
    let id = task_id(text(&out));
    let notes = h.wait_reminders(1).await;
    assert!(notes[0].contains(&format!("<task-id>{id}</task-id>")));
    assert!(notes[0].contains(
        "<status>killed</status>\n<summary>Background command \"sleep 5\" was stopped after reaching its background \
         time limit</summary>\n<note>If the work in progress still needs it, start it again with `run_in_background`"
    ));
}

#[tokio::test]
async fn foreground_timeout_moves_the_command_to_the_background() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"command": "echo started; sleep 1.5; echo finished", "timeout": 400})).await;
    let body = text(&out);
    assert!(!out.is_error);
    let id = task_id(body);
    let file = h.scratch().join("tasks").join(format!("{id}.output"));
    assert_eq!(
        body,
        format!(
            "started\nCommand did not complete within its 1s timeout and was moved to the background (ID: {id}). \
             Output is being written to: {}. You will be notified when it completes. If it is still running after \
             10m in the background, it will be stopped and you will be notified. It is running now: do not start a \
             second copy while it runs. If it is no longer needed, stop it with TaskStop. To check interim output, \
             use Read on that file path.",
            file.display()
        )
    );
    let notes = h.wait_reminders(1).await;
    assert!(notes[0].contains("<status>completed</status>"), "{}", notes[0]);
    assert!(notes[0].contains("<tool-use-id>toolu_0</tool-use-id>"));
    assert_eq!(std::fs::read_to_string(&file).expect("output"), "started\nfinished\n");
    // The cwd file of the moved command is cleaned up.
    let leftovers: Vec<_> = std::fs::read_dir(h.scratch().join("tasks"))
        .expect("tasks dir")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "cwd"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn task_stop_kills_the_whole_group() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"command": "sleep 30 & echo $!; wait", "run_in_background": true})).await;
    let id = task_id(text(&out));
    let sleeper = pid_from(&h.scratch().join("tasks").join(format!("{id}.output"))).await;
    assert!(alive(sleeper));

    let out = h.call("TaskStop", json!({"task_id": id})).await;
    assert!(!out.is_error);
    assert_eq!(
        text(&out),
        format!(
            "{{\"message\":\"Successfully stopped task: {id} (sleep 30 & echo $!; wait)\",\"task_id\":\"{id}\",\
             \"task_type\":\"local_bash\",\"command\":\"sleep 30 & echo $!; wait\"}}"
        )
    );
    wait_until(|| !alive(sleeper)).await;
    // Stopping is reported by TaskStop itself, not by a notification.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.reminders().is_empty());

    let again = h.call("TaskStop", json!({"shell_id": id})).await;
    assert_eq!(text(&again), format!("<tool_use_error>Task {id} is not running (status: killed)</tool_use_error>"));
    let unknown = h.call("TaskStop", json!({"task_id": "nope"})).await;
    assert_eq!(text(&unknown), "<tool_use_error>No task found with ID: nope</tool_use_error>");
    let missing = h.call("TaskStop", json!({})).await;
    assert_eq!(text(&missing), "<tool_use_error>Missing required parameter: task_id</tool_use_error>");
}

#[tokio::test]
async fn task_stop_stops_other_background_tasks() {
    let h = Harness::new();
    let task = Arc::new(BackgroundTask::new("w0a1b2c3d", "local_workflow", "Review changed files"));
    h.ext().get_or_default::<BackgroundTasks>().insert(Arc::clone(&task));
    let runner = Arc::clone(&task);
    tokio::spawn(async move {
        runner.stop_requested().cancelled().await;
        runner.finish(TaskStatus::Killed);
    });
    let out = h.call("TaskStop", json!({"task_id": "w0a1b2c3d"})).await;
    assert_eq!(
        text(&out),
        "{\"message\":\"Successfully stopped task: w0a1b2c3d (Review changed files)\",\"task_id\":\"w0a1b2c3d\",\
         \"task_type\":\"local_workflow\",\"command\":\"Review changed files\"}"
    );
    let again = h.call("TaskStop", json!({"task_id": "w0a1b2c3d"})).await;
    assert_eq!(text(&again), "<tool_use_error>Task w0a1b2c3d is not running (status: killed)</tool_use_error>");
}

#[tokio::test]
async fn dropping_the_agent_kills_its_tasks() {
    let mut h = Harness::new();
    let out = h.call("Bash", json!({"command": "echo $$; sleep 60", "run_in_background": true})).await;
    let shell = pid_from(&h.scratch().join("tasks").join(format!("{}.output", task_id(text(&out))))).await;
    let out = h.call("Monitor", json!({"command": "echo $$; sleep 60", "description": "idle"})).await;
    let monitor = pid_from(&h.scratch().join("tasks").join(format!("{}.output", task_id_monitor(text(&out))))).await;
    assert!(alive(shell) && alive(monitor));

    drop(h.ext.take());
    wait_until(|| !alive(shell) && !alive(monitor)).await;
}

#[tokio::test]
async fn an_interrupted_call_kills_its_command() {
    let h = Harness::new();
    let cancel = CancellationToken::new();
    let call = h.call_with("Bash", json!({"command": "echo $$; sleep 30"}), &cancel);
    let tasks = h.scratch().join("tasks");
    let pid = tokio::select! {
        _ = call => panic!("the command should still be running"),
        pid = async {
            let mut file = None;
            wait_until(|| {
                file = std::fs::read_dir(&tasks).ok().and_then(|d| {
                    d.filter_map(Result::ok).map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "output"))
                });
                file.is_some()
            })
            .await;
            pid_from(&file.unwrap_or_default()).await
        } => pid,
    };
    // The call future was dropped above, as the agent loop drops it on interrupt.
    wait_until(|| !alive(pid)).await;
}

fn task_id_monitor(result: &str) -> String {
    let at = result.find("(task ").expect("a task id") + 6;
    result[at..].chars().take_while(char::is_ascii_alphanumeric).collect()
}

/// TaskStop says which handles it takes and what a subagent may stop, and
/// names nothing that cannot exist here; the SDK's TaskStop tests hold the
/// behaviour to it.
#[test]
fn task_stop_describes_the_handles_and_scope_it_has() {
    let h = Harness::new();
    let spec = String::from_utf8_lossy(h.tool("TaskStop").spec().json()).into_owned();
    assert!(!spec.to_lowercase().contains("team"), "{spec}");
    assert!(spec.contains("pass its agent ID or the name it was spawned with as task_id"), "{spec}");
    assert!(spec.contains("A subagent can stop only its own tasks"), "{spec}");
    assert!(spec.contains("accepted by agent ID or by the name they were spawned with"), "{spec}");
}
