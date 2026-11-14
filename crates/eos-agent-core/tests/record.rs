//! A user turn recorded without a model request (a slash command that ran
//! locally).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eos_agent_core::{
    Agent, AgentConfig, AgentEvent, AllowAll, BoxFuture, Clock, Deps, EventSink, EventStream, ModelClient, ModelError,
    ModelRequest, SessionContext, StoredMessage, SystemPrompt, TurnOutcome,
};
use eos_agent_types::{ContentBlock, Role, StopReason};

/// Fails the test if the agent asks it anything.
struct NoModel;

impl ModelClient for NoModel {
    fn stream<'a>(&'a self, _: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        panic!("a recorded turn makes no model request")
    }
}

#[derive(Default)]
struct Collect(Mutex<Vec<AgentEvent>>);

impl EventSink for Collect {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'_, ()> {
        self.0.lock().expect("lock").push(event);
        Box::pin(async {})
    }
}

struct Instant;

impl Clock for Instant {
    fn sleep(&self, _: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

fn text(t: &str) -> ContentBlock {
    ContentBlock::Text { text: t.into(), citations: None }
}

fn texts(message: &StoredMessage) -> Vec<String> {
    message
        .blocks()
        .expect("blocks")
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_recorded_turn_joins_the_conversation_without_a_request() {
    let context = SessionContext {
        user_prefix: Arc::from(vec![text("<system-reminder>ctx</system-reminder>")]),
        system_notes: vec![StoredMessage::system_text("env")],

        ..SessionContext::default()
    };
    let mut agent = Agent::new("a", AgentConfig::new("m"), SystemPrompt::new(&[("sys", None)]), Arc::default())
        .with_session_context(context);
    let sink = Collect::default();
    let deps = Deps { model: &NoModel, events: &sink, permissions: &AllowAll, clock: &Instant, store: None };

    let out =
        agent.record_turn(vec![text("/survey"), text("<local-command-stdout>x</local-command-stdout>")], false, deps);
    assert_eq!(out.await.expect("recorded"), TurnOutcome::Finished(StopReason::EndTurn));
    let transcript = agent.transcript();
    assert_eq!(transcript.len(), 1, "the session's system note waits for the first request");
    assert_eq!(
        texts(&transcript[0]),
        ["<system-reminder>ctx</system-reminder>", "/survey", "<local-command-stdout>x</local-command-stdout>"],
        "the first message carries the session context"
    );
    let ended = sink.0.lock().expect("lock").iter().filter(|e| matches!(e, AgentEvent::TurnEnded { .. })).count();
    assert_eq!(ended, 1);

    let out = agent.record_turn(vec![text("/survey")], true, deps).await.expect("recorded");
    assert_eq!(out, TurnOutcome::Interrupted);
    let transcript = agent.transcript();
    assert_eq!(transcript.len(), 3);
    assert!(transcript.iter().all(|m| m.role() == Role::User));
    assert_eq!(texts(&transcript[1]), ["/survey"], "no session context after the first message");
    assert_eq!(texts(&transcript[2]), ["[Request interrupted by user]"]);
}
