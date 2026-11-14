#![allow(dead_code, unreachable_pub)]

pub mod mcp;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{AgentEvent, Credential, StaticCredential};
use eos_agent_mock_api::{MockApi, Recorded, Reply};
use eos_agent_sdk::{AgentOptions, Sdk, SdkAgent, SdkBuilder};
use serde_json::Value;
use tokio::sync::mpsc;

/// A scratch directory tree for one test: a home, a project and SDK data.
pub struct Dirs {
    pub root: PathBuf,
}

impl Dirs {
    pub fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("eos-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "project", "data"] {
            std::fs::create_dir_all(root.join(dir)).expect("dir");
        }
        Self { root: root.canonicalize().expect("canonical") }
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    pub fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    pub fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, text).expect("write");
    }
}

impl Drop for Dirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub async fn sdk(mock: &MockApi, dirs: &Dirs) -> Sdk {
    sdk_with_model(mock, dirs, "mock-model").await
}

pub async fn sdk_with_model(mock: &MockApi, dirs: &Dirs, model: &str) -> Sdk {
    builder(mock, dirs).model(model).build().await.expect("sdk")
}

/// A builder set up for `mock` and `dirs`, for tests that add their own settings.
pub fn builder(mock: &MockApi, dirs: &Dirs) -> SdkBuilder {
    Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
}

pub async fn agent(sdk: &Sdk, dirs: &Dirs) -> (SdkAgent, mpsc::Receiver<AgentEvent>) {
    sdk.spawn(AgentOptions::new(dirs.project()).mcp(false)).await.expect("spawn")
}

pub async fn mock(replies: impl IntoIterator<Item = Reply>) -> MockApi {
    MockApi::start(replies).await.expect("mock")
}

/// Waits (bounded) until the agent and its subagents are done.
pub async fn settle(agent: &SdkAgent) {
    tokio::time::timeout(Duration::from_secs(20), agent.settle()).await.expect("settled in time");
}

pub fn tool_names(request: &Recorded) -> Vec<String> {
    request.json()["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// Every text in the request's messages, joined.
pub fn all_text(request: &Recorded) -> String {
    let mut out = String::new();
    collect(&request.json()["messages"], &mut out);
    out
}

fn collect(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect(v, out)),
        _ => {}
    }
}

pub fn system_text(request: &Recorded) -> String {
    request.json()["system"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect()
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}
