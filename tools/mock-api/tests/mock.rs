use std::time::Duration;

use eos_agent_mock_api::{MockApi, Reply};
use eos_agent_types::{Delta, StreamEvent};

async fn post(mock: &MockApi) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", mock.base_url()))
        .body(r#"{"model":"m","messages":[]}"#)
        .send()
        .await
        .expect("send")
}

fn parse_sse(body: &str) -> Vec<StreamEvent> {
    body.split("\n\n")
        .filter(|f| !f.is_empty())
        .map(|frame| {
            let event = frame.lines().find_map(|l| l.strip_prefix("event: ")).expect("event line");
            let data = frame.lines().find_map(|l| l.strip_prefix("data: ")).expect("data line");
            StreamEvent::parse(event, data).expect("parse")
        })
        .collect()
}

#[tokio::test]
async fn replies_in_order_and_records_requests() {
    let mock = MockApi::start([Reply::text("hello"), Reply::rate_limited(3)]).await.expect("start");

    let first = post(&mock).await.text().await.expect("body");
    let events = parse_sse(&first);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentBlockDelta { delta: Delta::Text(t), .. } if t == "hello"))
    );
    assert!(matches!(events.last(), Some(StreamEvent::MessageStop)));

    let second = post(&mock).await;
    assert_eq!(second.status(), 429);
    assert_eq!(second.headers()["retry-after"], "3");

    assert_eq!(mock.requests().len(), 2);
    assert_eq!(mock.requests()[0].json()["model"], "m");
    assert_eq!(mock.remaining(), 0);
}

#[tokio::test]
async fn dropped_stream_fails_mid_body() {
    let mock = MockApi::start([Reply::text("cut me").dropped_after(2)]).await.expect("start");
    let res = post(&mock).await;
    assert_eq!(res.status(), 200);
    assert!(res.text().await.is_err(), "a cut stream must surface as a body error");
}

#[tokio::test]
async fn slowed_stream_takes_its_delays() {
    let mock = MockApi::start([Reply::text("slow").slowed(Duration::from_millis(20))]).await.expect("start");
    let started = std::time::Instant::now();
    post(&mock).await.text().await.expect("body");
    assert!(started.elapsed() >= Duration::from_millis(100));
}
