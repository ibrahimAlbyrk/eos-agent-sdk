use std::sync::Arc;
use std::time::Duration;

use eos_agent_anthropic::auth::{Credential, StaticCredential};
use eos_agent_anthropic::{AnthropicClient, ClientConfig};
use eos_agent_core::{AgentConfig, AgentEvent, SystemPrompt, Toolbox, TurnOutcome};
use eos_agent_engine::{AgentSpec, Engine, JsonlStore, LocalTurn};
use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_types::{Role, StopReason};

fn model(mock: &MockApi) -> Arc<AnthropicClient> {
    let auth = Arc::new(StaticCredential(Credential::ApiKey("k".into())));
    Arc::new(
        AnthropicClient::new(auth, ClientConfig { base_url: mock.base_url(), ..ClientConfig::default() })
            .expect("client"),
    )
}

fn spec() -> AgentSpec {
    AgentSpec::new(AgentConfig::new("mock-model"), SystemPrompt::new(&[("sys", None)]), Arc::new(Toolbox::default()))
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("eos-agent-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn runs_turns_streams_events_and_resumes_from_the_store() {
    let mock = MockApi::start([Reply::text("first"), Reply::text("second")]).await.expect("mock");
    let dir = temp_dir("store");
    let store = Arc::new(JsonlStore::new(&dir));
    let engine = Engine::builder(model(&mock)).store(store.clone()).build();

    let (agent, mut events) = engine.spawn(spec());
    let out = agent.send_text("hi").await.expect("alive").expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    let mut text = String::new();
    while let Ok(e) = events.try_recv() {
        if let AgentEvent::TextDelta { text: t } = e {
            text.push_str(&t);
        }
    }
    assert_eq!(text, "first");

    // A new agent with the recorded history continues the same conversation.
    let history = store.load(agent.id()).await.expect("load");
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].role(), Role::Assistant);
    let mut resumed = spec();
    resumed.id = Some(agent.id().to_owned());
    resumed.history = history;
    let (again, _) = engine.spawn(resumed);
    again.send_text("more").await.expect("alive").expect("turn");
    assert_eq!(mock.requests()[1].json()["messages"].as_array().map(Vec::len), Some(3));
    assert_eq!(store.load(agent.id()).await.expect("load").len(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn interrupt_stops_a_running_turn() {
    let mock = MockApi::start([Reply::text("slow answer").slowed(Duration::from_millis(200))]).await.expect("mock");
    let engine = Engine::builder(model(&mock)).build();
    let (agent, _events) = engine.spawn(spec());
    let agent = Arc::new(agent);
    let runner = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.send_text("hi").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    agent.interrupt();
    let out = runner.await.expect("join").expect("alive").expect("turn");
    assert_eq!(out, TurnOutcome::Interrupted);
}

#[tokio::test]
async fn a_local_turn_records_its_input_and_the_next_request_carries_it() {
    let mock = MockApi::start([Reply::text("ok")]).await.expect("mock");
    let engine = Engine::builder(model(&mock)).build();
    let (agent, _events) = engine.spawn(spec());
    let local = |text: &'static str| -> eos_agent_engine::LocalWork {
        Box::new(move |_| {
            let input = vec![eos_agent_types::ContentBlock::Text { text: text.into(), citations: None }];
            Box::pin(async move { LocalTurn { input, interrupted: false } })
        })
    };
    let out = agent.run_local(local("/cmd")).await.expect("alive").expect("turn");
    assert_eq!(out, TurnOutcome::Finished(StopReason::EndTurn));
    assert!(mock.requests().is_empty(), "no model request");
    agent.send_text("next").await.expect("alive").expect("turn");
    let messages = mock.requests()[0].json()["messages"].to_string();
    assert!(messages.contains("/cmd") && messages.contains("next"), "{messages}");

    // An interrupt cancels the work, and the turn is noted as interrupted.
    let slow: eos_agent_engine::LocalWork = Box::new(|cancel| {
        Box::pin(async move {
            cancel.cancelled().await;
            let input = vec![eos_agent_types::ContentBlock::Text { text: "/slow".into(), citations: None }];
            LocalTurn { input, interrupted: true }
        })
    });
    let agent = Arc::new(agent);
    let runner = {
        let agent = Arc::clone(&agent);
        tokio::spawn(async move { agent.run_local(slow).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!agent.is_idle(), "the local work is the agent's turn");
    agent.interrupt();
    let out = runner.await.expect("join").expect("alive").expect("turn");
    assert_eq!(out, TurnOutcome::Interrupted);
}

/// The body bytes of every request for two turns of one agent.
async fn two_turns(engine: &Engine, mock: &MockApi, pause: Duration) -> (Vec<String>, bool) {
    mock.push(Reply::text("first"));
    mock.push(Reply::text("second"));
    mock.clear_requests();
    let (agent, _events) = engine.spawn(spec());
    agent.send_text("hi").await.expect("alive").expect("turn");
    tokio::time::sleep(pause).await;
    let evicted = agent.is_evicted();
    agent.send_text("again").await.expect("alive").expect("turn");
    assert!(!agent.is_evicted(), "the turn paged the history back in");
    let bodies = mock.requests().iter().map(|r| String::from_utf8_lossy(&r.body).into_owned()).collect();
    (bodies, evicted)
}

#[tokio::test]
async fn idle_agents_page_their_history_out_and_back_unchanged() {
    let mock = MockApi::start([]).await.expect("mock");
    let dir = temp_dir("evict");
    let store = Arc::new(JsonlStore::new(&dir));

    let kept = Engine::builder(model(&mock)).store(store.clone()).idle_eviction(None).build();
    let (expected, evicted) = two_turns(&kept, &mock, Duration::from_millis(150)).await;
    assert!(!evicted);

    let evicting =
        Engine::builder(model(&mock)).store(store.clone()).idle_eviction(Some(Duration::from_millis(30))).build();
    let (got, evicted) = two_turns(&evicting, &mock, Duration::from_millis(150)).await;
    assert!(evicted, "an idle agent drops its history");
    assert_eq!(got, expected, "the conversation is byte-for-byte the same");

    // Without a store there is nowhere to page to: never evict.
    let no_store = Engine::builder(model(&mock)).idle_eviction(Some(Duration::from_millis(30))).build();
    let (_, evicted) = two_turns(&no_store, &mock, Duration::from_millis(150)).await;
    assert!(!evicted);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_history_given_at_spawn_is_written_before_it_is_evicted() {
    let mock = MockApi::start([Reply::text("third")]).await.expect("mock");
    let dir = temp_dir("evict-fork");
    let store = Arc::new(JsonlStore::new(&dir));
    let engine =
        Engine::builder(model(&mock)).store(store.clone()).idle_eviction(Some(Duration::from_millis(20))).build();
    let mut seeded = spec();
    seeded.history = vec![
        eos_agent_core::StoredMessage::user_text("one"),
        eos_agent_core::StoredMessage::new(
            Role::Assistant,
            &[eos_agent_types::ContentBlock::Text { text: "two".into(), citations: None }],
        )
        .expect("message"),
    ];
    let (agent, _events) = engine.spawn(seeded);
    agent.send_text("three").await.expect("alive").expect("turn");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(agent.is_evicted());
    assert_eq!(store.load(agent.id()).await.expect("load").len(), 4);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn compact_replaces_the_conversation_on_request() {
    let mock = MockApi::start([Reply::text("hello"), Reply::text("the summary")]).await.expect("mock");
    let engine = Engine::builder(model(&mock)).build();
    let mut with_compaction = spec();
    with_compaction.config.compaction =
        Some(eos_agent_core::Compaction { threshold_tokens: Some(u64::MAX), text: Arc::new(Words) });
    let (agent, _events) = engine.spawn(with_compaction);
    agent.send_text("hi").await.expect("alive").expect("turn");
    assert!(agent.compact(Some("keep names".into())).await.expect("alive").expect("compacted"));
    let asked = mock.requests()[1].json()["messages"].to_string();
    assert!(asked.contains("summarize; focus: keep names"), "{asked}");
}

#[derive(Debug)]
struct Words;

impl eos_agent_core::CompactionText for Words {
    fn instruction(&self, focus: Option<&str>) -> String {
        format!("summarize; focus: {}", focus.unwrap_or("-"))
    }

    fn seed(&self, summary: &str) -> String {
        format!("Earlier: {summary}")
    }
}

#[tokio::test]
async fn an_agent_is_reconfigured_and_rewound_between_turns_even_when_evicted() {
    let mock = MockApi::start([Reply::text("first"), Reply::text("second"), Reply::text("third")]).await.expect("mock");
    let dir = temp_dir("rewind");
    let store = Arc::new(JsonlStore::new(&dir));
    let engine =
        Engine::builder(model(&mock)).store(store.clone()).idle_eviction(Some(Duration::from_millis(30))).build();
    let (agent, _events) = engine.spawn(spec());
    agent.send_text("one").await.expect("alive").expect("turn");
    agent.send_text("two").await.expect("alive").expect("turn");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(agent.is_evicted());

    // Read back from the store, without paging the history in.
    let history = agent.transcript().await.expect("alive").expect("read");
    assert_eq!(history.len(), 4);
    assert!(agent.is_evicted());

    agent.rewind(2).await.expect("alive").expect("rewound");
    assert_eq!(store.load(agent.id()).await.expect("load").len(), 2);
    let mut config = AgentConfig::new("other-model");
    config.max_tokens = 1234;
    let update = eos_agent_engine::AgentUpdate {
        config: Some(config),
        system: Some(SystemPrompt::new(&[("new sys", None)])),
        toolbox: None,
    };
    agent.reconfigure(update).await.expect("alive");
    agent.send_text("two again").await.expect("alive").expect("turn");

    let body = mock.requests()[2].json();
    assert_eq!(body["model"], "other-model");
    assert_eq!(body["max_tokens"], 1234);
    assert_eq!(body["system"][0]["text"], "new sys");
    let messages = body["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3, "the first turn, then the new message");
    assert!(body["messages"].to_string().contains("two again") && !body["messages"].to_string().contains("second"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The host's seed in place of the conversation is recorded at once: a
/// session resumed before anyone wrote again opens with it, then with the
/// session context and the next message.
#[tokio::test]
async fn a_hosts_seed_is_recorded_and_a_resumed_session_opens_after_it() {
    let mock = MockApi::start([Reply::text("first"), Reply::text("second")]).await.expect("mock");
    let dir = temp_dir("seed");
    let store = Arc::new(JsonlStore::new(&dir));
    let engine = Engine::builder(model(&mock)).store(store.clone()).build();
    let context = || eos_agent_core::SessionContext {
        user_prefix: vec![eos_agent_types::ContentBlock::Text { text: "CTX".into(), citations: None }].into(),
        system_notes: vec![eos_agent_core::StoredMessage::system_text("ENV")],
        ..Default::default()
    };
    let mut first = spec();
    first.session = Some(context());
    let (agent, _events) = engine.spawn(first);
    agent.send_text("hi").await.expect("alive").expect("turn");
    let seed = eos_agent_types::ContentBlock::Text { text: "Earlier: SUMMARY".into(), citations: None };
    agent.replace_context(vec![seed]).await.expect("alive").expect("replaced");

    let history = store.load(agent.id()).await.expect("load");
    assert_eq!(history.len(), 1);
    assert!(history[0].is_meta() && history[0].text() == "Earlier: SUMMARY");
    let mut resumed = spec();
    resumed.id = Some(agent.id().to_owned());
    resumed.history = history;
    resumed.session = Some(context());
    let (again, _) = engine.spawn(resumed);
    again.send_text("go on").await.expect("alive").expect("turn");
    let sent = mock.requests()[1].json()["messages"].clone();
    let texts: Vec<String> = sent
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["content"].as_array().expect("blocks").iter().filter_map(|b| b["text"].as_str()).collect())
        .collect();
    assert_eq!(texts, ["Earlier: SUMMARY", "CTXgo on", "ENV"]);
    let _ = std::fs::remove_dir_all(&dir);
}
