#![allow(dead_code, unreachable_pub)]

use std::collections::VecDeque;
use std::sync::Mutex;

use eos_agent_core::{
    BoxFuture, CancellationToken, EventStream, Extensions, ModelClient, ModelError, ModelRequest, Tool, ToolContext,
    ToolOutput,
};
use eos_agent_types::{StreamEvent, ToolResultContent};
use serde_json::value::RawValue;

/// Replays scripted responses and records every request.
#[derive(Default)]
pub struct FakeModel {
    script: Mutex<VecDeque<Vec<StreamEvent>>>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl FakeModel {
    pub fn new(script: impl IntoIterator<Item = Vec<StreamEvent>>) -> Self {
        Self { script: Mutex::new(script.into_iter().collect()), ..Self::default() }
    }

    pub fn request(&self, i: usize) -> ModelRequest {
        self.requests.lock().expect("lock")[i].clone()
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().expect("lock").len()
    }
}

impl ModelClient for FakeModel {
    fn stream<'a>(&'a self, request: &'a ModelRequest) -> BoxFuture<'a, Result<EventStream, ModelError>> {
        self.requests.lock().expect("lock").push(request.clone());
        let next = self.script.lock().expect("lock").pop_front();
        Box::pin(async move {
            let events = next.ok_or_else(|| ModelError::Invalid("script exhausted".into()))?;
            let stream: EventStream = Box::pin(futures_util::stream::iter(events.into_iter().map(Ok)));
            Ok(stream)
        })
    }
}

pub fn ev(name: &str, data: &str) -> StreamEvent {
    StreamEvent::parse(name, data).expect("event")
}

/// A reply made of whole blocks: `(content_block_start JSON, optional delta JSON)`.
pub fn reply(blocks: &[(&str, Option<&str>)]) -> Vec<StreamEvent> {
    let mut out = vec![ev("message_start", r#"{"message":{"id":"m","model":"x","role":"assistant","usage":{}}}"#)];
    for (i, (start, delta)) in blocks.iter().enumerate() {
        out.push(ev("content_block_start", &format!(r#"{{"index":{i},"content_block":{start}}}"#)));
        if let Some(delta) = delta {
            out.push(ev("content_block_delta", &format!(r#"{{"index":{i},"delta":{delta}}}"#)));
        }
        out.push(ev("content_block_stop", &format!(r#"{{"index":{i}}}"#)));
    }
    out.push(ev("message_delta", r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#));
    out.push(ev("message_stop", "{}"));
    out
}

pub fn text_reply(text: &str) -> Vec<StreamEvent> {
    let delta = format!(r#"{{"type":"text_delta","text":{}}}"#, serde_json::to_string(text).expect("json"));
    reply(&[(r#"{"type":"text","text":""}"#, Some(&delta))])
}

pub struct Agent {
    pub id: String,
    pub extensions: Extensions,
    cancel: CancellationToken,
}

impl Agent {
    pub fn new(id: &str) -> Self {
        Self { id: id.into(), extensions: Extensions::default(), cancel: CancellationToken::new() }
    }

    pub async fn call(&self, tool: &dyn Tool, input: serde_json::Value) -> ToolOutput {
        let raw = RawValue::from_string(input.to_string()).expect("json");
        let ctx = ToolContext {
            agent_id: &self.id,
            tool_use_id: "toolu_1",
            cancel: &self.cancel,
            extensions: &self.extensions,
            transcript: &[],
        };
        tool.call(&raw, ctx).await
    }
}

pub fn text_of(out: &ToolOutput) -> &str {
    match &out.content {
        ToolResultContent::Text(t) => t,
        ToolResultContent::Blocks(_) => panic!("expected text"),
    }
}

/// The text of the single user message in a side request.
pub fn prompt_of(request: &ModelRequest) -> String {
    let msg: serde_json::Value = serde_json::from_slice(&request.messages[0]).expect("json");
    msg["content"][0]["text"].as_str().expect("text").to_owned()
}
