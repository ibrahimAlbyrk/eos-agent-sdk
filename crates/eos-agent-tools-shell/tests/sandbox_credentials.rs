//! `sandbox.credentials` under the real macOS sandbox (skipped where
//! `sandbox-exec` is missing): variables unset or replaced by placeholders,
//! credential files unreadable whatever `allowRead` says.

mod common;

use std::sync::Arc;

use common::{Harness, output, text};
use eos_agent_core::{BoxFuture, Permission, PermissionGate, ToolCall};
use serde_json::json;

struct No;

impl PermissionGate for No {
    fn decide<'a>(&'a self, _: ToolCall<'a>) -> BoxFuture<'a, Permission> {
        Box::pin(async { Permission::Deny("no".into()) })
    }
}

fn sandboxed(config: serde_json::Value) -> Option<Harness> {
    Harness::sandboxed(config, Arc::new(No))
}

#[tokio::test]
async fn variables_are_unset_or_masked() {
    // Cargo sets both for a test process.
    let Some(h) = sandboxed(json!({"credentials": {"envVars": [
        {"name": "CARGO_PKG_NAME", "mode": "deny"},
        {"name": "CARGO_MANIFEST_DIR", "mode": "mask", "extract": "/(crates)/"},
        {"name": "CARGO_PKG_VERSION", "mode": "mask"},
    ]}})) else {
        return;
    };
    let out = h
        .call(
            "Bash",
            json!({"command": "printf '%s|%s|%s' \"${CARGO_PKG_NAME-unset}\" \"$CARGO_MANIFEST_DIR\" \"$CARGO_PKG_VERSION\""}),
        )
        .await;
    let body = output(&out).to_owned();
    let parts: Vec<&str> = body.split('|').collect();
    assert_eq!(parts[0], "unset", "{body}");
    assert!(parts[1].contains("/fake_value_") && parts[1].ends_with("/eos-agent-tools-shell"), "{body}");
    assert!(!parts[1].contains("/crates/"), "{body}");
    assert!(parts[2].starts_with("fake_value_") && parts[2].len() == 47, "{body}");

    let again = h.call("Bash", json!({"command": "printf %s \"$CARGO_PKG_VERSION\""})).await;
    assert_eq!(output(&again), parts[2], "a variable keeps its placeholder");
}

#[tokio::test]
async fn a_failing_extract_stops_the_command() {
    let Some(h) = sandboxed(json!({"credentials": {"envVars": [
        {"name": "CARGO_PKG_NAME", "mode": "mask", "extract": "nothing=(\\w+)", "onExtractNoMatch": "error"},
    ]}})) else {
        return;
    };
    let out = h.call("Bash", json!({"command": "echo hi"})).await;
    assert!(out.is_error);
    assert!(text(&out).contains("matched nothing (onExtractNoMatch: \"error\")"), "{}", text(&out));
}

#[tokio::test]
async fn credential_files_stay_unreadable() {
    let dir =
        std::fs::canonicalize(std::env::temp_dir()).expect("tmp").join(format!("eos-cred-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let secret = dir.join("netrc");
    std::fs::write(&secret, "machine x password p").expect("write");
    let Some(h) = sandboxed(json!({
        "filesystem": {"allowRead": [dir.to_string_lossy()]},
        "credentials": {"files": [{"path": secret.to_string_lossy(), "mode": "mask"}]},
    })) else {
        return;
    };
    let out = h.call("Bash", json!({"command": format!("cat {}", secret.display())})).await;
    assert!(out.is_error, "{}", text(&out));
    assert!(text(&out).to_lowercase().contains("operation not permitted"), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A local HTTP server answering with the request's `authorization` header.
async fn echo_auth() -> u16 {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => seen.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&seen).to_string();
                let auth = head
                    .lines()
                    .find_map(|l| l.strip_prefix("authorization: ").or_else(|| l.strip_prefix("Authorization: ")))
                    .unwrap_or("")
                    .to_owned();
                let reply =
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{auth}", auth.len());
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
async fn plain_http_gets_real_values_only_when_allowed() {
    let port = echo_auth().await;
    let real = std::env::var("CARGO_PKG_VERSION").expect("cargo sets it");
    let command =
        format!("curl -s --noproxy '' -H \"Authorization: Bearer $CARGO_PKG_VERSION\" http://127.0.0.1:{port}/");
    for (allow, expect_real) in [(true, true), (false, false)] {
        let Some(h) = sandboxed(json!({
            "network": {"allowedDomains": ["127.0.0.1"]},
            "credentials": {"allowPlaintextInject": allow, "envVars": [{"name": "CARGO_PKG_VERSION", "mode": "mask"}]},
        })) else {
            return;
        };
        let out = h.call("Bash", json!({"command": command})).await;
        let body = output(&out);
        if expect_real {
            assert_eq!(body, format!("Bearer {real}"));
        } else {
            assert!(body.starts_with("Bearer fake_value_"), "{body}");
        }
    }
}

#[tokio::test]
async fn commands_trust_the_terminating_proxy() {
    // Accepts and closes: the proxy's own TLS connection to it fails.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    let Some(h) = sandboxed(json!({"network": {"allowedDomains": ["127.0.0.1"], "tlsTerminate": {}}})) else { return };
    let command =
        format!("curl -s -w ' %{{http_code}}' --noproxy '' https://127.0.0.1:{port}/; printf ' %s' \"$SSL_CERT_FILE\"");
    let out = h.call("Bash", json!({"command": command})).await;
    let body = output(&out);
    assert!(body.starts_with("Bad Gateway 502 "), "curl trusted the proxy's CA and got its answer: {body}");
    assert!(body.ends_with("/trust-bundle.crt"), "{body}");
}
