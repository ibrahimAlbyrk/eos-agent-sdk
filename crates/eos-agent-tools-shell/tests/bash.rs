mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Harness, text};
use serde_json::json;
use serde_json::value::RawValue;

#[tokio::test]
async fn echoes_and_reports_exit_codes() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"command": "echo hello"})).await;
    assert_eq!((text(&out), out.is_error), ("hello", false));

    let out = h.call("Bash", json!({"command": "echo out; echo err >&2; exit 3"})).await;
    assert_eq!((text(&out), out.is_error), ("Exit code 3\nout\nerr", true));

    let out = h.call("Bash", json!({"command": "true", "description": "Do nothing"})).await;
    assert_eq!((text(&out), out.is_error), ("(Bash completed with no output)", false));

    // grep finding nothing exits 1, which is not a failure.
    let out = h.call("Bash", json!({"command": "grep needle /dev/null"})).await;
    assert_eq!((text(&out), out.is_error), ("(Bash completed with no output)", false));

    let out = h.call("Bash", json!({"command": "printf '\\n\\n  indented\\nlast  \\n\\n'"})).await;
    assert_eq!(text(&out), "  indented\nlast");
}

#[tokio::test]
async fn refuses_bad_input_and_long_sleeps() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"cmd": "ls"})).await;
    assert!(out.is_error);
    assert!(text(&out).starts_with("<tool_use_error>InputValidationError: "), "{}", text(&out));

    let out = h.call("Bash", json!({"command": "sleep 30 && echo done"})).await;
    assert!(out.is_error);
    assert!(
        text(&out).starts_with("<tool_use_error>Blocked: sleep 30 followed by: echo done. To wait for a condition")
    );

    let out = h.call("Bash", json!({"command": "sleep 0.1 && echo ok"})).await;
    assert_eq!(text(&out), "ok");
}

#[tokio::test]
async fn cd_carries_over_inside_the_roots_only() {
    let h = Harness::new();
    let ws = h.workspace();
    let out = h.call("Bash", json!({"command": "cd sub"})).await;
    assert!(!out.is_error, "{}", text(&out));
    assert_eq!(ws.cwd(), h.root.join("sub"));

    let out = h.call("Bash", json!({"command": "basename \"$(pwd)\""})).await;
    assert_eq!(text(&out), "sub");

    // A failing command does not move the cwd.
    let out = h.call("Bash", json!({"command": "cd .. && false"})).await;
    assert!(out.is_error);
    assert_eq!(ws.cwd(), h.root.join("sub"));

    let out = h.call("Bash", json!({"command": "cd / && echo moved"})).await;
    assert_eq!(text(&out), format!("moved\nShell cwd was reset to {}", h.root.display()));
    assert_eq!(ws.cwd(), h.root);
}

/// As Bash's description says, `cd` carries over only in the main session.
#[tokio::test]
async fn a_subagents_cd_does_not_carry_over() {
    let h = Harness::new();
    let origin = eos_agent_core::AgentOrigin {
        root_id: "main".into(),
        parent_id: "main".into(),
        tool_use_id: None,
        workflow: None,
    };
    h.ext().insert(origin);
    let ws = h.workspace();
    let out = h.call("Bash", json!({"command": "cd sub && echo in"})).await;
    assert_eq!(text(&out), "in");
    assert_eq!(ws.cwd(), h.root);
    let out = h.call("Bash", json!({"command": "cd / && echo out"})).await;
    assert_eq!(text(&out), "out", "nothing to reset");
    let out = h.call("Bash", json!({"command": "pwd -P"})).await;
    assert_eq!(text(&out), h.root.canonicalize().expect("root").display().to_string());
}

#[tokio::test]
async fn large_output_spills_to_a_file_with_a_preview() {
    let h = Harness::new();
    let out = h.call("Bash", json!({"command": "seq 1 20000"})).await;
    let body = text(&out);
    assert!(!out.is_error);
    assert!(body.starts_with("<persisted-output>\nOutput too large (106.3KB). Full output saved to: "), "{body}");
    assert!(body.contains("\n\nPreview (first 2KB):\n1\n2\n3\n"));
    assert!(body.ends_with("\n...\n</persisted-output>"));
    let path = body.lines().nth(1).and_then(|l| l.split("saved to: ").nth(1)).expect("a path");
    let saved = std::fs::read_to_string(path).expect("spill file");
    assert_eq!(saved.lines().count(), 20_000);
    assert!(path.ends_with("tool-results/toolu_0.txt"), "{path}");

    let h = Harness::with(|c| c.limits.output_max_chars = 10);
    let out = h.call("Bash", json!({"command": "echo 0123456789"})).await;
    assert_eq!(text(&out), "0123456789");
    let out = h.call("Bash", json!({"command": "echo 0123456789a"})).await;
    assert!(text(&out).starts_with("<persisted-output>\nOutput too large (12 bytes)."), "{}", text(&out));
    assert!(text(&out).ends_with("0123456789a\n</persisted-output>"));
}

#[tokio::test]
async fn read_only_commands_are_concurrency_safe() {
    let h = Harness::new();
    let bash = h.tool("Bash");
    let safe = |cmd: &str| {
        let raw = RawValue::from_string(json!({ "command": cmd }).to_string()).expect("raw");
        bash.is_concurrency_safe(&raw)
    };
    assert!(safe("git status && ls -la"));
    assert!(safe("grep -rn TODO src | wc -l"));
    assert!(!safe("rm -rf target"));
    assert!(!safe("echo hi > file"));
    assert!(!safe("cat $(ls)"));
    let raw = RawValue::from_string("{\"nope\":1}".into()).expect("raw");
    assert!(!bash.is_concurrency_safe(&raw));
}

#[tokio::test]
async fn the_process_budget_limits_concurrent_commands() {
    let h = Arc::new(Harness::with(|c| c.budget = Arc::new(tokio::sync::Semaphore::new(1))));
    // Warm the snapshot so it does not count in the timing.
    h.call("Bash", json!({"command": "true"})).await;
    let start = Instant::now();
    let (a, b) = tokio::join!(
        h.call("Bash", json!({"command": "sleep 0.6; echo a"})),
        h.call("Bash", json!({"command": "sleep 0.6; echo b"})),
    );
    assert_eq!((text(&a), text(&b)), ("a", "b"));
    assert!(start.elapsed() >= Duration::from_millis(1200), "ran together: {:?}", start.elapsed());

    let h = Harness::new();
    h.call("Bash", json!({"command": "true"})).await;
    let start = Instant::now();
    let _ =
        tokio::join!(h.call("Bash", json!({"command": "sleep 0.6"})), h.call("Bash", json!({"command": "sleep 0.6"})),);
    assert!(start.elapsed() < Duration::from_millis(1100), "serialized: {:?}", start.elapsed());
}
