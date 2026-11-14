//! `/btw`: a question answered from an agent's conversation, beside it.

mod common;

use std::time::Duration;

use common::{Dirs, agent, mock, sdk};
use eos_agent_mock_api::Reply;
use eos_agent_sdk::SideError;
use serde_json::Value;

fn texts(message: &Value) -> Vec<String> {
    message["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str().map(str::to_owned)).collect()
}

#[tokio::test]
async fn an_idle_agent_answers_from_its_conversation_and_keeps_none_of_it() {
    let dirs = Dirs::new("side-idle");
    let mock = mock([Reply::text("The codename is MARLIN."), Reply::text("MARLIN"), Reply::text("next")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;
    agent.send_text("remember the codename").await.expect("alive").expect("turn");

    let answer = agent.ask_side("what was the codename?").await.expect("answer");
    assert_eq!(answer, "MARLIN");

    let requests = mock.requests();
    let (turn, side) = (requests[0].json(), requests[1].json());
    assert_eq!(side["system"], turn["system"], "the agent's own prompt");
    assert_eq!(side["tools"], turn["tools"], "and tools, for its prompt cache");
    let messages = side["messages"].as_array().expect("messages");
    let asked = texts(messages.last().expect("the question"));
    assert!(asked[0].starts_with("<system-reminder>This is a side question from the user."));
    assert!(asked[0].ends_with("</system-reminder>\n\n"));
    assert_eq!(asked[1], "what was the codename?");
    assert_eq!(texts(&messages[messages.len() - 2]), ["The codename is MARLIN."]);

    // Neither the question nor the answer is in the conversation afterwards.
    agent.send_text("carry on").await.expect("alive").expect("turn");
    let next = common::all_text(&mock.requests()[2]);
    assert!(!next.contains("side question") && !next.contains("what was the codename?"));
    assert!(agent.handle().is_idle());
}

#[tokio::test]
async fn a_running_turn_keeps_going_while_the_question_is_answered() {
    let dirs = Dirs::new("side-busy");
    let slow = Reply::text("a long answer").slowed(Duration::from_millis(150));
    let mock = mock([slow, Reply::text("quick aside")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;

    let turn = agent.handle().clone();
    let running = tokio::spawn(async move { turn.send_text("think hard").await });
    while mock.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let answer = agent.ask_side("how far along?").await.expect("answer");
    assert_eq!(answer, "quick aside");
    assert!(!agent.handle().is_idle(), "the turn is still running");
    // The question was put to the conversation the turn had sent.
    let side = mock.requests()[1].json();
    assert_eq!(texts(&side["messages"][0]).last().map(String::as_str), Some("think hard"));

    running.await.expect("join").expect("alive").expect("turn");
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn a_question_can_be_dropped_and_an_empty_answer_is_an_error() {
    let dirs = Dirs::new("side-cancel");
    let never = Reply::text("too late").slowed(Duration::from_secs(5));
    let mock =
        mock([never, Reply::tool_use("toolu_1", "Read", r#"{"file_path":"/x"}"#), Reply::text("still here")]).await;
    let sdk = sdk(&mock, &dirs).await;
    let (agent, _events) = agent(&sdk, &dirs).await;

    let dropped = tokio::time::timeout(Duration::from_millis(100), agent.ask_side("anyone?")).await;
    assert!(dropped.is_err(), "the question was cancelled by dropping it");
    // A reply that only calls a tool has no answer; the tool is not run.
    assert!(matches!(agent.ask_side("read it for me").await, Err(SideError::NoAnswer)));
    agent.send_text("hello").await.expect("alive").expect("turn");
    assert_eq!(mock.requests().len(), 3);
}
