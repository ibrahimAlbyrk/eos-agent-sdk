mod common;

use std::sync::{Arc, Mutex};

use common::{Dirs, mock, settle};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{BoxFuture, Credential, Permission, PermissionGate, StaticCredential, ToolCall};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, ConfineReport, ConfineSpec, Confined, ReadOnly, SandboxConfig, Sdk, Violation};
use eos_agent_tools_shell::sandbox::Seatbelt;
use serde_json::{Value, json};

/// Allows everything and remembers each call it was asked about.
#[derive(Default)]
struct Recorder {
    asked: Mutex<Vec<(String, bool, Value)>>,
    cwds: Mutex<Vec<Option<std::path::PathBuf>>>,
}

impl PermissionGate for Recorder {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let input = serde_json::from_str(call.input.get()).unwrap_or(Value::Null);
        self.asked.lock().expect("lock").push((call.name.to_owned(), call.sandboxed, input));
        self.cwds.lock().expect("lock").push(call.workspace.map(|w| w.cwd()));
        Box::pin(async { Permission::Allow })
    }
}

async fn sdk(mock: &MockApi, dirs: &Dirs, gate: Arc<dyn PermissionGate>, sandbox: Option<Value>) -> Sdk {
    let mut builder = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .permissions(gate);
    if let Some(config) = sandbox {
        builder = builder.sandbox(serde_json::from_value::<SandboxConfig>(config).expect("config"));
    }
    builder.build().await.expect("sdk")
}

fn bash(id: &str, input: Value) -> Reply {
    Reply::tool_use(id, "Bash", &input.to_string())
}

/// The text of the request's system-role message.
fn system_note(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "system")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn sandboxed_commands_skip_the_gate_others_are_asked() {
    if !Seatbelt::available() {
        eprintln!("skipped: sandbox-exec is not available here");
        return;
    }
    let dirs = Dirs::new("sandbox-gate");
    // Turned on in Claude Code's own user settings, as Claude Code reads them.
    dirs.write("home/.claude/settings.json", r#"{"sandbox": {"enabled": true}}"#);
    let mock = mock([
        bash("t1", json!({"command": "echo hi > made.txt"})),
        bash("t2", json!({"command": "echo out > ../outside.txt", "dangerouslyDisableSandbox": true})),
        bash("t3", json!({"command": "rm -rf ."})),
        bash("t4", json!({"command": "echo x > ../blocked.txt"})),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(Recorder::default());
    let sdk = sdk(&mock, &dirs, Arc::clone(&gate) as Arc<dyn PermissionGate>, None).await;
    let (agent, _events) = sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;

    let asked: Vec<(String, bool, String)> = gate
        .asked
        .lock()
        .expect("lock")
        .iter()
        .map(|(n, s, i)| (n.clone(), *s, i["command"].as_str().unwrap_or("").to_owned()))
        .collect();
    assert_eq!(
        asked,
        [
            ("Bash".to_owned(), false, "echo out > ../outside.txt".to_owned()),
            ("Bash".to_owned(), true, "rm -rf .".to_owned()),
        ],
        "a sandboxed command is allowed without asking; unsandboxed and exception commands are asked"
    );
    assert!(dirs.project().join("made.txt").exists());
    assert!(dirs.root.join("outside.txt").exists(), "the unsandboxed retry ran outside");
    assert!(!dirs.root.join("blocked.txt").exists());

    let requests = mock.requests();
    let note = system_note(&requests[0].json());
    assert!(note.starts_with("## Bash command sandbox\nBy default, Bash commands run inside an OS-level sandbox"));
    assert!(note.contains("\n\n# Environment\n"), "the section leads the environment");
    let last = requests.last().expect("requests").json().to_string();
    assert!(last.contains("<sandbox_violations>"), "the denied write is reported");
    let tools = requests[0].json()["tools"].clone();
    let bash = tools.as_array().expect("tools").iter().find(|t| t["name"] == "Bash").expect("Bash").clone();
    assert!(bash["input_schema"]["properties"]["allowed_domains"].is_object());
}

#[tokio::test]
async fn read_only_sessions_still_ask_and_off_changes_nothing() {
    if !Seatbelt::available() {
        return;
    }
    let dirs = Dirs::new("sandbox-readonly");
    let api = mock([bash("t1", json!({"command": "echo hi > made.txt"})), Reply::text("done")]).await;
    let built = sdk(&api, &dirs, Arc::new(ReadOnly), Some(json!({"enabled": true}))).await;
    let (agent, _events) = built.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    assert!(!dirs.project().join("made.txt").exists(), "read-only is not loosened by the sandbox");

    let dirs = Dirs::new("sandbox-off");
    let api = mock([Reply::text("done")]).await;
    let built = sdk(&api, &dirs, Arc::new(Recorder::default()), None).await;
    let (agent, _events) = built.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    let body = api.requests()[0].json();
    assert!(system_note(&body).starts_with("# Environment\n"), "no sandbox section while it is off");
    let bash = body["tools"].as_array().expect("tools").iter().find(|t| t["name"] == "Bash").expect("Bash").clone();
    assert!(bash["input_schema"]["properties"].get("allowed_domains").is_none());
}

/// A local HTTP server answering `hello`; returns its port.
async fn upstream() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut seen = Vec::new();
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello").await;
            });
        }
    });
    port
}

#[tokio::test]
async fn each_directory_has_its_own_sandbox_settings() {
    if !Seatbelt::available() {
        return;
    }
    let port = upstream().await;
    let dirs = Dirs::new("sandbox-projects");
    // A: the project's shared settings turn it on and list the host; its
    // local settings refuse unsandboxed commands. B: no settings. C: on,
    // nothing listed.
    dirs.write(
        "a/.claude/settings.json",
        &json!({"sandbox": {"enabled": true, "network": {"allowedDomains": [format!("127.0.0.1:{port}")]}}})
            .to_string(),
    );
    dirs.write("a/.claude/settings.local.json", r#"{"sandbox": {"allowUnsandboxedCommands": false}}"#);
    dirs.write("b/readme.txt", "b");
    dirs.write("c/.claude/settings.json", r#"{"sandbox": {"enabled": true}}"#);
    let curl = format!("curl -sS --max-time 10 --noproxy '' http://127.0.0.1:{port}/");
    let api = mock([
        bash("a1", json!({"command": curl})),
        Reply::text("a done"),
        bash("b1", json!({"command": "echo b > made.txt"})),
        Reply::text("b done"),
        bash("c1", json!({"command": curl})),
        Reply::text("c done"),
    ])
    .await;
    let gate = Arc::new(Recorder::default());
    let built = sdk(&api, &dirs, Arc::clone(&gate) as Arc<dyn PermissionGate>, None).await;
    for name in ["a", "b", "c"] {
        let (agent, _events) = built.spawn(AgentOptions::new(dirs.root.join(name)).mcp(false)).await.expect("spawn");
        agent.send_text("go").await.expect("alive").expect("turn");
        settle(&agent).await;
    }
    let requests = api.requests();
    let bash_schema = |i: usize| {
        let tools = requests[i].json()["tools"].clone();
        tools.as_array().expect("tools").iter().find(|t| t["name"] == "Bash").expect("Bash")["input_schema"].clone()
    };
    let (a, b, c) = (bash_schema(0), bash_schema(2), bash_schema(4));
    assert!(
        a["properties"]["allowed_domains"].is_object() && a["properties"].get("dangerouslyDisableSandbox").is_none()
    );
    assert!(
        b["properties"].get("allowed_domains").is_none() && b["properties"]["dangerouslyDisableSandbox"].is_object()
    );
    assert!(c["properties"]["allowed_domains"].is_object() && c["properties"]["dangerouslyDisableSandbox"].is_object());
    assert!(requests[1].json().to_string().contains("hello"), "A's listed host is reached through the shared proxy");
    assert!(system_note(&requests[2].json()).starts_with("# Environment\n"), "no sandbox section for B");

    let asked: Vec<(String, bool)> = gate.asked.lock().expect("lock").iter().map(|(n, s, _)| (n.clone(), *s)).collect();
    assert_eq!(
        asked,
        [("Bash".to_owned(), false), ("SandboxNetworkAccess".to_owned(), false)],
        "B's command is not sandboxed; only C's connection, unlisted for C, is asked about"
    );
    assert!(requests[5].json().to_string().contains("hello"), "the gate let C through");
}

/// Claude Code checks a removal against the shell's working directory as
/// it is after earlier `cd`s.
#[tokio::test]
async fn removals_are_judged_from_the_live_working_directory() {
    if !Seatbelt::available() {
        return;
    }
    let dirs = Dirs::new("sandbox-live-cwd");
    dirs.write("project/.claude/settings.json", r#"{"sandbox": {"enabled": true}}"#);
    dirs.write("project/sub/keep.txt", "x");
    let sub = std::fs::canonicalize(dirs.project().join("sub")).expect("sub");
    let api = mock([
        bash("c1", json!({"command": "cd sub"})),
        bash("c2", json!({"command": "rm -rf .."})),
        Reply::text("done"),
    ])
    .await;
    let gate = Arc::new(Recorder::default());
    let built = sdk(&api, &dirs, Arc::clone(&gate) as Arc<dyn PermissionGate>, None).await;
    let (agent, _events) = built.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn");
    agent.send_text("go").await.expect("alive").expect("turn");
    settle(&agent).await;
    let asked: Vec<String> = gate
        .asked
        .lock()
        .expect("lock")
        .iter()
        .map(|(_, _, i)| i["command"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(asked, ["rm -rf .."], "`cd sub` is allowed; removing the working directory is asked");
    assert_eq!(gate.cwds.lock().expect("lock").clone(), [Some(sub)], "the gate sees the live directory");
}

#[tokio::test]
async fn a_host_confines_a_command_of_its_own() {
    let dirs = Dirs::new("sandbox-confine");
    let api = mock([]).await;
    let built = sdk(&api, &dirs, Arc::new(Recorder::default()), None).await;
    if !built.sandbox_available() {
        eprintln!("skipped: sandbox-exec is not available here");
        return;
    }
    dirs.write("secret.txt", "TOP-SECRET");
    let spec = ConfineSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "cat ../secret.txt || echo refused".into()],
        cwd: dirs.project(),
        read_paths: vec![dirs.project()],
        tmpdir: dirs.root.join("call"),
        ..ConfineSpec::default()
    };
    let confined: Confined = built.confine(spec).await.expect("confined");
    let out = tokio::process::Command::new(&confined.program)
        .args(&confined.args)
        .env_clear()
        .envs(confined.env.clone())
        .current_dir(dirs.project())
        .output()
        .await
        .expect("spawn");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "refused\n");
    let report: ConfineReport = confined.guard.finish().await;
    assert!(report.violations.contains(&Violation::FileRead { path: dirs.root.join("secret.txt") }), "{report:?}");
}
