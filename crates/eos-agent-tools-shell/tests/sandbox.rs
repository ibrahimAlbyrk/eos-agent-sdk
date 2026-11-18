//! Commands under the real macOS sandbox (skipped where `sandbox-exec` is
//! missing). Network tests use local servers only.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Harness, alive, output, pid_from, task_id, text, wait_until};
use eos_agent_core::{BoxFuture, CancellationToken, Permission, PermissionGate, ToolCall};
use eos_agent_tools_shell::sandbox::{ConfineSpec, Confined, Violation};
use eos_agent_tools_shell::{SandboxDirs, SandboxRuntime, sandbox_schema};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Answers every question the same way and remembers what it was asked.
#[derive(Default)]
struct Gate {
    allow: AtomicBool,
    asked: Mutex<Vec<(String, Value)>>,
}

impl Gate {
    fn asked(&self) -> Vec<(String, Value)> {
        self.asked.lock().expect("lock").clone()
    }
}

impl PermissionGate for Gate {
    fn decide<'a>(&'a self, call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        let input = serde_json::from_str(call.input.get()).unwrap_or(Value::Null);
        self.asked.lock().expect("lock").push((call.name.to_owned(), input));
        let answer = if self.allow.load(Ordering::SeqCst) { Permission::Allow } else { Permission::Deny("no".into()) };
        Box::pin(async move { answer })
    }
}

fn sandboxed(config: Value) -> Option<(Harness, Arc<Gate>)> {
    let gate = Arc::new(Gate::default());
    Harness::sandboxed(config, Arc::clone(&gate) as Arc<dyn PermissionGate>).map(|h| (h, gate))
}

/// A local HTTP server answering `hello` to every request; returns its port.
async fn upstream() -> u16 {
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
async fn writes_stay_inside_the_working_directories() {
    let Some((h, _)) = sandboxed(json!({})) else { return };
    let out = h.call("Bash", json!({"command": "echo inside > made.txt && cat made.txt"})).await;
    assert_eq!((output(&out), out.is_error), ("inside", false));

    let outside = h.base().join("outside.txt");
    let out = h.call("Bash", json!({"command": format!("echo x > {}", outside.display())})).await;
    assert!(out.is_error);
    let body = text(&out);
    assert!(body.to_lowercase().contains("operation not permitted"), "{body}");
    assert!(body.contains("\n<sandbox_violations>\n"), "{body}");
    assert!(body.contains("deny(1) file-write-create"), "{body}");
    assert!(body.contains("outside.txt"), "{body}");
    assert!(body.ends_with("</sandbox_violations>"), "{body}");
    assert!(!outside.exists());

    // Reads are not limited; $TMPDIR is writable.
    let out = h
        .call(
            "Bash",
            json!({"command": "head -c 5 /etc/hosts >/dev/null && echo t > \"$TMPDIR/t\" && cat \"$TMPDIR/t\""}),
        )
        .await;
    assert_eq!((output(&out), out.is_error), ("t", false));
}

#[tokio::test]
async fn protected_files_inside_the_roots_stay_read_only() {
    let Some((h, _)) = sandboxed(json!({})) else { return };
    // As in Claude Code, a repository is made outside the sandbox: `.git` itself
    // is protected from being created (or swapped) by a sandboxed command.
    let out = h.call("Bash", json!({"command": "git init -q"})).await;
    assert!(out.is_error && text(&out).contains("file-write-create"), "{}", text(&out));
    let git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&h.root).status().expect("git");
    assert!(git.success());
    std::fs::create_dir_all(h.root.join("sub/.git/hooks")).expect("nested repo");
    for target in [
        ".git/hooks/pre-commit",
        ".git/config",
        ".zshrc",
        "sub/.git/hooks/x",
        ".claude/settings.json",
        ".vscode/settings.json",
    ] {
        let out = h
            .call(
                "Bash",
                json!({"command": format!("mkdir -p \"$(dirname {target})\" 2>/dev/null; echo x > {target}")}),
            )
            .await;
        assert!(out.is_error, "{target} was writable: {}", text(&out));
        assert!(text(&out).contains("<sandbox_violations>"), "{target}: {}", text(&out));
    }
    // Claude Code's default-on gates: the legacy remote folders (made for it),
    // a nested repository's `config.lock` in any spelling APFS folds together,
    // and the git `info` folders pinned against being moved away.
    let git_dir = h.root.join(".git");
    assert!(git_dir.join("remotes").is_dir() && git_dir.join("objects/info/commit-graphs").is_dir());
    for command in [
        "echo x > .git/remotes/origin",
        "echo x > .git/branches/main",
        "echo x > sub/.git/config.lock",
        "echo x > sub/.git/CONFIG.LOCK",
        "echo x > sub/.git/con\u{FB01}g.lock",
        "mv .git/info .git/info-moved",
        "mv .git/objects/info/commit-graphs .git/objects/info/moved",
    ] {
        let out = h.call("Bash", json!({ "command": command })).await;
        assert!(out.is_error, "{command} was allowed: {}", text(&out));
    }
    assert!(git_dir.join("info").is_dir() && !git_dir.join("info-moved").exists());
    // Ordinary git work inside the root still works.
    let out = h
        .call(
            "Bash",
            json!({"command": "echo a > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -qm first && git log --format=%s"}),
        )
        .await;
    assert_eq!((output(&out), out.is_error), ("first", false), "{}", text(&out));
}

#[tokio::test]
async fn deny_read_and_cd_carry_over() {
    let Some((h, _)) = sandboxed(json!({"filesystem": {"denyRead": ["secret"]}})) else { return };
    std::fs::create_dir_all(h.root.join("secret")).expect("dir");
    std::fs::write(h.root.join("secret/key"), "k").expect("file");
    let out = h.call("Bash", json!({"command": "cat secret/key"})).await;
    assert!(out.is_error && text(&out).contains("Operation not permitted"), "{}", text(&out));
    assert!(text(&out).contains("deny(1) file-read-data"), "{}", text(&out));

    let out = h.call("Bash", json!({"command": "cd sub"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(h.workspace().cwd(), h.root.join("sub"));
}

#[tokio::test]
async fn unsandboxed_runs_and_the_sandboxed_flag() {
    let Some((h, _)) = sandboxed(json!({"excludedCommands": ["touch *"]})) else { return };
    let outside = h.base().join("free.txt");
    let bash = h.tool("Bash");
    let raw = |v: Value| RawValue::from_string(v.to_string()).expect("raw");
    let cmd = format!("echo x > {}", outside.display());
    assert!(bash.runs_sandboxed(&raw(json!({"command": cmd})), h.ext()));
    assert!(!bash.runs_sandboxed(&raw(json!({"command": cmd, "dangerouslyDisableSandbox": true})), h.ext()));
    assert!(!bash.runs_sandboxed(&raw(json!({"command": format!("touch {}", outside.display())})), h.ext()));
    assert!(!bash.runs_sandboxed(&raw(json!({"command": cmd})), &Default::default()), "an agent without a sandbox");
    let out = h.call("Bash", json!({"command": cmd, "dangerouslyDisableSandbox": true})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert!(outside.exists());
    let out =
        h.call("Bash", json!({"command": "ls", "dangerouslyDisableSandbox": true, "allowed_domains": ["a.com"]})).await;
    assert!(text(&out).contains("allowed_domains applies only to a command that runs in the sandbox"));
}

#[tokio::test]
async fn network_goes_through_the_proxy() {
    let allowed = upstream().await;
    let Some((h, gate)) = sandboxed(json!({"network": {"allowedDomains": [format!("127.0.0.1:{allowed}")]}})) else {
        return;
    };
    let curl = |extra: &str, port: u16| format!("curl -sS --max-time 10 --noproxy '' {extra} http://127.0.0.1:{port}/");
    let out = h.call("Bash", json!({"command": curl("", allowed)})).await;
    assert_eq!((output(&out), out.is_error), ("hello", false), "absolute-form");
    let out = h.call("Bash", json!({"command": curl("-p", allowed)})).await;
    assert_eq!((output(&out), out.is_error), ("hello", false), "CONNECT");
    let out = h.call("Bash", json!({"command": curl("-x \"$FTP_PROXY\"", allowed)})).await;
    assert_eq!((output(&out), out.is_error), ("hello", false), "SOCKS5: {}", text(&out));
    assert!(gate.asked().is_empty(), "listed hosts are not asked about");

    // Straight to a local port, or resolving a name itself: refused by the sandbox.
    let out = h
        .call("Bash", json!({"command": format!("curl -sS --max-time 5 --noproxy '*' http://127.0.0.1:{allowed}/")}))
        .await;
    assert!(out.is_error, "{}", text(&out));
    let out = h.call("Bash", json!({"command": "curl -sS --max-time 5 --noproxy '*' http://example.com/"})).await;
    assert!(text(&out).contains("Could not resolve host"), "no DNS inside the sandbox: {}", text(&out));

    // Wrong credentials: 407, never asked.
    let out = h
        .call("Bash", json!({"command": format!("curl -s -o /dev/null -w '%{{http_code}}' --noproxy '' -x \"http://eos.x:bad@127.0.0.1:${{HTTP_PROXY##*:}}\" http://127.0.0.1:{allowed}/")}))
        .await;
    assert_eq!(output(&out), "407");
}

#[tokio::test]
async fn unlisted_hosts_ask_the_gate_once_per_session() {
    let other = upstream().await;
    let Some((h, gate)) = sandboxed(json!({})) else { return };
    let curl =
        format!("curl -sS --max-time 10 --noproxy '' -o /dev/null -w '%{{http_code}}' http://127.0.0.1:{other}/");
    let out = h.call("Bash", json!({"command": curl})).await;
    assert!(text(&out).starts_with("403"), "{}", text(&out));
    assert!(
        text(&out).contains(&format!("\ndeny network-outbound 127.0.0.1:{other} (user denied)\n</sandbox_violations>")),
        "{}",
        text(&out)
    );
    assert_eq!(gate.asked(), vec![("SandboxNetworkAccess".to_owned(), json!({"host": "127.0.0.1", "port": other}))]);

    gate.allow.store(true, Ordering::SeqCst);
    let out = h.call("Bash", json!({"command": curl})).await;
    assert_eq!(output(&out), "200");
    let out = h.call("Bash", json!({"command": curl})).await;
    assert_eq!(output(&out), "200");
    assert_eq!(gate.asked().len(), 2, "a yes lasts for the session");
}

#[tokio::test]
async fn strict_allowlists_and_deny_lists_never_ask() {
    let port = upstream().await;
    let Some((h, gate)) = sandboxed(json!({"network": {"strictAllowlist": true}})) else { return };
    let out = h
        .call("Bash", json!({"command": format!("curl -s --noproxy '' -w '%{{http_code}}' http://127.0.0.1:{port}/")}))
        .await;
    assert!(text(&out).contains("host is not on the allow list"), "{}", text(&out));
    assert!(gate.asked().is_empty());
}

#[tokio::test]
async fn timeouts_kills_and_background_tasks_work_sandboxed() {
    let Some((h, _)) = sandboxed(json!({})) else { return };
    // Interrupting a foreground command kills its process group.
    let cancel = CancellationToken::new();
    let pidfile = h.root.join("pid");
    let call = h.call_with("Bash", json!({"command": "echo $$ > pid; sleep 30"}), &cancel);
    let pid = tokio::select! {
        _ = call => panic!("finished early"),
        pid = pid_from(&pidfile) => pid,
    };
    // The kill is delivered asynchronously; the shell is gone shortly after.
    wait_until(|| !alive(pid)).await;

    // A foreground timeout moves it to the background; TaskStop kills it there.
    let out = h.call("Bash", json!({"command": "echo $$ > pid2; sleep 30", "timeout": 500})).await;
    let id = task_id(text(&out));
    let pid = pid_from(&h.root.join("pid2")).await;
    assert!(alive(pid));
    let out = h.call("TaskStop", json!({"task_id": id})).await;
    assert!(!out.is_error, "{}", text(&out));
    wait_until(|| !alive(pid)).await;

    // Background commands and monitors run sandboxed too.
    let outside = h.base().join("bg.txt");
    let out = h
        .call(
            "Bash",
            json!({"command": format!("echo x > {} || echo refused", outside.display()), "run_in_background": true}),
        )
        .await;
    let id = task_id(text(&out));
    h.wait_reminders(1).await;
    let file = h.scratch().join("tasks").join(format!("{id}.output"));
    assert!(std::fs::read_to_string(file).expect("output").contains("refused"));
    let out = h
        .call("Monitor", json!({"description": "w", "command": format!("echo x > {} || echo blocked", outside.display()), "timeout_ms": 5000, "persistent": false}))
        .await;
    assert!(!out.is_error, "{}", text(&out));
    let notes = h.wait_reminders(1).await;
    assert!(notes.iter().any(|n| n.contains("blocked")), "{notes:?}");
    assert!(!outside.exists());
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[test]
fn the_schema_follows_claude_code() {
    let plain = Harness::new();
    let schema = |tool: &str| -> Value {
        let def: Value = serde_json::from_slice(plain.tool(tool).spec().json()).expect("json");
        def["input_schema"].clone()
    };
    let mut bash = schema("Bash");
    assert!(bash["properties"].get("allowed_domains").is_none());
    assert!(sandbox_schema("Bash", &mut bash, true));
    let captured: Value =
        serde_json::from_str(include_str!("fixtures/Bash.sandbox.schema.json")).expect("captured schema");
    assert_eq!(bash, captured, "Bash schema as Claude Code 2.1.319 sends it with the sandbox on");
    let mut monitor = schema("Monitor");
    assert!(sandbox_schema("Monitor", &mut monitor, false));
    assert!(monitor["properties"]["allowed_domains"].is_object());
    assert!(monitor["properties"].get("dangerouslyDisableSandbox").is_none());
    let mut strict = schema("Bash");
    sandbox_schema("Bash", &mut strict, false);
    assert!(strict["properties"].get("dangerouslyDisableSandbox").is_none());
    let mut read = json!({"properties": {}});
    assert!(!sandbox_schema("Read", &mut read, true));
}

/// `cargo test -p eos-agent-tools-shell --release --test sandbox command_cost -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement, not a test"]
async fn command_cost() {
    let Some((sandboxed, _)) = sandboxed(json!({})) else { return };
    let plain = Harness::new();
    let bare = mean_of_50(&plain).await;
    let boxed = mean_of_50(&sandboxed).await;
    println!(
        "Bash `true`, mean of 50: unsandboxed {bare:?}, sandboxed {boxed:?}, overhead {:?}",
        boxed.saturating_sub(bare)
    );
    let start = std::time::Instant::now();
    let outs =
        futures_util::future::join_all((0..200).map(|_| sandboxed.call("Bash", json!({"command": "true"})))).await;
    assert!(outs.iter().all(|o| !o.is_error));
    println!("200 concurrent sandboxed commands: {:?}", start.elapsed());
}

async fn mean_of_50(h: &Harness) -> Duration {
    h.call("Bash", json!({"command": "true"})).await; // the snapshot, proxy and monitor start here
    let start = std::time::Instant::now();
    for _ in 0..50 {
        assert!(!h.call("Bash", json!({"command": "true"})).await.is_error);
    }
    start.elapsed() / 50
}

/// A host's own folders for a confined command: a package with a `data`
/// folder, and a secret beside it.
struct HostDirs {
    base: PathBuf,
    /// `base` as the kernel names it (through `/var` -> `/private/var`).
    real: PathBuf,
}

impl HostDirs {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!("eos-confine-test-{}", fastrand::u64(..)));
        std::fs::create_dir_all(base.join("pkg/data")).expect("dirs");
        std::fs::write(base.join("pkg/readme"), "PKG-FILE").expect("readme");
        std::fs::write(base.join("secret.txt"), "TOP-SECRET").expect("secret");
        let real = std::fs::canonicalize(&base).expect("real");
        Self { base, real }
    }

    fn spec(&self, script: &str) -> ConfineSpec {
        ConfineSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: self.base.join("pkg"),
            env: vec![("GREETING".into(), "hi".into()), ("PATH".into(), "/usr/bin:/bin".into())],
            read_paths: vec![self.base.join("pkg")],
            write_paths: vec![self.base.join("pkg/data")],
            tmpdir: self.base.join("call"),
            ..ConfineSpec::default()
        }
    }
}

impl Drop for HostDirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn confining_runtime() -> Option<Arc<SandboxRuntime>> {
    let runtime = SandboxRuntime::new(SandboxDirs::from_env(), Arc::new(Gate::default()));
    if !runtime.available() {
        eprintln!("skipped: sandbox-exec is not available here");
        return None;
    }
    Some(runtime)
}

/// Starts it as a host does: its own process, exactly the confined environment.
async fn run_confined(confined: &Confined, cwd: &Path) -> String {
    let out = tokio::process::Command::new(&confined.program)
        .args(&confined.args)
        .env_clear()
        .envs(confined.env.clone())
        .current_dir(cwd)
        .output()
        .await
        .expect("spawn");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test]
async fn a_confined_host_command_touches_only_what_it_was_given() {
    let Some(runtime) = confining_runtime() else { return };
    let dirs = HostDirs::new();
    let port = upstream().await;
    let outside = dirs.base.join("outside.txt");
    let curl = "curl -fsS --max-time 5 --noproxy '' -o /dev/null";
    let script = format!(
        "cat readme; echo; cat ../secret.txt || echo no-secret; echo made > data/made.txt && echo wrote; \
         echo x > {outside} || echo no-outside; /usr/bin/security list-keychains || echo no-security; \
         {curl} http://blocked.invalid/ || echo no-host; {curl} http://localhost:{port}/ || echo no-loopback; env",
        outside = outside.display()
    );
    let spec = ConfineSpec { hosts: vec!["localhost".into()], ..dirs.spec(&script) };
    let confined = runtime.confine(spec).await.expect("confined");
    assert_eq!(confined.program, Path::new("/usr/bin/sandbox-exec"));
    let out = run_confined(&confined, &dirs.base.join("pkg")).await;
    let violations = confined.guard.finish().await;

    for line in ["PKG-FILE", "no-secret", "wrote", "no-outside", "no-security", "no-host", "no-loopback"] {
        assert!(out.contains(line), "{line}: {out}");
    }
    assert!(!out.contains("TOP-SECRET"), "{out}");
    assert_eq!(std::fs::read_to_string(dirs.base.join("pkg/data/made.txt")).expect("made"), "made\n");
    assert!(!outside.exists());
    // Its environment is the spec's and the sandbox's, nothing of the host's.
    assert!(out.contains("GREETING=hi\n") && out.contains("HTTP_PROXY=http://eos.") && !out.contains("HOME="), "{out}");
    assert!(out.contains(&format!("TMPDIR={}\n", dirs.base.join("call").display())), "{out}");

    let expected = [
        Violation::FileRead { path: dirs.real.join("secret.txt") },
        Violation::FileWrite { path: dirs.real.join("outside.txt") },
        Violation::Exec { path: "/usr/bin/security".into() },
        Violation::Network { target: "blocked.invalid:80".into() },
        Violation::Network { target: format!("localhost:{port}") },
    ];
    for v in &expected {
        assert!(violations.contains(v), "{v:?} in {violations:?}");
    }
}

#[tokio::test]
async fn an_audit_lets_reads_through_and_reports_them() {
    let Some(runtime) = confining_runtime() else { return };
    let dirs = HostDirs::new();
    let outside = dirs.base.join("outside.txt");
    let script = format!("cat ../secret.txt; echo x > {} || echo no-outside", outside.display());
    let confined = runtime.confine(ConfineSpec { audit: true, ..dirs.spec(&script) }).await.expect("confined");
    let out = run_confined(&confined, &dirs.base.join("pkg")).await;
    let violations = confined.guard.finish().await;
    assert!(out.contains("TOP-SECRET") && out.contains("no-outside"), "{out}");
    assert!(!outside.exists());
    assert!(violations.contains(&Violation::FileRead { path: dirs.real.join("secret.txt") }), "{violations:?}");
    assert!(violations.contains(&Violation::FileWrite { path: dirs.real.join("outside.txt") }), "{violations:?}");
}
