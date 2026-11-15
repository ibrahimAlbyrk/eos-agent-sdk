//! Scripted Chat Completions streams (`POST /v1/chat/completions`).

use serde_json::{Value, json};

use crate::reply::{Reply, Step};

impl Reply {
    /// A stream of `chunks` as sent, closed by `[DONE]`.
    pub fn chat(chunks: &[Value]) -> Self {
        let mut steps: Vec<Step> = chunks.iter().map(|c| Step::Raw(format!("data: {c}\n\n"))).collect();
        steps.push(Step::Raw("data: [DONE]\n\n".into()));
        Self::Stream(steps)
    }

    /// A complete reply of `deltas`, ended with `finish`, then usage.
    fn chat_reply(deltas: &[Value], finish: &str) -> Self {
        let chunk = |delta: &Value, finish: Value| {
            json!({"id": "chatcmpl-mock", "model": "mock-chat",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        };
        let mut chunks: Vec<Value> = deltas.iter().map(|d| chunk(d, Value::Null)).collect();
        chunks.push(chunk(&json!({}), finish.into()));
        chunks.push(json!({"id": "chatcmpl-mock", "choices": [],
                           "usage": {"prompt_tokens": 10, "completion_tokens": 5}}));
        Self::chat(&chunks)
    }

    /// A complete turn that answers with `text`, after `reasoning` when it is not empty.
    pub fn chat_text(reasoning: &str, text: &str) -> Self {
        let mut deltas = vec![json!({"role": "assistant"})];
        if !reasoning.is_empty() {
            deltas.push(json!({"reasoning_content": reasoning}));
        }
        deltas.push(json!({"content": text}));
        Self::chat_reply(&deltas, "stop")
    }

    /// A complete turn that calls one tool; the arguments arrive in two parts.
    pub fn chat_call(id: &str, name: &str, arguments: &str) -> Self {
        let (head, tail) = arguments.split_at(arguments.len() / 2);
        let part = |id: Value, name: Value, arguments: &str| {
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                                   "function": {"name": name, "arguments": arguments}}]})
        };
        let deltas = [part(id.into(), name.into(), head), part(Value::Null, Value::Null, tail)];
        Self::chat_reply(&deltas, "tool_calls")
    }
}
