#![allow(dead_code, unreachable_pub)]

//! Drives the `eos-agent-server` binary over stdio like a host would.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// Answers the server's own requests (`permission.decide`, `user.ask`).
pub type Answer = Box<dyn FnMut(&str, &Value) -> Value + Send>;

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::UnboundedReceiver<Value>,
    next_id: u64,
    responses: HashMap<u64, Value>,
    /// Every `agent.event` notification so far, in arrival order.
    pub events: Vec<Value>,
    /// Every `task.event` notification so far.
    pub tasks: Vec<Value>,
    /// Every server request so far, with its method.
    pub asked: Vec<Value>,
    pub answer: Answer,
    pub root: PathBuf,
}

impl Client {
    pub fn start(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("eos-server-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "project", "data"] {
            std::fs::create_dir_all(root.join(dir)).expect("dir");
        }
        let root = root.canonicalize().expect("canonical");
        let mut child = Command::new(env!("CARGO_BIN_EXE_eos-agent-server"))
            .env_clear()
            .env("HOME", root.join("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TEST_KEY", "k")
            .env("TEST_SIGN_IN", "t")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("server starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let value: Value = serde_json::from_str(&line).expect("every line is JSON");
                assert_eq!(value["jsonrpc"], "2.0", "{line}");
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let answer: Answer = Box::new(|_, _| json!({"decision": "allow"}));
        Self {
            child,
            stdin,
            lines,
            next_id: 1,
            responses: HashMap::new(),
            events: Vec::new(),
            tasks: Vec::new(),
            asked: Vec::new(),
            answer,
            root,
        }
    }

    pub fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    pub async fn write(&mut self, value: &Value) {
        let mut line = serde_json::to_vec(value).expect("json");
        line.push(b'\n');
        self.stdin.write_all(&line).await.expect("write");
        self.stdin.flush().await.expect("flush");
    }

    /// Sends a request and returns its id without waiting.
    pub async fn start_call(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        id
    }

    /// The response to `id` (`Ok(result)` or `Err(error)`), handling
    /// everything else that arrives meanwhile.
    pub async fn wait(&mut self, id: u64) -> Result<Value, Value> {
        loop {
            if let Some(response) = self.responses.remove(&id) {
                return match response.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(response["result"].clone()),
                };
            }
            self.pump_one().await;
        }
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.start_call(method, params).await;
        self.wait(id).await
    }

    /// Handles one line from the server.
    pub async fn pump_one(&mut self) {
        let message = tokio::time::timeout(Duration::from_secs(20), self.lines.recv())
            .await
            .expect("the server answered in time")
            .expect("the server is running");
        match (message.get("id"), message.get("method").and_then(Value::as_str)) {
            (Some(id), None) => {
                let id = id.as_u64().expect("our ids are numbers");
                self.responses.insert(id, message);
            }
            (Some(id), Some(method)) => {
                let result = (self.answer)(method, &message["params"]);
                self.asked.push(message.clone());
                let id = id.clone();
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
            }
            (None, Some("agent.event")) => self.events.push(message["params"].clone()),
            (None, Some("task.event")) => self.tasks.push(message["params"].clone()),
            (None, other) => panic!("unexpected notification {other:?}"),
        }
    }

    /// Waits until an event matching `pred` has arrived.
    pub async fn until_event(&mut self, pred: impl Fn(&Value) -> bool) {
        while !self.events.iter().any(&pred) {
            self.pump_one().await;
        }
    }

    pub async fn initialize(&mut self, base_url: &str, extra: Value) -> Value {
        let mut params = json!({
            "auth": {"apiKeyEnv": "TEST_KEY"},
            "baseUrl": base_url,
            "model": "mock-model",
            "dataDir": self.root.join("data"),
        });
        if let (Some(map), Some(more)) = (params.as_object_mut(), extra.as_object()) {
            map.extend(more.clone());
        }
        self.call("initialize", params).await.expect("initialized")
    }

    pub async fn spawn_agent(&mut self) -> String {
        let project = self.project();
        let spawned = self.call("agent.spawn", json!({"cwd": project, "mcp": false})).await.expect("spawned");
        spawned["agentId"].as_str().expect("id").to_owned()
    }

    /// The text of every `textDelta` event of `agent`.
    pub fn text_of(&self, agent: &str) -> String {
        self.events
            .iter()
            .filter(|e| e["agentId"] == agent && e["event"]["type"] == "textDelta")
            .filter_map(|e| e["event"]["text"].as_str())
            .collect()
    }

    pub async fn close(mut self) {
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await.expect("exits on EOF");
        assert!(status.expect("status").success());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
