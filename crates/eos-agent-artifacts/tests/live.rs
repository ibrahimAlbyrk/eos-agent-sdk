//! Drives the real frame API with a real Claude sign-in, by hand. Read-only
//! listing:
//! `EOS_CLAUDE_STORE=~/.eos/accounts/claude cargo test -p eos-agent-artifacts --test live -- --ignored --nocapture lists`
//!
//! One tool call (it publishes, edits and deletes for real):
//! `EOS_LIVE_CALL='{"tool":"Artifact","input":{…}}' EOS_LIVE_DIR=/some/dir … -- --ignored --nocapture one_call`
//! (`EOS_LIVE_DIR` is the agent's working directory for local files.)
//! Or one raw request, printed as status and body:
//! `EOS_LIVE_CALL='{"raw":{"method":"GET","path":"/api/frame/…","body":{…}}}'`
//! Tokens are never printed.

use std::path::PathBuf;
use std::sync::Arc;

use eos_agent_anthropic::auth::ClaudeLogin;
use eos_agent_artifacts::{DEFAULT_BASE_URL, FrameConfig, tools};
use eos_agent_core::{CancellationToken, Credential, Extensions, TokenSource, ToolContext, Workspace};
use eos_agent_types::ToolResultContent;
use serde_json::Value;

fn sign_in() -> Arc<dyn TokenSource> {
    let store = std::env::var_os("EOS_CLAUDE_STORE").or_else(|| std::env::var_os("CLAUDE_SECURESTORAGE_CONFIG_DIR"));
    Arc::new(ClaudeLogin::keychain(store.as_deref().map(std::path::Path::new)))
}

/// One agent: the three tools and its extensions (what it has seen of its artifacts).
struct Agent {
    tools: Vec<Arc<dyn eos_agent_core::Tool>>,
    ext: Extensions,
}

impl Agent {
    fn new(dir: PathBuf) -> Self {
        let ext = Extensions::default();
        ext.insert(Workspace::new(dir.clone(), Vec::new(), dir.join(".scratch")));
        Self { tools: tools(sign_in(), FrameConfig::default()).expect("tools"), ext }
    }

    async fn run(&self, tool: &str, input: &Value) -> (bool, String) {
        let tool = self.tools.iter().find(|t| t.spec().name() == tool).expect("one of the artifact tools");
        let raw = serde_json::value::to_raw_value(input).expect("raw");
        let cancel = CancellationToken::new();
        let ctx = ToolContext {
            agent_id: "live",
            tool_use_id: "live",
            cancel: &cancel,
            extensions: &self.ext,
            transcript: &[],
        };
        let out = tool.call(&raw, ctx).await;
        match out.content {
            ToolResultContent::Text(text) => (out.is_error, text),
            ToolResultContent::Blocks(_) => (out.is_error, "(blocks)".into()),
        }
    }
}

async fn raw(request: &Value) -> (u16, String) {
    let Ok(Credential::Bearer(token)) = sign_in().credential().await else { panic!("needs a Claude sign-in") };
    let method = reqwest::Method::from_bytes(request["method"].as_str().unwrap_or("GET").as_bytes()).expect("method");
    let path = request["path"].as_str().unwrap_or_default();
    let mut req = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client")
        .request(method, format!("{DEFAULT_BASE_URL}{path}"))
        .bearer_auth(&token)
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("x-frame-cp", "go")
        .header("x-frame-surface", "code")
        .header("accept", "application/json");
    // `asset_of`: sends that artifact's asset token (read from its boot, never
    // printed) as a header, and in place of `{asset_token}` in a full `url`.
    if let Some(slug) = request["asset_of"].as_str() {
        let boot: Value = reqwest::Client::new()
            .get(format!("{DEFAULT_BASE_URL}/api/frame/{slug}?via=model_read"))
            .bearer_auth(&token)
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("x-frame-cp", "go")
            .header("x-frame-surface", "code")
            .send()
            .await
            .expect("boot")
            .bytes()
            .await
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .expect("boot JSON");
        let asset = boot["assetToken"].as_str().expect("asset token").to_owned();
        if let Some(url) = request["url"].as_str() {
            let url = url.replace("{asset_token}", &asset);
            req = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client")
                .get(url);
        } else {
            req = req.header("x-frame-asset-token", asset);
        }
    }
    if let Some(body) = request.get("body") {
        req = req.header("content-type", "application/json").body(body.to_string());
    }
    let res = req.send().await.expect("sent");
    let status = res.status().as_u16();
    let headers: Vec<String> = res
        .headers()
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("x-frame") || *k == "content-type")
        .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("?")))
        .collect();
    (status, format!("{}\n{}", headers.join("\n"), res.text().await.unwrap_or_default()))
}

#[tokio::test]
#[ignore = "talks to the real frame API with a real sign-in"]
async fn lists_my_real_artifacts() {
    let agent = Agent::new(std::env::temp_dir());
    let (is_error, text) = agent.run("Artifact", &serde_json::json!({ "action": "list", "limit": 10 })).await;
    println!("{text}");
    assert!(!is_error, "{text}");
}

#[tokio::test]
#[ignore = "publishes, edits and deletes real artifacts"]
async fn one_call() {
    let call: Value = serde_json::from_str(&std::env::var("EOS_LIVE_CALL").expect("EOS_LIVE_CALL")).expect("JSON");
    if let Some(request) = call.get("raw") {
        let (status, body) = raw(request).await;
        println!("HTTP {status}\n{body}");
        return;
    }
    // A list runs as one agent, in order.
    let dir = std::env::var_os("EOS_LIVE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    // `"agent": "b"` runs a call as a second agent (a separate session).
    let agents = [Agent::new(dir.clone()), Agent::new(dir)];
    let calls = match call {
        Value::Array(calls) => calls,
        one => vec![one],
    };
    for call in calls {
        let tool = call["tool"].as_str().unwrap_or("Artifact");
        let agent = &agents[usize::from(call["agent"] == "b")];
        let (is_error, text) = agent.run(tool, &call["input"]).await;
        println!("== {tool} {}\nis_error: {is_error}\n{text}\n", call["input"]);
    }
}

/// Notes for one agent, printed and kept.
#[derive(Default)]
struct Notes(std::sync::Mutex<Vec<(String, String, bool)>>);

impl eos_agent_artifacts::WatchNotifier for Notes {
    fn notify<'a>(&'a self, agent: &'a str, text: String, wake: bool) -> eos_agent_core::BoxFuture<'a, bool> {
        println!("-- note to {agent} (wake: {wake}):\n{text}\n");
        self.0.lock().expect("notes").push((agent.to_owned(), text, wake));
        Box::pin(std::future::ready(true))
    }
}

/// Publishes a test page as a watching agent, republishes it as a second
/// client, waits for the watching agent's note, then deletes the page.
/// `EOS_LIVE_DIR` must hold `page.html` and `other.html`.
#[tokio::test]
#[ignore = "publishes and deletes a real artifact"]
async fn watch_live() {
    let dir = std::env::var_os("EOS_LIVE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let notes = Arc::new(Notes::default());
    let (watched, watcher) = eos_agent_artifacts::tools_watched(
        sign_in(),
        FrameConfig::default(),
        Arc::clone(&notes) as Arc<dyn eos_agent_artifacts::WatchNotifier>,
        eos_agent_artifacts::WatchConfig::default(),
    )
    .expect("tools");
    let a = Agent { tools: watched, ext: Extensions::default() };
    a.ext.insert(Workspace::new(dir.clone(), Vec::new(), dir.join(".scratch")));
    let b = Agent::new(dir);
    let publish =
        serde_json::json!({ "file_path": "page.html", "title": "eos-agent-sdk test — safe to delete", "icon": "test" });
    let (err, text) = a.run("Artifact", &publish).await;
    println!("{text}\n");
    assert!(!err);
    let url = text.split_whitespace().find(|w| w.starts_with("https://claude.ai/artifact/")).expect("url").to_owned();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !watcher.list("live").first().is_some_and(|r| r.connected) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let (_, listing) = a.run("ArtifactComments", &serde_json::json!({ "action": "watch" })).await;
    println!("{listing}\n");

    let (_, text) = b.run("Artifact", &serde_json::json!({ "action": "read", "url": url })).await;
    println!("second client read: {}\n", text.lines().next().unwrap_or_default());
    let (err, text) = b.run("Artifact", &serde_json::json!({ "file_path": "other.html", "url": url })).await;
    println!("second client published: {text}\n");
    assert!(!err);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while notes.0.lock().expect("notes").is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let got = notes.0.lock().expect("notes").clone();
    let (err, text) = a.run("Artifact", &serde_json::json!({ "action": "delete", "url": url })).await;
    println!("cleanup: {text}");
    assert!(!err);
    assert!(got.iter().any(|(_, t, wake)| t.contains("was published after the version you have") && !wake), "{got:?}");
}

/// The live channel's token lease against the real API: the watch boot says
/// when the token ends and whether it may be renewed, and the watch-token
/// endpoint renews it. Prints only field names and kinds, never tokens.
/// `EOS_LIVE_DIR` must hold `page.html`.
#[tokio::test]
#[ignore = "publishes and deletes a real artifact"]
async fn watch_token_lease_live() {
    let dir = std::env::var_os("EOS_LIVE_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let a = Agent::new(dir);
    let publish =
        serde_json::json!({ "file_path": "page.html", "title": "eos-agent-sdk test — safe to delete", "icon": "test" });
    let (err, text) = a.run("Artifact", &publish).await;
    assert!(!err, "{text}");
    let url = text.split_whitespace().find(|w| w.starts_with("https://claude.ai/artifact/")).expect("url").to_owned();
    let url = url.trim_end_matches(['.', ')', ',']).to_owned();
    let slug = eos_agent_artifacts::slug_from_url(&url).expect("slug");
    let shape = |body: &str| -> String {
        let json = body.split_once('\n').map_or(body, |(_, b)| b);
        let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
        let mut keys: Vec<String> = v
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| {
                        let kind = match v {
                            Value::Bool(b) => format!("bool {b}"),
                            Value::Number(_) => "number".into(),
                            Value::String(_) => "string".into(),
                            other => (if other.is_object() { "object" } else { "other" }).to_owned(),
                        };
                        format!("{k}: {kind}")
                    })
                    .collect()
            })
            .unwrap_or_default();
        keys.sort();
        keys.join(", ")
    };
    let boot =
        serde_json::json!({ "method": "GET", "path": format!("/api/frame/{slug}?via=model_read&bk=probe&vn=passive") });
    let (status, body) = raw(&boot).await;
    println!("watch boot: HTTP {status}\n  {}", shape(&body));
    let renew = serde_json::json!({ "method": "POST", "path": format!("/api/frame/watch-token/{slug}?vn=passive"), "body": {} });
    let (status, body) = raw(&renew).await;
    println!("renewal: HTTP {status}\n  {}", shape(&body));
    let (err, text) = a.run("Artifact", &serde_json::json!({ "action": "delete", "url": url })).await;
    println!("cleanup: {text}");
    assert!(!err);
}
