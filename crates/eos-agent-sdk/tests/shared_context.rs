//! Agents with the same session context hold one copy of it, and the model
//! still receives the same bytes. Runs alone in its own binary so the
//! counters see only this.

use std::sync::Arc;

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_sdk::{AgentOptions, Sdk};
use eos_agent_testkit::{AllocStats, CountingAlloc};
use serde_json::Value;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const AGENTS: usize = 200;
/// A large CLAUDE.md, as real projects have.
const INSTRUCTIONS: usize = 40 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_share_one_copy_of_their_context() {
    let root = std::env::temp_dir().join(format!("eos-sdk-shared-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["home", "project", "data"] {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
    }
    let root = root.canonicalize().expect("canonical");
    let line = "Keep functions small and name things for what they mean.\n";
    std::fs::write(root.join("project/CLAUDE.md"), line.repeat(INSTRUCTIONS / line.len())).expect("write");
    for n in 0..40 {
        let dir = root.join(format!("home/.claude/skills/skill-{n}"));
        std::fs::create_dir_all(&dir).expect("dir");
        let body =
            format!("---\nname: skill-{n}\ndescription: Does task number {n} {}\n---\nSteps.", "well ".repeat(40));
        std::fs::write(dir.join("SKILL.md"), body).expect("write");
    }
    let mock = MockApi::start([Reply::text("one"), Reply::text("two")]).await.expect("mock");
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("claude-haiku-5-5")
        .claude_paths(ClaudePaths::for_home(&root.join("home")))
        .data_dir(root.join("data"))
        .build()
        .await
        .expect("sdk");
    let options = AgentOptions::new(root.join("project")).mcp(false);

    // The first agent builds the context; the second gets the shared copy.
    let (first, _) = sdk.spawn(options.clone()).await.expect("spawn");
    let (second, _) = sdk.spawn(options.clone()).await.expect("spawn");
    first.send_text("hello").await.expect("alive").expect("turn");
    second.send_text("hello").await.expect("alive").expect("turn");
    let requests = mock.requests();
    let messages = |i: usize| requests[i].json()["messages"].as_array().cloned().expect("messages");
    let (built, shared) = (messages(0), messages(1));
    assert_eq!(built, shared, "a shared context reaches the model unchanged");
    assert_eq!(built[1]["role"], "system");
    let note = built[1]["content"][0]["text"].as_str().expect("note");
    assert!(note.contains("- skill-39: Does task number 39") && note.contains("# Environment"));
    assert!(text_of(&built[0]).contains("Keep functions small"));
    drop((first, second));

    let before = AllocStats::now().live_bytes as i64;
    let mut agents = Vec::with_capacity(AGENTS);
    for _ in 0..AGENTS {
        agents.push(sdk.spawn(options.clone()).await.expect("spawn").0);
    }
    let idle = (AllocStats::now().live_bytes as i64 - before) / AGENTS as i64;
    println!("idle agent with a {INSTRUCTIONS} B CLAUDE.md and a {} B note: {idle} B", note.len());
    assert!(idle < 20 * 1024, "an idle agent holds {idle} bytes: its context is not shared");
    drop(agents);
    let _ = std::fs::remove_dir_all(&root);
}

fn text_of(message: &Value) -> String {
    message["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect()
}
