//! What reaches the model in `<sandbox_violations>` under the real macOS
//! sandbox (skipped where `sandbox-exec` is missing): every kind of denial a
//! command can hit, alongside the noise the system makes, attributed to the
//! right command when many run at once.

mod common;

use std::sync::Arc;

use common::{Harness, text};
use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall};
use serde_json::json;

/// Refuses every host the proxy asks about.
struct DenyHosts;

impl PermissionGate for DenyHosts {
    fn decide<'a>(&'a self, _call: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        Box::pin(async { Permission::Deny("no".into()) })
    }
}

fn sandboxed(config: serde_json::Value) -> Option<Harness> {
    Harness::sandboxed(config, Arc::new(DenyHosts))
}

/// The lines of the result's `<sandbox_violations>` block.
fn violations(out: &str) -> Vec<&str> {
    let Some((_, block)) = out.split_once("<sandbox_violations>\n") else { return Vec::new() };
    block.trim_end_matches("</sandbox_violations>").lines().collect()
}

fn has(lines: &[&str], operation: &str, target: &str) -> bool {
    lines.iter().any(|l| l.contains(&format!("deny(1) {operation} ")) && l.contains(target))
}

#[tokio::test]
async fn every_kind_of_denial_reaches_the_model() {
    let Some(h) = sandboxed(json!({"filesystem": {"denyRead": ["secret"]}})) else { return };
    let git = std::process::Command::new("git").args(["init", "-q"]).current_dir(&h.root).status().expect("git");
    assert!(git.success());
    std::fs::create_dir_all(h.root.join("secret")).expect("dir");
    std::fs::create_dir_all(h.root.join(".claude")).expect("dir");
    std::fs::write(h.root.join("secret/key"), "k").expect("file");
    let socket_path = h.base().join("outside.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("unix socket");
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp");
    let port = tcp.local_addr().expect("addr").port();

    let outside = h.base().join("outside.txt");
    let cases: Vec<(String, &str, String)> = vec![
        (format!("echo x > {}", outside.display()), "file-write-create", "outside.txt".into()),
        ("echo x > .git/hooks/pre-commit".into(), "file-write-create", ".git/hooks/pre-commit".into()),
        ("echo x > .claude/settings.json".into(), "file-write-create", ".claude/settings.json".into()),
        ("cat secret/key".into(), "file-read-data", "secret/key".into()),
        (format!("nc -U {} </dev/null", socket_path.display()), "network-outbound", "outside.sock".into()),
        (format!("nc -z 127.0.0.1 {port}"), "network-outbound", format!(":{port}")),
        ("pbpaste".into(), "mach-lookup", "com.apple.pasteboard".into()),
    ];
    for (command, operation, target) in cases {
        let out = h.call("Bash", json!({"command": command})).await;
        let body = text(&out);
        assert!(has(&violations(body), operation, &target), "{command}: {body}");
    }
    assert!(!outside.exists());

    // Signals to processes outside the sandbox are refused, but the kernel
    // logs no line for them (checked with `log stream`): nothing to report.
    let out = h.call("Bash", json!({"command": format!("kill -0 {}", std::process::id())})).await;
    assert!(out.is_error && text(&out).contains("operation not permitted"), "{}", text(&out));

    // The proxy's refusals come in the same block.
    let out = h.call("Bash", json!({"command": "curl -s -m 5 http://blocked.invalid/"})).await;
    let body = text(&out);
    assert!(violations(body).iter().any(|l| l.starts_with("deny network-outbound blocked.invalid:80")), "{body}");
}

#[tokio::test]
async fn system_noise_never_hides_a_real_denial() {
    let Some(h) = sandboxed(json!({})) else { return };
    let outside = h.base().join("mixed.txt");
    // Every process reads sysctls and asks for free space on macOS 27, and
    // `scutil` looks up configd: all reported, as Claude Code reports them.
    let command =
        format!("git --version >/dev/null; scutil --proxy >/dev/null; pbpaste; echo x > {}", outside.display());
    let out = h.call("Bash", json!({"command": command})).await;
    let body = text(&out);
    let lines = violations(body);
    assert!(has(&lines, "file-write-create", "mixed.txt"), "{body}");
    assert!(has(&lines, "mach-lookup", "com.apple.pasteboard"), "{body}");
    assert!(has(&lines, "mach-lookup", "com.apple.SystemConfiguration.configd"), "{body}");

    // A command that fails on one of macOS 27's own probes sees why.
    let out = h.call("Bash", json!({"command": "sysctl kern.iossupportversion"})).await;
    let body = text(&out);
    if out.is_error {
        assert!(has(&violations(body), "sysctl-read", "kern.iossupportversion"), "{body}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_commands_get_only_their_own_lines() {
    let Some(h) = sandboxed(json!({})) else { return };
    let h = Arc::new(h);
    let runs = (0..12).map(|i| {
        let h = Arc::clone(&h);
        tokio::spawn(async move {
            let target = h.base().join(format!("own-{i}.txt"));
            let out =
                h.call("Bash", json!({"command": format!("sleep 0.0{}; echo x > {}", i % 5, target.display())})).await;
            (i, text(&out).to_owned())
        })
    });
    for run in runs {
        let (i, body) = run.await.expect("join");
        let lines = violations(&body);
        assert!(has(&lines, "file-write-create", &format!("own-{i}.txt")), "{i}: {body}");
        let others: Vec<_> =
            lines.iter().filter(|l| l.contains("own-") && !l.contains(&format!("own-{i}.txt"))).collect();
        assert!(others.is_empty(), "{i} got another command's lines: {others:?}");
    }
}
