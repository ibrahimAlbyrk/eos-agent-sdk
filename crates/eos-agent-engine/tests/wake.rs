//! A note from an agent's own background work wakes it once it is idle, and
//! only for notes no request has read.

use std::sync::Arc;
use std::time::Duration;

use eos_agent_anthropic::auth::{Credential, StaticCredential};
use eos_agent_anthropic::{AnthropicClient, ClientConfig};
use eos_agent_core::{AgentConfig, SystemPrompt, TaskNotices, Toolbox};
use eos_agent_engine::{AgentSpec, Engine};
use eos_agent_mock_api::{MockApi, Recorded, Reply};

fn engine(mock: &MockApi) -> Engine {
    let auth = Arc::new(StaticCredential(Credential::ApiKey("k".into())));
    let model = AnthropicClient::new(auth, ClientConfig { base_url: mock.base_url(), ..ClientConfig::default() })
        .expect("client");
    Engine::builder(Arc::new(model)).build()
}

fn spec() -> AgentSpec {
    AgentSpec::new(AgentConfig::new("mock-model"), SystemPrompt::new(&[("sys", None)]), Arc::new(Toolbox::default()))
}

/// The text of a request's last message (what the turn added).
fn last_message(request: &Recorded) -> String {
    let json = request.json();
    json["messages"].as_array().and_then(|m| m.last()).map(ToString::to_string).unwrap_or_default()
}

fn count(requests: &[Recorded], needle: &str) -> usize {
    requests.iter().filter(|r| last_message(r).contains(needle)).count()
}

#[tokio::test]
async fn notes_arriving_together_wake_an_idle_agent_once() {
    let mock = MockApi::start([Reply::text("woke"), Reply::text("extra")]).await.expect("mock");
    let engine = engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    let notices = TaskNotices::of(agent.extensions());
    for n in ["NOTE-1", "NOTE-2", "NOTE-3"] {
        notices.push(n);
    }
    agent.wait_idle().await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "one turn for the three notes");
    assert!(["NOTE-1", "NOTE-2", "NOTE-3"].iter().all(|n| last_message(&requests[0]).contains(n)));
}

#[tokio::test]
async fn a_note_read_by_the_running_turn_starts_no_other() {
    // The turn makes a second request (its tool call fails), which reads the note.
    let mock = MockApi::start([
        Reply::tool_use("toolu_1", "Missing", "{}").slowed(Duration::from_millis(300)),
        Reply::text("done"),
        Reply::text("extra"),
    ])
    .await
    .expect("mock");
    let engine = engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    let notices = TaskNotices::of(agent.extensions());
    let turn = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.send_text("work").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    notices.push("NOTE-MID");
    turn.await.expect("join").expect("alive").expect("turn");
    agent.wait_idle().await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "no turn after the one that read the note");
    assert_eq!(count(&requests, "NOTE-MID"), 1);
}

#[tokio::test]
async fn a_note_that_came_after_the_last_request_wakes_the_agent_after_its_turn() {
    let mock = MockApi::start([Reply::text("done").slowed(Duration::from_millis(300)), Reply::text("woke")])
        .await
        .expect("mock");
    let engine = engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    let notices = TaskNotices::of(agent.extensions());
    let turn = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.send_text("work").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    notices.push("NOTE-LATE");
    turn.await.expect("join").expect("alive").expect("turn");
    agent.wait_idle().await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(last_message(&requests[1]).contains("NOTE-LATE"));
}

#[tokio::test]
async fn a_message_sent_with_a_wake_is_its_own_turn_and_nothing_is_lost() {
    let mock = MockApi::start([Reply::text("one"), Reply::text("two"), Reply::text("extra")]).await.expect("mock");
    let engine = engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    TaskNotices::of(agent.extensions()).push("NOTE-RACE");
    agent.send_text("MESSAGE").await.expect("alive").expect("turn");
    agent.wait_idle().await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2, "the wake's turn, then the message's");
    assert_eq!(count(&requests, "NOTE-RACE"), 1);
    assert_eq!(count(&requests, "MESSAGE"), 1);

    // Racing: whichever command comes first, the note is read once and no
    // turn runs on nothing.
    let mock = MockApi::start([Reply::text("one"), Reply::text("two"), Reply::text("extra")]).await.expect("mock");
    let engine = self::engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    let notices = TaskNotices::of(agent.extensions());
    let turn = {
        let agent = agent.clone();
        tokio::spawn(async move { agent.send_text("MESSAGE").await })
    };
    notices.push("NOTE-RACE");
    turn.await.expect("join").expect("alive").expect("turn");
    agent.wait_idle().await;
    let requests = mock.requests();
    assert_eq!(count(&requests, "NOTE-RACE"), 1, "read exactly once");
    assert_eq!(count(&requests, "MESSAGE"), 1);
    assert!(requests.iter().all(|r| ["NOTE-RACE", "MESSAGE"].iter().any(|n| last_message(r).contains(n))));
}

#[tokio::test]
async fn an_agent_that_is_gone_is_not_woken() {
    let mock = MockApi::start([Reply::text("never")]).await.expect("mock");
    let engine = engine(&mock);
    let (agent, _events) = engine.spawn(spec());
    let notices = TaskNotices::of(agent.extensions());
    drop(agent);
    tokio::time::sleep(Duration::from_millis(50)).await;
    notices.push("NOTE-AFTER");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(mock.requests().is_empty());
}
