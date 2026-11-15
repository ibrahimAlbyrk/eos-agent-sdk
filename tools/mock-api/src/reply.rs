use std::time::Duration;

use bytes::Bytes;
use eos_agent_types::{ContentBlock, Delta, MessageStart, Role, StopReason, StreamEvent, Usage};
use futures_util::Stream;
use serde_json::value::RawValue;

/// What the mock answers to one request.
#[derive(Debug, Clone)]
pub enum Reply {
    Stream(Vec<Step>),
    Status { status: u16, headers: Vec<(String, String)>, body: String },
}

/// One step of a streamed reply.
#[derive(Debug, Clone)]
pub enum Step {
    Event(StreamEvent),
    /// Bytes sent as-is: a recorded `.sse` file or deliberately broken framing.
    Raw(String),
    Delay(Duration),
    /// Cut the connection here, as a network failure would.
    Drop,
}

impl Reply {
    /// A complete turn that answers with `text`.
    pub fn text(text: &str) -> Self {
        let mut steps = open();
        steps.extend(block(0, ContentBlock::Text { text: String::new(), citations: None }, Delta::Text(text.into())));
        steps.extend(close(StopReason::EndTurn));
        Self::Stream(steps)
    }

    /// A complete turn that calls one tool. `input` is the tool input as JSON.
    pub fn tool_use(id: &str, name: &str, input: &str) -> Self {
        let empty = RawValue::from_string("{}".into()).expect("`{}` is valid JSON");
        let mut steps = open();
        steps.extend(block(
            0,
            ContentBlock::ToolUse { id: id.into(), name: name.into(), input: empty },
            Delta::InputJson(input.into()),
        ));
        steps.extend(close(StopReason::ToolUse));
        Self::Stream(steps)
    }

    /// A recorded SSE body replayed verbatim.
    pub fn sse(body: impl Into<String>) -> Self {
        Self::Stream(vec![Step::Raw(body.into())])
    }

    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::error(429, "rate_limit_error", vec![("retry-after".into(), retry_after_secs.to_string())])
    }

    pub fn overloaded() -> Self {
        Self::error(529, "overloaded_error", Vec::new())
    }

    /// The Messages API's refusal of a prompt over the model's context window.
    pub fn prompt_too_long() -> Self {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        Self::Status { status: 400, headers: vec![("x-should-retry".into(), "false".into())], body: body.into() }
    }

    fn error(status: u16, kind: &str, headers: Vec<(String, String)>) -> Self {
        let body = format!(r#"{{"type":"error","error":{{"type":"{kind}","message":"mock {kind}"}}}}"#);
        Self::Status { status, headers, body }
    }

    /// Inserts `delay` before every event, for slow-stream and watchdog tests.
    pub fn slowed(self, delay: Duration) -> Self {
        match self {
            Self::Stream(steps) => Self::Stream(steps.into_iter().flat_map(|s| [Step::Delay(delay), s]).collect()),
            other => other,
        }
    }

    /// The same reply with stop reason `reason` (`None`: the stream states none).
    pub fn stopping(self, reason: Option<StopReason>) -> Self {
        match self {
            Self::Stream(steps) => Self::Stream(
                steps
                    .into_iter()
                    .map(|step| match step {
                        Step::Event(StreamEvent::MessageDelta { stop_sequence, usage, .. }) => {
                            Step::Event(StreamEvent::MessageDelta { stop_reason: reason.clone(), stop_sequence, usage })
                        }
                        other => other,
                    })
                    .collect(),
            ),
            other => other,
        }
    }

    /// Keeps the first `events` steps, then cuts the connection.
    pub fn dropped_after(self, events: usize) -> Self {
        match self {
            Self::Stream(mut steps) => {
                steps.truncate(events);
                steps.push(Step::Drop);
                Self::Stream(steps)
            }
            other => other,
        }
    }
}

fn open() -> Vec<Step> {
    let usage = Usage { input_tokens: Some(10), output_tokens: Some(1), ..Usage::default() };
    vec![Step::Event(StreamEvent::MessageStart(MessageStart {
        id: "msg_mock".into(),
        model: "mock-model".into(),
        role: Role::Assistant,
        content: Vec::new(),
        stop_reason: None,
        usage,
    }))]
}

fn block(index: u32, start: ContentBlock, delta: Delta) -> [Step; 3] {
    [
        Step::Event(StreamEvent::ContentBlockStart { index, block: start }),
        Step::Event(StreamEvent::ContentBlockDelta { index, delta }),
        Step::Event(StreamEvent::ContentBlockStop { index }),
    ]
}

fn close(reason: StopReason) -> [Step; 2] {
    let usage = Usage { output_tokens: Some(5), ..Usage::default() };
    [
        Step::Event(StreamEvent::MessageDelta { stop_reason: Some(reason), stop_sequence: None, usage: Some(usage) }),
        Step::Event(StreamEvent::MessageStop),
    ]
}

pub(crate) fn stream(steps: Vec<Step>) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    futures_util::stream::unfold(steps.into_iter(), |mut steps| async move {
        loop {
            match steps.next()? {
                Step::Delay(d) => tokio::time::sleep(d).await,
                Step::Raw(raw) => return Some((Ok(Bytes::from(raw)), steps)),
                Step::Event(event) => {
                    let data = event.to_json()?;
                    return Some((Ok(Bytes::from(format!("event: {}\ndata: {data}\n\n", event.name()))), steps));
                }
                Step::Drop => {
                    // Let the bytes already yielded reach the socket first; hyper would
                    // otherwise discard its unflushed buffer and the client would see a
                    // failed request instead of a stream that breaks mid-way.
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    let cut = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "mock drop");
                    return Some((Err(cut), Vec::new().into_iter()));
                }
            }
        }
    })
}

/// A load-test conversation: a request whose latest assistant message called
/// a tool gets a short text answer; any other gets a call to Read `file`. So
/// every user turn is one tool call and one answer, whatever the order in
/// which concurrent agents' requests arrive.
pub fn read_then_answer(file: &str) -> impl Fn(&[u8]) -> Reply + Send + Sync + 'static {
    let input = serde_json::json!({ "file_path": file }).to_string();
    // Each call its own id, as the API gives them (a request with two calls
    // of one id is refused).
    let calls = std::sync::atomic::AtomicU64::new(0);
    move |body| {
        let last = find_last(body, br#"{"role":"assistant""#).map(|at| &body[at..]);
        let called = last.is_some_and(|msg| {
            let next = br#"{"role":"#;
            let end = msg[1..].windows(next.len()).position(|w| w == next).map_or(msg.len(), |i| i + 1);
            find_last(&msg[..end], br#""type":"tool_use""#).is_some()
        });
        if called {
            Reply::text("Done.")
        } else {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Reply::tool_use(&format!("toolu_read_{n}"), "Read", &input)
        }
    }
}

fn find_last(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}
