//! A Chat Completions stream as Messages stream events.
//!
//! Text and reasoning go out as they arrive. Tool calls are held until the
//! stream ends: servers send their pieces in different ways (whole, in
//! parts, several calls interleaved), and a Messages block is one call from
//! start to end.

use eos_agent_core::ModelError;
use eos_agent_types::{ContentBlock, Delta, MessageStart, Role, StopReason, StreamEvent, Usage};
use serde_json::Value;
use serde_json::value::RawValue;

use super::request::CARRIER;

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
    /// What the server sent beside the call and wants back with it.
    extra: Option<Value>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    Text,
    Thinking,
}

#[derive(Default)]
pub(super) struct Translator {
    started: bool,
    open: Option<Open>,
    /// The next content block's index.
    next: u32,
    /// By the index the server gives each call.
    calls: Vec<(u64, Call)>,
    finish: Option<String>,
    usage: Option<Usage>,
    finished: bool,
}

fn empty_input() -> Box<RawValue> {
    RawValue::from_string("{}".into()).expect("`{}` is valid JSON")
}

impl Translator {
    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    /// The server said how the reply ended (the stream may still carry usage).
    pub(super) fn has_finish(&self) -> bool {
        self.finish.is_some()
    }

    /// One chunk's `data`.
    pub(super) fn push(&mut self, data: &str, out: &mut Vec<StreamEvent>) -> Result<(), ModelError> {
        let chunk: Value =
            serde_json::from_str(data).map_err(|e| ModelError::Transient(format!("unreadable stream chunk: {e}")))?;
        if chunk["error"].is_object() || chunk["error"].is_string() {
            return Err(crate::errors::from_error_object(&chunk["error"]));
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart(MessageStart {
                id: chunk["id"].as_str().unwrap_or("chatcmpl").to_owned(),
                model: chunk["model"].as_str().unwrap_or_default().to_owned(),
                role: Role::Assistant,
                content: Vec::new(),
                stop_reason: None,
                usage: Usage::default(),
            }));
        }
        if chunk["usage"].is_object() {
            self.usage = Some(usage(&chunk["usage"]));
        }
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        let reasoning = delta["reasoning_content"].as_str().or_else(|| delta["reasoning"].as_str());
        if let Some(text) = reasoning.filter(|t| !t.is_empty()) {
            let index = self.ensure(Open::Thinking, out);
            out.push(StreamEvent::ContentBlockDelta { index, delta: Delta::Thinking(text.to_owned()) });
        }
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            let index = self.ensure(Open::Text, out);
            out.push(StreamEvent::ContentBlockDelta { index, delta: Delta::Text(text.to_owned()) });
        }
        for (position, part) in delta["tool_calls"].as_array().into_iter().flatten().enumerate() {
            let index = part["index"].as_u64().unwrap_or(position as u64);
            let at = match self.calls.iter().position(|(i, _)| *i == index) {
                Some(at) => at,
                None => {
                    self.calls.push((index, Call::default()));
                    self.calls.len() - 1
                }
            };
            let call = &mut self.calls[at].1;
            if let Some(id) = part["id"].as_str().filter(|id| !id.is_empty()) {
                call.id = id.to_owned();
            }
            if let Some(name) = part["function"]["name"].as_str().filter(|n| !n.is_empty()) {
                call.name = name.to_owned();
            }
            call.arguments.push_str(part["function"]["arguments"].as_str().unwrap_or_default());
            if part["extra_content"].is_object() {
                call.extra = Some(part["extra_content"].clone());
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish = Some(reason.to_owned());
        }
        Ok(())
    }

    /// The index of the open block of `kind`, opening one (and closing another) first.
    fn ensure(&mut self, kind: Open, out: &mut Vec<StreamEvent>) -> u32 {
        if self.open == Some(kind) {
            return self.next - 1;
        }
        self.close(out);
        let block = match kind {
            Open::Text => ContentBlock::Text { text: String::new(), citations: None },
            Open::Thinking => ContentBlock::Thinking { thinking: String::new(), signature: String::new() },
        };
        out.push(StreamEvent::ContentBlockStart { index: self.next, block });
        self.open = Some(kind);
        self.next += 1;
        self.next - 1
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        if self.open.take().is_some() {
            out.push(StreamEvent::ContentBlockStop { index: self.next - 1 });
        }
    }

    fn block(&mut self, block: ContentBlock, delta: Delta, out: &mut Vec<StreamEvent>) {
        let index = self.next;
        self.next += 1;
        out.push(StreamEvent::ContentBlockStart { index, block });
        out.push(StreamEvent::ContentBlockDelta { index, delta });
        out.push(StreamEvent::ContentBlockStop { index });
    }

    /// The stream is over: the held tool calls, then how the reply ended.
    pub(super) fn finish(&mut self, out: &mut Vec<StreamEvent>) {
        if self.finished || !self.started {
            return;
        }
        self.finished = true;
        self.close(out);
        let calls = std::mem::take(&mut self.calls);
        let called = !calls.is_empty();
        for (_, mut call) in calls {
            if call.id.is_empty() {
                call.id = format!("call_{}", uuid::Uuid::new_v4().simple());
            }
            if let Some(extra) = call.extra.take() {
                let kept = serde_json::json!({"id": call.id, "extra": extra});
                let carrier = ContentBlock::Thinking { thinking: String::new(), signature: String::new() };
                self.block(carrier, Delta::Signature(format!("{CARRIER}{kept}")), out);
            }
            // A server may send no arguments for a call that takes none.
            let arguments = if call.arguments.trim().is_empty() { "{}".to_owned() } else { call.arguments };
            let block = ContentBlock::ToolUse { id: call.id, name: call.name, input: empty_input() };
            self.block(block, Delta::InputJson(arguments), out);
        }
        let reason = match self.finish.as_deref() {
            _ if called => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal,
            _ => StopReason::EndTurn,
        };
        out.push(StreamEvent::MessageDelta {
            stop_reason: Some(reason),
            stop_sequence: None,
            usage: self.usage.take(),
        });
        out.push(StreamEvent::MessageStop);
    }
}

/// Chat usage in the Messages fields: cached prompt tokens are reported apart.
fn usage(usage: &Value) -> Usage {
    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = usage["prompt_tokens_details"]["cached_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_cache_hit_tokens"].as_u64())
        .unwrap_or(0)
        .min(prompt);
    Usage {
        input_tokens: Some(prompt - cached),
        output_tokens: Some(usage["completion_tokens"].as_u64().unwrap_or(0)),
        cache_read_input_tokens: (cached > 0).then_some(cached),
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn run(chunks: &[Value]) -> Vec<Value> {
        let mut translator = Translator::default();
        let mut out = Vec::new();
        for chunk in chunks {
            translator.push(&chunk.to_string(), &mut out).expect("chunk");
        }
        translator.finish(&mut out);
        out.iter().map(|e| serde_json::from_str(&e.to_json().expect("json")).expect("value")).collect()
    }

    fn delta(delta: Value) -> Value {
        json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": delta}]})
    }

    #[test]
    fn reasoning_text_and_calls_become_blocks_in_order() {
        let events = run(&[
            delta(json!({"role": "assistant", "reasoning_content": "Hm."})),
            delta(json!({"content": "Read"})),
            delta(json!({"content": "ing."})),
            delta(
                json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "Read", "arguments": "{\"pa"}}]}),
            ),
            delta(json!({"tool_calls": [{"index": 1, "id": "c2", "function": {"name": "Glob", "arguments": ""},
                "extra_content": {"google": {"thought_signature": "SIG"}}}]})),
            delta(json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\":\"a\"}"}}]})),
            json!({"id": "c", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"id": "c", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 7,
                "prompt_tokens_details": {"cached_tokens": 60}}}),
        ]);
        let starts: Vec<&Value> = events.iter().filter(|e| e["type"] == "content_block_start").collect();
        let kinds: Vec<&str> = starts.iter().filter_map(|e| e["content_block"]["type"].as_str()).collect();
        assert_eq!(kinds, ["thinking", "text", "tool_use", "thinking", "tool_use"]);
        assert_eq!(starts[2]["content_block"]["id"], "c1");
        let deltas: Vec<&Value> = events.iter().filter(|e| e["type"] == "content_block_delta").collect();
        let partial: Vec<&str> = deltas.iter().filter_map(|e| e["delta"]["partial_json"].as_str()).collect();
        assert_eq!(partial, [r#"{"path":"a"}"#, "{}"], "a call's arguments are whole; none is an empty object");
        let carried = deltas.iter().find_map(|e| e["delta"]["signature"].as_str()).expect("the carrier");
        assert!(carried.starts_with(CARRIER) && carried.contains("\"c2\"") && carried.contains("SIG"), "{carried}");
        let end = events.iter().find(|e| e["type"] == "message_delta").expect("the end");
        assert_eq!(end["delta"]["stop_reason"], "tool_use");
        assert_eq!(end["usage"], json!({"input_tokens": 40, "output_tokens": 7, "cache_read_input_tokens": 60}));
        assert_eq!(events.last().expect("last")["type"], "message_stop");
    }

    #[test]
    fn a_plain_answer_ends_the_turn_and_an_error_chunk_fails() {
        let events =
            run(&[delta(json!({"content": "hi"})), json!({"choices": [{"delta": {}, "finish_reason": "length"}]})]);
        let end = events.iter().find(|e| e["type"] == "message_delta").expect("the end");
        assert_eq!(end["delta"]["stop_reason"], "max_tokens");

        let mut translator = Translator::default();
        let failed = translator
            .push(&json!({"error": {"message": "nope", "code": "server_error"}}).to_string(), &mut Vec::new());
        assert!(matches!(failed, Err(ModelError::Transient(_))), "{failed:?}");
    }
}
