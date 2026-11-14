//! Credentials per agent: an agent's own account, the host's say when it
//! stops working, and carrying on the turn that failed.

mod common;

use std::sync::{Arc, Mutex};

use common::{Dirs, agent, mock, sdk};
use eos_agent_compat::ClaudePaths;
use eos_agent_core::{BoxFuture, ModelError, TokenSource, TurnError, TurnOutcome};
use eos_agent_mock_api::{MockApi, Recorded, Reply};
use eos_agent_sdk::{Account, AgentOptions, AuthFailover, AuthFailure, Sdk};
use eos_agent_types::StopReason;

fn key_of(request: &Recorded) -> String {
    request.headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned()
}

/// Claude's answer when a plan's limit is reached.
fn limit_reached() -> Reply {
    Reply::Status {
        status: 429,
        headers: vec![
            ("anthropic-ratelimit-unified-status".into(), "rejected".into()),
            ("anthropic-ratelimit-unified-reset".into(), "1794416400".into()),
            ("anthropic-ratelimit-unified-representative-claim".into(), "five_hour".into()),
        ],
        body: r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#.into(),
    }
}

fn signed_out() -> Reply {
    Reply::Status {
        status: 401,
        headers: Vec::new(),
        body: r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth token has expired"}}"#.into(),
    }
}

/// Hands out its accounts in order and remembers what it was asked.
#[derive(Default)]
struct Spare {
    accounts: Mutex<Vec<Arc<dyn TokenSource>>>,
    asked: Mutex<Vec<(String, AuthFailure)>>,
}

impl Spare {
    fn with(keys: &[&str]) -> Arc<Self> {
        let accounts = keys.iter().rev().map(|k| Arc::new(Account::api_key(*k)) as Arc<dyn TokenSource>).collect();
        Arc::new(Self { accounts: Mutex::new(accounts), asked: Mutex::default() })
    }

    fn asked(&self) -> Vec<(String, AuthFailure)> {
        self.asked.lock().expect("lock").clone()
    }
}

impl AuthFailover for Spare {
    fn replacement<'a>(
        &'a self,
        agent_id: &'a str,
        failure: &'a AuthFailure,
    ) -> BoxFuture<'a, Option<Arc<dyn TokenSource>>> {
        self.asked.lock().expect("lock").push((agent_id.to_owned(), failure.clone()));
        let next = self.accounts.lock().expect("lock").pop();
        Box::pin(async move { next })
    }
}

async fn sdk_with_failover(mock: &MockApi, dirs: &Dirs, failover: Arc<dyn AuthFailover>) -> Sdk {
    Sdk::builder()
        .auth(Arc::new(Account::api_key("k")))
        .auth_failover(failover)
        .base_url(mock.base_url())
        .model("mock-model")
        .claude_paths(ClaudePaths::for_home(&dirs.home()))
        .data_dir(dirs.root.join("data"))
        .build()
        .await
        .expect("sdk")
}

#[tokio::test]
async fn each_agent_runs_on_the_credential_it_was_given() {
    let dirs = Dirs::new("accounts-own");
    let mock = mock([Reply::text("one"), Reply::text("two"), Reply::text("three")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let own = Arc::new(Account::api_key("own-key"));
    let options = AgentOptions::new(dirs.project()).mcp(false).auth(Arc::clone(&own) as Arc<dyn TokenSource>);
    let (with_own, _events) = sdk.spawn(options).await.expect("spawn");
    let (with_default, _events) = agent(&sdk, &dirs).await;

    with_own.send_text("hi").await.expect("alive").expect("turn");
    with_default.send_text("hi").await.expect("alive").expect("turn");
    with_own.set_auth(Arc::new(Account::api_key("later-key")));
    with_own.send_text("again").await.expect("alive").expect("turn");

    let keys: Vec<String> = mock.requests().iter().map(key_of).collect();
    assert_eq!(keys, ["own-key", "k", "later-key"]);
}

#[tokio::test]
async fn a_reached_limit_asks_the_host_and_the_request_goes_again_on_its_answer() {
    let dirs = Dirs::new("accounts-limit");
    let mock = mock([limit_reached(), limit_reached(), Reply::text("done")]).await;
    let spare = Spare::with(&["second", "third"]);
    let sdk = sdk_with_failover(&mock, &dirs, Arc::clone(&spare) as Arc<dyn AuthFailover>).await;
    let first = Arc::new(Account::api_key("first"));
    let options = AgentOptions::new(dirs.project()).mcp(false).auth(Arc::clone(&first) as Arc<dyn TokenSource>);
    let (agent, _events) = sdk.spawn(options).await.expect("spawn");

    let outcome = agent.send_text("hi").await.expect("alive").expect("the turn carries on");
    assert_eq!(outcome, TurnOutcome::Finished(StopReason::EndTurn));

    let requests = mock.requests();
    assert_eq!(requests.iter().map(key_of).collect::<Vec<_>>(), ["first", "second", "third"]);
    assert_eq!(requests[0].body, requests[2].body, "the same request, on another credential");
    let asked = spare.asked();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0].0, agent.id());
    assert!(matches!(&asked[0].1, AuthFailure::LimitReached(info) if info.resets_at == Some(1_794_416_400)));
    // The account that ran out knows until when.
    let limits = first.rate_limit().expect("reported");
    assert_eq!(limits.status.as_deref(), Some("rejected"));
    assert_eq!(limits.claim.as_deref(), Some("five_hour"));
}

#[tokio::test]
async fn a_sign_in_that_ran_out_asks_the_host_too() {
    let dirs = Dirs::new("accounts-expired");
    let mock = mock([signed_out(), Reply::text("done")]).await;
    let spare = Spare::with(&["fresh"]);
    let sdk = sdk_with_failover(&mock, &dirs, Arc::clone(&spare) as Arc<dyn AuthFailover>).await;
    let (agent, _events) = agent(&sdk, &dirs).await;

    agent.send_text("hi").await.expect("alive").expect("the turn carries on");

    assert_eq!(mock.requests().iter().map(key_of).collect::<Vec<_>>(), ["k", "fresh"]);
    assert!(matches!(&spare.asked()[0].1, AuthFailure::Expired(why) if why.contains("expired")));
}

#[tokio::test]
async fn with_no_other_credential_the_turn_fails_and_is_carried_on_later() {
    let dirs = Dirs::new("accounts-resume");
    let mock = mock([limit_reached(), Reply::text("done")]).await;
    let spare = Spare::with(&[]);
    let sdk = sdk_with_failover(&mock, &dirs, Arc::clone(&spare) as Arc<dyn AuthFailover>).await;
    let (agent, _events) = agent(&sdk, &dirs).await;

    let failed = agent.send_text("what is the plan?").await.expect("alive");
    assert!(matches!(
        failed,
        Err(TurnError::Model(ModelError::LimitReached { info })) if info.resets_at == Some(1_794_416_400)
    ));
    assert_eq!(spare.asked().len(), 1, "the host was asked before the turn failed");

    // "Use an API key for this chat": the cut-off turn runs again, with no new prompt.
    agent.set_auth(Arc::new(Account::api_key("api-key")));
    let resumed = agent.resume().await.expect("alive").expect("turn");
    assert_eq!(resumed, Some(TurnOutcome::Finished(StopReason::EndTurn)));

    let requests = mock.requests();
    assert_eq!(key_of(&requests[1]), "api-key");
    assert_eq!(requests[0].json()["messages"], requests[1].json()["messages"], "no message was added");
    // The model answered: nothing is left to carry on.
    assert_eq!(agent.resume().await.expect("alive").expect("no turn"), None);
    assert_eq!(mock.requests().len(), 2);
}
