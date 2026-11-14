//! Agents given the same host tools hold one serialized tool list between
//! them, and what a host seeds an agent with is freed with the agent. Runs
//! alone in its own binary so the counters see only this.

use std::sync::{Arc, Weak};

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{BoxFuture, Credential, Extensions, StaticCredential, Tool, ToolContext, ToolOutput, ToolSpec};
use eos_agent_mock_api::MockApi;
use eos_agent_sdk::{AgentOptions, HostTool, Sdk};
use eos_agent_testkit::{AllocStats, CountingAlloc};
use serde_json::value::RawValue;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const AGENTS: usize = 200;
/// A long description, as a tool that documents a whole catalog has.
const DESCRIPTION: usize = 64 * 1024;

struct Described(ToolSpec);

impl Tool for Described {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }

    fn call<'a>(&'a self, _input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async { ToolOutput::text("ok") })
    }
}

/// The host's state for one session.
struct Session(#[expect(dead_code, reason = "held, to be freed")] Vec<u8>);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equal_host_tool_sets_share_one_tool_list_and_seeds_are_freed() {
    let root = std::env::temp_dir().join(format!("eos-sdk-host-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["home", "project", "data"] {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
    }
    let root = root.canonicalize().expect("canonical");
    let mock = MockApi::start([]).await.expect("mock");
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .model("claude-haiku-5-5")
        .claude_paths(ClaudePaths::for_home(&root.join("home")))
        .data_dir(root.join("data"))
        .build()
        .await
        .expect("sdk");
    let spec = ToolSpec::new("mcp__worker__present", &"catalog ".repeat(DESCRIPTION / 8), serde_json::json!({}));
    let tools = vec![HostTool::new(Arc::new(Described(spec)))];
    let spawn = |seed: Extensions| {
        let options = AgentOptions::new(root.join("project")).mcp(false).tools(tools.clone()).extensions(seed);
        let sdk = sdk.clone();
        async move { sdk.spawn(options).await.expect("spawn").0 }
    };

    // The first agent builds the tool list; the rest share it.
    let first = spawn(Extensions::default()).await;
    let before = AllocStats::now().live_bytes as i64;
    let mut agents = Vec::with_capacity(AGENTS);
    let mut sessions: Vec<Weak<Session>> = Vec::with_capacity(AGENTS);
    for _ in 0..AGENTS {
        let seed = Extensions::default();
        seed.insert(Session(vec![0; 256]));
        sessions.push(Arc::downgrade(&seed.get::<Session>().expect("seeded")));
        agents.push(spawn(seed).await);
    }
    let idle = (AllocStats::now().live_bytes as i64 - before) / AGENTS as i64;
    println!("idle agent with a {DESCRIPTION} B host tool description: {idle} B");
    assert!(idle < 20 * 1024, "an idle agent holds {idle} bytes: its tool list is not shared");

    drop(agents);
    // Agents end on their own tasks, a moment after their handles are gone.
    for _ in 0..100 {
        if sessions.iter().all(|s| s.strong_count() == 0) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let alive = sessions.iter().filter(|s| s.strong_count() > 0).count();
    assert_eq!(alive, 0, "a seed outlived its agent");
    let left = (AllocStats::now().live_bytes as i64 - before) / AGENTS as i64;
    assert!(left < 512, "{left} bytes per agent are still held after the agents ended");
    drop(first);
    let _ = std::fs::remove_dir_all(&root);
}
