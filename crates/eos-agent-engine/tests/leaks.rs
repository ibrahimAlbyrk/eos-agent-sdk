//! Leak gate: repeated engine lifecycles (100 agents × 3 turns each, then
//! everything dropped) must not leave memory behind, and an idle agent must
//! stay under 16 KB. Runs alone in its own binary so the counters see only this.

use std::sync::Arc;
use std::time::Duration;

use eos_agent_anthropic::auth::{Credential, StaticCredential};
use eos_agent_anthropic::{AnthropicClient, ClientConfig};
use eos_agent_core::{AgentConfig, StoredMessage, SystemPrompt, Toolbox};
use eos_agent_engine::{AgentSpec, Engine};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_testkit::{AllocStats, CountingAlloc, slope};
use eos_agent_types::{ContentBlock, Role};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const AGENTS: usize = 100;
const TURNS: usize = 3;

async fn cycle(mock: &MockApi) {
    for _ in 0..AGENTS * TURNS {
        mock.push(Reply::text("ok"));
    }
    let auth = Arc::new(StaticCredential(Credential::ApiKey("k".into())));
    let client = AnthropicClient::new(auth, ClientConfig { base_url: mock.base_url(), ..ClientConfig::default() })
        .expect("client");
    let engine = Engine::builder(Arc::new(client)).build();
    let toolbox = Arc::new(Toolbox::default());
    let agents: Vec<_> = (0..AGENTS)
        .map(|_| {
            let spec = AgentSpec::new(AgentConfig::new("mock"), SystemPrompt::new(&[("sys", None)]), toolbox.clone());
            engine.spawn(spec).0
        })
        .collect();
    for _ in 0..TURNS {
        for r in futures_util::future::join_all(agents.iter().map(|a| a.send_text("hello"))).await {
            r.expect("alive").expect("turn");
        }
    }
    drop(agents);
    drop(engine);
    mock.clear_requests();
    // Agent tasks end once their command channels close.
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Bytes one idle agent holds: spawned with a short recorded conversation
/// (a question and an answer), waiting for its next turn. No network involved.
async fn idle_agent_bytes(mock: &MockApi) -> u64 {
    const N: u64 = 1000;
    let auth = Arc::new(StaticCredential(Credential::ApiKey("k".into())));
    let client = AnthropicClient::new(auth, ClientConfig { base_url: mock.base_url(), ..ClientConfig::default() })
        .expect("client");
    let engine = Engine::builder(Arc::new(client)).build();
    let toolbox = Arc::new(Toolbox::default());
    let system = SystemPrompt::new(&[("sys", None)]);
    let history = vec![
        StoredMessage::user_text("What is the capital of France?"),
        StoredMessage::new(Role::Assistant, &[ContentBlock::Text { text: "Paris.".into(), citations: None }])
            .expect("message"),
    ];
    let before = AllocStats::now().live_bytes;
    let agents: Vec<_> = (0..N)
        .map(|_| {
            let mut spec = AgentSpec::new(AgentConfig::new("mock"), system.clone(), toolbox.clone());
            spec.history = history.clone();
            engine.spawn(spec)
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(50)).await; // let every task park on its inbox
    let per_agent = AllocStats::now().live_bytes.saturating_sub(before) / N;
    drop(agents);
    tokio::time::sleep(Duration::from_millis(50)).await;
    per_agent
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_lifecycles_leave_nothing_behind() {
    let mock = MockApi::start([]).await.expect("mock");
    let idle = idle_agent_bytes(&mock).await;
    cycle(&mock).await; // the first cycle builds pools and lazy statics
    let mut live = Vec::new();
    for _ in 0..5 {
        cycle(&mock).await;
        live.push(AllocStats::now().live_bytes);
    }
    let growth = slope(&live);
    println!("idle agent: {idle} bytes; live bytes per cycle: {live:?}; slope {growth:.0} B/cycle");
    assert!(growth.abs() < 16.0 * 1024.0, "memory grows by {growth:.0} bytes per cycle");
    assert!(idle < 16 * 1024, "an idle agent holds {idle} bytes");
}
