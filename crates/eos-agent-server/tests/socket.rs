use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

#[tokio::test]
async fn serves_a_private_unix_socket() {
    let dir = std::env::temp_dir().join(format!("eos-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).expect("dir");
    let path = dir.join("s.sock");
    let mut server = Command::new(env!("CARGO_BIN_EXE_eos-agent-server"))
        .arg("--socket")
        .arg(&path)
        .env_clear()
        .env("HOME", dir.join("home"))
        .env("TEST_KEY", "k")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("server starts");
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mode = std::fs::metadata(&path).expect("socket").permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let stream = UnixStream::connect(&path).await.expect("connect");
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let requests = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"auth":{"apiKeyEnv":"TEST_KEY"},"permissions":"allow"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"agent.list"}),
    ];
    for request in requests {
        write.write_all(format!("{request}\n").as_bytes()).await.expect("write");
        let line = lines.next_line().await.expect("read").expect("a line");
        let response: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(response["id"], request["id"]);
        assert!(response.get("result").is_some(), "{response}");
    }
    server.kill().await.expect("stopped");
    let _ = std::fs::remove_dir_all(&dir);
}
