//! Writes outside the working directories are the permission gate's to
//! decide, with the absolute path and whether it is inside; an agent
//! isolated in its own worktree, and the plain subagents it starts, still
//! write only inside it; the engine's own files stay unwritten, except the
//! agent's own scratch folder.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use common::{Dirs, agent, all_text, builder, mock, settle};
use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall, WriteTarget};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::AgentOptions;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
struct Seen {
    tool: String,
    input: Value,
    writes: Vec<WriteTarget>,
}

type Ask = (PathBuf, oneshot::Sender<Permission>);

/// Notes every call; denies writes below `denied`; with `asks`, a Write
/// waits for an answer from the test, as a host asking its user does.
#[derive(Default)]
struct Gate {
    seen: Mutex<Vec<Seen>>,
    denied: Option<PathBuf>,
    asks: Option<mpsc::UnboundedSender<Ask>>,
}

impl Gate {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().expect("lock").clone()
    }

    fn writes_of(&self, tool: &str) -> Vec<WriteTarget> {
        self.seen().into_iter().filter(|s| s.tool == tool).flat_map(|s| s.writes).collect()
    }
}

impl PermissionGate for Gate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let input: Value = serde_json::from_str(call.input.get()).unwrap_or_default();
        self.seen.lock().expect("lock").push(Seen { tool: call.name.to_owned(), input, writes: call.writes.to_vec() });
        let denied = self.denied.as_ref().is_some_and(|d| call.writes.iter().any(|w| w.path.starts_with(d)));
        if denied {
            let path = call.writes[0].path.display().to_string();
            return Box::pin(std::future::ready(Permission::Deny(format!("Permission to write {path} was denied."))));
        }
        if let (Some(asks), "Write", Some(target)) = (&self.asks, call.name, call.writes.first()) {
            let (answer, answered) = oneshot::channel();
            let _ = asks.send((target.path.clone(), answer));
            return Box::pin(async move { answered.await.unwrap_or(Permission::Deny("no answer".into())) });
        }
        Box::pin(std::future::ready(Permission::Allow))
    }
}

async fn run(dirs: &Dirs, replies: Vec<Reply>, gate: Arc<Gate>) -> MockApi {
    let mock = mock(replies).await;
    let sdk = builder(&mock, dirs).permissions(gate as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, dirs).await;
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    mock
}

fn call(id: &str, tool: &str, input: &Value) -> Reply {
    Reply::tool_use(id, tool, &input.to_string())
}

const NOTEBOOK: &str = r#"{"cells":[{"cell_type":"code","id":"c1","metadata":{},"source":["x = 1"],"outputs":[],"execution_count":null}],"metadata":{},"nbformat":4,"nbformat_minor":5}"#;

fn target(path: &Path, inside: bool) -> WriteTarget {
    WriteTarget { path: path.to_owned(), inside }
}

#[tokio::test]
async fn a_top_level_agent_writes_edits_and_notebook_edits_outside_its_roots_when_the_gate_allows() {
    let dirs = Dirs::new("write-anywhere-allowed");
    dirs.write("elsewhere/notes.txt", "far away\n");
    dirs.write("elsewhere/n.ipynb", NOTEBOOK);
    dirs.write("elsewhere/unread.txt", "untouched\n");
    let notes = dirs.root.join("elsewhere/notes.txt");
    let nb = dirs.root.join("elsewhere/n.ipynb");
    let unread = dirs.root.join("elsewhere/unread.txt");
    let created = dirs.root.join("elsewhere/new.txt");
    let inside = dirs.project().join("a.txt");
    let gate = Arc::new(Gate::default());
    let mock = run(
        &dirs,
        vec![
            call("toolu_r1", "Read", &json!({ "file_path": notes })),
            call("toolu_e", "Edit", &json!({ "file_path": notes, "old_string": "far", "new_string": "near" })),
            call("toolu_w", "Write", &json!({ "file_path": "../elsewhere/new.txt", "content": "made" })),
            call("toolu_r2", "Read", &json!({ "file_path": nb })),
            call(
                "toolu_n",
                "NotebookEdit",
                &json!({ "notebook_path": nb, "new_source": "y = 2", "edit_mode": "insert", "cell_type": "code" }),
            ),
            call("toolu_u", "Edit", &json!({ "file_path": unread, "old_string": "un", "new_string": "" })),
            call("toolu_i", "Write", &json!({ "file_path": "a.txt", "content": "in" })),
            Reply::text("done"),
        ],
        Arc::clone(&gate),
    )
    .await;

    assert_eq!(std::fs::read_to_string(&notes).expect("notes"), "near away\n");
    assert_eq!(std::fs::read_to_string(&created).expect("created"), "made");
    assert!(std::fs::read_to_string(&nb).expect("nb").contains("y = 2"));
    assert_eq!(std::fs::read_to_string(&inside).expect("inside"), "in");
    // Read-before-write holds outside the roots as inside them.
    assert_eq!(std::fs::read_to_string(&unread).expect("unread"), "untouched\n");
    let after_unread = all_text(&mock.requests()[6]);
    assert!(after_unread.contains("File has not been read yet."), "{after_unread}");

    assert_eq!(gate.writes_of("Edit"), [target(&notes, false), target(&unread, false)]);
    assert_eq!(gate.writes_of("NotebookEdit"), [target(&nb, false)]);
    assert_eq!(gate.writes_of("Write"), [target(&created, false), target(&inside, true)]);
    assert_eq!(gate.writes_of("Read"), [], "a read writes nothing");
    let shown: Vec<Value> = gate.seen().into_iter().filter(|s| s.tool == "Write").map(|s| s.input).collect();
    assert_eq!(shown[0]["file_path"], json!(created), "the gate sees the relative path made absolute");
    assert_eq!(shown[1]["file_path"], json!(inside));
}

#[tokio::test]
async fn a_tilde_path_reaches_the_gate_absolute() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    let dirs = Dirs::new("write-anywhere-tilde");
    let gate = Arc::new(Gate { denied: Some(home.clone()), ..Gate::default() });
    run(
        &dirs,
        vec![
            call("toolu_w", "Write", &json!({ "file_path": "~/eos-write-anywhere-never.txt", "content": "x" })),
            Reply::text("done"),
        ],
        Arc::clone(&gate),
    )
    .await;
    let wanted = home.join("eos-write-anywhere-never.txt");
    assert_eq!(gate.writes_of("Write"), [target(&wanted, false)]);
    assert!(!wanted.exists(), "the gate denied it");
}

#[tokio::test]
async fn a_deny_from_the_gate_stops_a_write_outside() {
    let dirs = Dirs::new("write-anywhere-denied");
    std::fs::create_dir_all(dirs.root.join("elsewhere/locked")).expect("dir");
    let locked = dirs.root.join("elsewhere/locked/x.txt");
    let gate = Arc::new(Gate { denied: Some(dirs.root.join("elsewhere/locked")), ..Gate::default() });
    let mock = run(
        &dirs,
        vec![call("toolu_w", "Write", &json!({ "file_path": locked, "content": "x" })), Reply::text("done")],
        Arc::clone(&gate),
    )
    .await;
    assert!(!locked.exists());
    let after = all_text(&mock.requests()[1]);
    assert!(after.contains(&format!("Permission to write {} was denied.", locked.display())), "{after}");
}

#[tokio::test]
async fn a_write_outside_waits_for_the_gates_ask() {
    let dirs = Dirs::new("write-anywhere-ask");
    std::fs::create_dir_all(dirs.root.join("elsewhere")).expect("dir");
    let file = dirs.root.join("elsewhere/asked.txt");
    let (asks, mut asked) = mpsc::unbounded_channel();
    let gate = Arc::new(Gate { asks: Some(asks), ..Gate::default() });
    let mock =
        mock([call("toolu_w", "Write", &json!({ "file_path": file, "content": "x" })), Reply::text("done")]).await;
    let sdk =
        builder(&mock, &dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    let user = {
        let file = file.clone();
        tokio::spawn(async move {
            let (path, answer) = asked.recv().await.expect("asked");
            assert_eq!(path, file);
            assert!(!file.exists(), "nothing is written while the question is open");
            let _ = answer.send(Permission::Allow);
        })
    };
    agent.send_text("go").await.expect("alive").expect("turn");
    user.await.expect("answered");
    assert_eq!(std::fs::read_to_string(&file).expect("written"), "x");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_out_of_the_roots_is_judged_by_its_target() {
    let dirs = Dirs::new("write-anywhere-link");
    std::fs::create_dir_all(dirs.root.join("elsewhere")).expect("dir");
    std::os::unix::fs::symlink(dirs.root.join("elsewhere"), dirs.project().join("out")).expect("link");
    let through = dirs.project().join("out/x.txt");
    let gate = Arc::new(Gate::default());
    run(
        &dirs,
        vec![call("toolu_w", "Write", &json!({ "file_path": "out/x.txt", "content": "x" })), Reply::text("done")],
        Arc::clone(&gate),
    )
    .await;
    assert_eq!(gate.writes_of("Write"), [target(&through, false)], "the link leads outside");
    assert_eq!(std::fs::read_to_string(dirs.root.join("elsewhere/x.txt")).expect("written"), "x");
}

#[tokio::test]
async fn the_engines_own_files_stay_unwritten_whatever_the_gate_says() {
    let dirs = Dirs::new("write-anywhere-protected");
    let transcript = dirs.root.join("data/some-session.jsonl");
    let gate = Arc::new(Gate::default());
    let mock = run(
        &dirs,
        vec![call("toolu_w", "Write", &json!({ "file_path": transcript, "content": "x" })), Reply::text("done")],
        Arc::clone(&gate),
    )
    .await;
    assert!(!transcript.exists());
    let after = all_text(&mock.requests()[1]);
    assert!(after.contains("is in the agent engine's own files, which tools do not write."), "{after}");
    assert!(gate.writes_of("Write").is_empty(), "refused before the gate is asked");
}

#[tokio::test]
async fn an_agent_writes_its_own_scratch_and_no_other() {
    let dirs = Dirs::new("write-anywhere-scratch");
    let own = dirs.root.join("data/scratch/a1/artifacts/page.html");
    let other = dirs.root.join("data/scratch/a2/page.html");
    let gate = Arc::new(Gate::default());
    let mock = mock(vec![
        call("toolu_own", "Write", &json!({ "file_path": own, "content": "x" })),
        call("toolu_other", "Write", &json!({ "file_path": other, "content": "x" })),
        Reply::text("done"),
    ])
    .await;
    let sdk =
        builder(&mock, &dirs).permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>).build().await.expect("sdk");
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).id("a1").mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    assert_eq!(std::fs::read_to_string(&own).expect("written"), "x");
    assert_eq!(gate.writes_of("Write"), [target(&own, true)], "its own scratch is inside");
    assert!(!other.exists());
    assert!(all_text(&mock.requests()[2]).contains("is in the agent engine's own files"));
}

#[tokio::test]
async fn a_plain_subagent_follows_its_parent() {
    let dirs = Dirs::new("write-anywhere-plain-sub");
    std::fs::create_dir_all(dirs.root.join("elsewhere")).expect("dir");
    let file = dirs.root.join("elsewhere/from-sub.txt");
    let gate = Arc::new(Gate::default());
    let agent_call = json!({"description": "Write", "prompt": "Write it", "run_in_background": false});
    run(
        &dirs,
        vec![
            call("toolu_a", "Agent", &agent_call),
            call("toolu_w", "Write", &json!({ "file_path": file, "content": "sub" })),
            Reply::text("sub done"),
            Reply::text("done"),
        ],
        Arc::clone(&gate),
    )
    .await;
    assert_eq!(std::fs::read_to_string(&file).expect("written"), "sub");
    assert_eq!(gate.writes_of("Write"), [target(&file, false)]);
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

fn repo(dirs: &Dirs) {
    dirs.write("project/notes.md", "parent notes");
    git(&dirs.project(), &["init", "-q"]);
    git(&dirs.project(), &["add", "."]);
    git(&dirs.project(), &["commit", "-q", "-m", "init"]);
}

fn refusal(path: &Path) -> String {
    format!("{} is outside this agent's working directories.", path.display())
}

/// An isolated subagent, and a plain one it starts, write only inside the
/// worktree; the refusal comes before the gate is asked.
#[tokio::test]
async fn an_isolated_subagent_and_its_plain_subagents_stay_inside_their_worktree() {
    let dirs = Dirs::new("write-anywhere-isolated");
    repo(&dirs);
    let blocked = dirs.project().join("from-isolated.txt");
    let nested = dirs.project().join("from-nested.txt");
    let isolated =
        json!({"description": "Work", "prompt": "Work apart", "isolation": "worktree", "run_in_background": false});
    let plain = json!({"description": "Help", "prompt": "Help out", "run_in_background": false});
    let gate = Arc::new(Gate::default());
    let mock = run(
        &dirs,
        vec![
            call("toolu_a", "Agent", &isolated),
            call("toolu_w", "Write", &json!({ "file_path": blocked, "content": "x" })),
            call("toolu_p", "Agent", &plain),
            call("toolu_n", "Write", &json!({ "file_path": nested, "content": "x" })),
            Reply::text("nested done"),
            Reply::text("sub done"),
            Reply::text("done"),
        ],
        Arc::clone(&gate),
    )
    .await;
    let requests = mock.requests();
    let sub = all_text(&requests[2]);
    assert!(sub.contains(&refusal(&blocked)), "{sub}");
    let inner = all_text(&requests[4]);
    assert!(inner.contains(&refusal(&nested)), "{inner}");
    assert!(!blocked.exists() && !nested.exists());
    assert!(gate.writes_of("Write").is_empty(), "the gate is not asked about a write that cannot happen");
}

/// A workflow's agent with `isolation: 'worktree'` writes only inside its
/// worktree; one without follows the session.
#[tokio::test]
async fn a_workflow_agent_with_isolation_stays_inside_its_worktree() {
    let dirs = Dirs::new("write-anywhere-workflow");
    repo(&dirs);
    std::fs::create_dir_all(dirs.root.join("elsewhere")).expect("dir");
    let isolated = dirs.root.join("elsewhere/isolated.txt");
    let shared = dirs.root.join("elsewhere/shared.txt");
    let script = "export const meta = {name: 'apart', description: 'Apart', phases: [{title: 'Go'}]}\n\
        await agent('Write isolated', {label: 'apart', phase: 'Go', isolation: 'worktree'})\n\
        await agent('Write shared', {label: 'together', phase: 'Go'})\n\
        return 'ok'";
    let (iso, sha) = (isolated.clone(), shared.clone());
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&bodies);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |body| {
        let body: Value = serde_json::from_slice(body).unwrap_or_default();
        log.lock().expect("lock").push(body["messages"].to_string());
        let system: String =
            body["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect();
        let text = body["messages"].to_string();
        if system.contains("spawned by a workflow orchestration script") {
            if text.contains("tool_result") {
                return Reply::text("written");
            }
            let path = if text.contains("Write isolated") { &iso } else { &sha };
            return Reply::tool_use("toolu_w", "Write", &json!({ "file_path": path, "content": "x" }).to_string());
        }
        if text.contains("<task-notification>") {
            return Reply::text("all done");
        }
        if text.contains("Workflow launched in background") {
            return Reply::text("started it");
        }
        Reply::tool_use("toolu_wf", "Workflow", &json!({ "script": script }).to_string())
    })
    .await
    .expect("mock");
    let gate = Arc::new(Gate::default());
    let sdk = builder(&mock, &dirs)
        .workflows(true)
        .permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>)
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("run the workflow").await.expect("alive").expect("turn");
    settle(&agent).await;

    assert!(!isolated.exists(), "the isolated agent cannot write outside its worktree");
    assert_eq!(std::fs::read_to_string(&shared).expect("shared"), "x");
    assert_eq!(gate.writes_of("Write"), [target(&shared, false)]);
    let refused = serde_json::to_string(&refusal(&isolated)).expect("json");
    let refused = refused.trim_matches('"');
    assert!(bodies.lock().expect("lock").iter().any(|b| b.contains(refused)));
}

/// An isolated background subagent stopped with TaskStop, let go, and
/// started again from its transcript by a message is still confined to its
/// worktree.
#[tokio::test]
async fn an_isolated_subagent_stays_confined_after_it_is_stopped_and_resumed() {
    let dirs = Dirs::new("write-anywhere-stopped");
    repo(&dirs);
    let blocked = dirs.project().join("from-resumed.txt");
    let path = blocked.clone();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&bodies);
    let mock = MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), move |body| {
        let body: Value = serde_json::from_slice(body).unwrap_or_default();
        let text = body["messages"].to_string();
        log.lock().expect("lock").push(text.clone());
        if !text.contains("PARENT-START") {
            if text.contains("toolu_w") {
                return Reply::text("resumed done");
            }
            if text.contains("RESUME-NOTE") {
                return Reply::tool_use("toolu_w", "Write", &json!({ "file_path": path, "content": "x" }).to_string());
            }
            return Reply::text("working").slowed(std::time::Duration::from_secs(30));
        }
        let results = text.matches("\"tool_result\"").count();
        let input = match results {
            0 => json!({"description": "Work apart", "prompt": "CHILD-START", "name": "apart",
                        "isolation": "worktree", "run_in_background": true}),
            1 => {
                return call(
                    "toolu_s",
                    "ToolSearch",
                    &json!({"query": "select:TaskStop,SendMessage", "max_results": 5}),
                );
            }
            2 => return call("toolu_stop", "TaskStop", &json!({ "task_id": "apart" })),
            // Late enough that the stopped subagent has been let go.
            3 => {
                let send = json!({ "to": "apart", "message": "RESUME-NOTE" });
                return call("toolu_m", "SendMessage", &send).slowed(std::time::Duration::from_millis(400));
            }
            _ => return Reply::text("parent done"),
        };
        call("toolu_a", "Agent", &input)
    })
    .await
    .expect("mock");
    let gate = Arc::new(Gate::default());
    let sdk = builder(&mock, &dirs)
        .store(Arc::new(eos_agent_engine::JsonlStore::new(dirs.root.join("transcripts"))))
        .idle_eviction(Some(std::time::Duration::from_millis(50)))
        .permissions(Arc::clone(&gate) as Arc<dyn PermissionGate>)
        .build()
        .await
        .expect("sdk");
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("PARENT-START").await.expect("alive").expect("turn");
    settle(&agent).await;
    let refused = serde_json::to_string(&refusal(&blocked)).expect("json");
    let refused = refused.trim_matches('"').to_owned();
    let mut seen = false;
    for _ in 0..200 {
        if bodies.lock().expect("lock").iter().any(|b| b.contains(&refused)) {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(seen, "the resumed subagent's write outside was refused");
    assert!(!blocked.exists());
    assert!(gate.writes_of("Write").is_empty(), "the gate is not asked about a write that cannot happen");
}
