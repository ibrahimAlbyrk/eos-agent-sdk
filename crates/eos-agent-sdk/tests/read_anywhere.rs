//! Reads go anywhere the process can read, decided by the host's permission
//! gate; an agent isolated in its own worktree writes only inside it.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use common::{Dirs, agent, all_text, builder, mock, settle};
use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall};
use eos_agent_mock_api::Reply;
use serde_json::{Value, json};

/// Denies reads below `denied`; notes the paths it was shown.
struct DenyBelow {
    denied: PathBuf,
    seen: Mutex<Vec<String>>,
}

impl PermissionGate for DenyBelow {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let input: Value = serde_json::from_str(call.input.get()).unwrap_or_default();
        let path = input["file_path"].as_str().unwrap_or_default().to_owned();
        self.seen.lock().expect("lock").push(path.clone());
        let answer = if call.name == "Read" && Path::new(&path).starts_with(&self.denied) {
            Permission::Deny(format!("Permission to read {path} has been denied."))
        } else {
            Permission::Allow
        };
        Box::pin(std::future::ready(answer))
    }
}

#[tokio::test]
async fn reads_outside_the_roots_are_the_gates_to_decide() {
    let dirs = Dirs::new("read-anywhere-gate");
    dirs.write("elsewhere/open.txt", "far away");
    dirs.write("elsewhere/secret/key.txt", "do not show");
    let mock = mock([
        Reply::tool_use("toolu_s", "Read", r#"{"file_path":"../elsewhere/secret/key.txt"}"#),
        Reply::tool_use("toolu_o", "Read", r#"{"file_path":"../elsewhere/open.txt"}"#),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(DenyBelow { denied: dirs.root.join("elsewhere/secret"), seen: Mutex::default() });
    let sdk =
        builder(&mock, &dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("read them").await.expect("alive").expect("turn");

    let secret = dirs.root.join("elsewhere/secret/key.txt").display().to_string();
    let open = dirs.root.join("elsewhere/open.txt").display().to_string();
    assert_eq!(*gate.seen.lock().expect("lock"), [secret.clone(), open], "the gate sees absolute paths");
    let requests = mock.requests();
    let after_secret = all_text(&requests[1]);
    assert!(after_secret.contains(&format!("Permission to read {secret} has been denied.")), "{after_secret}");
    assert!(!after_secret.contains("do not show"));
    let after_open = all_text(&requests[2]);
    assert!(after_open.contains("far away"), "{after_open}");
    assert!(after_open.contains("../elsewhere/open.txt"), "the conversation keeps the call as the model wrote it");
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git")
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// A subagent in its own worktree reads outside it (the parent's project,
/// another folder) but writes only inside it.
#[tokio::test]
async fn a_worktree_subagent_reads_outside_its_roots_but_writes_only_inside() {
    let dirs = Dirs::new("read-anywhere-worktree");
    dirs.write("project/notes.md", "parent notes");
    dirs.write("elsewhere/open.txt", "far away");
    git(&dirs.project(), &["init", "-q"]);
    git(&dirs.project(), &["add", "."]);
    git(&dirs.project(), &["commit", "-q", "-m", "init"]);
    let notes = dirs.project().join("notes.md");
    let open = dirs.root.join("elsewhere/open.txt");
    let blocked = dirs.project().join("from-subagent.txt");
    let call = json!({"description": "Look", "prompt": "Read and write", "isolation": "worktree",
        "run_in_background": false});
    let mock = mock([
        Reply::tool_use("toolu_a", "Agent", &call.to_string()),
        Reply::tool_use("toolu_n", "Read", &json!({ "file_path": notes }).to_string()),
        Reply::tool_use("toolu_o", "Read", &json!({ "file_path": open }).to_string()),
        Reply::tool_use("toolu_w", "Write", &json!({ "file_path": blocked, "content": "x" }).to_string()),
        Reply::text("sub done"),
        Reply::text("done"),
    ])
    .await;
    let sdk = builder(&mock, &dirs).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("delegate").await.expect("alive").expect("turn");
    settle(&agent).await;

    let requests = mock.requests();
    let sub = all_text(&requests[4]);
    assert!(sub.contains("parent notes") && sub.contains("far away"), "{sub}");
    let refusal = format!("{} is outside this agent's working directories.", blocked.display());
    assert!(sub.contains(&refusal), "{sub}");
    assert!(!blocked.exists());
}
