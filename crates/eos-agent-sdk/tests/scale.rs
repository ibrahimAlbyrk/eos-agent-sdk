//! Always-on scale gate: 200 agents doing tool-using turns, spawned and
//! dropped again and again, must leave nothing behind, and an idle agent must
//! stay small. Runs alone in its own binary so the counters see only this.
//! The big runs are `tools/scale-bench`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use eos_agent_compat::ClaudePaths;
use eos_agent_core::{Credential, StaticCredential};
use eos_agent_mock_api::{MockApi, read_then_answer};
use eos_agent_sdk::{AgentOptions, Sdk};
use eos_agent_testkit::{AllocStats, CountingAlloc, slope};
use futures_util::future::join_all;

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const AGENTS: usize = 200;
const PROMPT: &str = "Read small.txt, then say done.";

/// Spawns the agents, runs `turns` rounds, and returns the heap bytes per
/// agent while they sit idle, before dropping them.
async fn cycle(sdk: &Sdk, options: &AgentOptions, turns: usize) -> i64 {
    let before = AllocStats::now().live_bytes as i64;
    let mut agents = Vec::with_capacity(AGENTS);
    for _ in 0..AGENTS {
        agents.push(sdk.spawn(options.clone()).await.expect("spawn").0);
    }
    for _ in 0..turns {
        for result in join_all(agents.iter().map(|a| a.send_text(PROMPT))).await {
            result.expect("alive").expect("turn");
        }
    }
    let idle = (AllocStats::now().live_bytes as i64 - before) / AGENTS as i64;
    drop(agents);
    tokio::time::sleep(Duration::from_millis(50)).await;
    idle
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_agents_leave_nothing_behind() {
    let root = std::env::temp_dir().join(format!("eos-sdk-scale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["home", "project", "data"] {
        std::fs::create_dir_all(root.join(dir)).expect("dir");
    }
    let root = root.canonicalize().expect("canonical");
    let file = root.join("project/small.txt");
    std::fs::write(&file, "three\nshort\nlines\n").expect("write");
    let mock =
        MockApi::responder(SocketAddr::from(([127, 0, 0, 1], 0)), read_then_answer(file.to_str().expect("utf8")))
            .await
            .expect("mock");
    let sdk = Sdk::builder()
        .auth(Arc::new(StaticCredential(Credential::ApiKey("k".into()))))
        .base_url(mock.base_url())
        .http2_prior_knowledge(true)
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&root.join("home")))
        .data_dir(root.join("data"))
        .build()
        .await
        .expect("sdk");
    let options = AgentOptions::new(root.join("project")).mcp(false);

    cycle(&sdk, &options, 2).await; // fills pools, caches and lazy statics
    let mut live = Vec::new();
    let mut idle = 0;
    for _ in 0..8 {
        idle = cycle(&sdk, &options, 2).await;
        live.push(AllocStats::now().live_bytes);
    }
    println!("idle agent after 2 turns: {idle} B; live bytes per cycle: {live:?}; slope {:.0} B/cycle", slope(&live));
    // A leak adds to every cycle; pools and HTTP/2 stream tables step up to
    // a high-water mark now and then and stay. So at least one of the last
    // cycles must have added (next to) nothing.
    let deltas: Vec<i64> = live.windows(2).map(|w| w[1] as i64 - w[0] as i64).collect();
    let least = deltas.iter().rev().take(3).min().copied().unwrap_or(0);
    assert!(least < 8 * 1024, "memory grows by {least}+ bytes per cycle of {AGENTS} agents: {live:?}");
    assert!(idle < 48 * 1024, "an idle agent holds {idle} bytes");
    assert_eq!(sdk.agent_count(), 0);
    let _ = std::fs::remove_dir_all(&root);
}
