//! Scripted OpenAI Responses streams (`POST /v1/responses`).

use serde_json::{Value, json};

use crate::reply::{Reply, Step};

/// One output item of a scripted response.
#[derive(Debug, Clone)]
pub enum Output {
    /// A reasoning item: its summary (streamed) and encrypted content.
    Reasoning {
        id: String,
        summary: String,
        encrypted: String,
    },
    Text(String),
    /// A function call; the arguments stream in chunks of odd sizes.
    Call {
        call_id: String,
        name: String,
        arguments: String,
    },
}

impl Output {
    pub fn reasoning(id: &str, summary: &str, encrypted: &str) -> Self {
        Self::Reasoning { id: id.into(), summary: summary.into(), encrypted: encrypted.into() }
    }

    pub fn text(text: &str) -> Self {
        Self::Text(text.into())
    }

    pub fn call(call_id: &str, name: &str, arguments: &str) -> Self {
        Self::Call { call_id: call_id.into(), name: name.into(), arguments: arguments.into() }
    }
}

fn event(data: &Value) -> Step {
    let kind = data["type"].as_str().unwrap_or("message");
    Step::Raw(format!("event: {kind}\ndata: {data}\n\n"))
}

/// Splits `s` at char boundaries into pieces of 1, 2, 3, … chars.
fn chunks(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let (mut at, mut size) = (0, 1);
    while at < chars.len() {
        let end = (at + size).min(chars.len());
        out.push(chars[at..end].iter().collect());
        at = end;
        size += 1;
    }
    out
}

impl Reply {
    /// A completed response with `outputs`, then usage (`input` tokens of
    /// which `cached` were cached, `output` tokens).
    pub fn responses(outputs: &[Output], usage: (u64, u64, u64)) -> Self {
        let mut steps = vec![event(
            &json!({"type": "response.created", "response": {"id": "resp_mock", "model": "mock-gpt", "status": "in_progress"}}),
        )];
        for (i, output) in outputs.iter().enumerate() {
            let at = i as u64;
            match output {
                Output::Reasoning { id, summary, encrypted } => {
                    steps.push(event(&json!({"type": "response.output_item.added", "output_index": at, "item": {"type": "reasoning", "id": id, "summary": []}})));
                    if !summary.is_empty() {
                        steps.push(event(&json!({"type": "response.reasoning_summary_part.added", "item_id": id, "output_index": at, "summary_index": 0})));
                        for piece in chunks(summary) {
                            steps.push(event(&json!({"type": "response.reasoning_summary_text.delta", "item_id": id, "output_index": at, "summary_index": 0, "delta": piece})));
                        }
                    }
                    let summary =
                        if summary.is_empty() { json!([]) } else { json!([{"type": "summary_text", "text": summary}]) };
                    steps.push(event(&json!({"type": "response.output_item.done", "output_index": at, "item": {"type": "reasoning", "id": id, "summary": summary, "encrypted_content": encrypted}})));
                }
                Output::Text(text) => {
                    let id = format!("msg_{i}");
                    steps.push(event(&json!({"type": "response.output_item.added", "output_index": at, "item": {"type": "message", "id": id, "role": "assistant", "status": "in_progress", "content": []}})));
                    steps.push(event(&json!({"type": "response.content_part.added", "item_id": id, "output_index": at, "content_index": 0, "part": {"type": "output_text", "text": ""}})));
                    for piece in chunks(text) {
                        steps.push(event(&json!({"type": "response.output_text.delta", "item_id": id, "output_index": at, "content_index": 0, "delta": piece})));
                    }
                    steps.push(event(&json!({"type": "response.output_item.done", "output_index": at, "item": {"type": "message", "id": id, "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text, "annotations": []}]}})));
                }
                Output::Call { call_id, name, arguments } => {
                    let id = format!("fc_{i}");
                    steps.push(event(&json!({"type": "response.output_item.added", "output_index": at, "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name, "namespace": "functions", "arguments": ""}})));
                    for piece in chunks(arguments) {
                        steps.push(event(&json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": at, "delta": piece})));
                    }
                    steps.push(event(&json!({"type": "response.output_item.done", "output_index": at, "item": {"type": "function_call", "id": id, "call_id": call_id, "name": name, "namespace": "functions", "arguments": arguments, "status": "completed"}})));
                }
            }
        }
        let (input, cached, output) = usage;
        steps.push(event(&json!({"type": "response.completed", "response": {"id": "resp_mock", "status": "completed", "usage": {
            "input_tokens": input, "input_tokens_details": {"cached_tokens": cached},
            "output_tokens": output, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": input + output}}})));
        Self::Stream(steps)
    }

    /// A response that answers with `text`.
    pub fn responses_text(text: &str) -> Self {
        Self::responses(&[Output::text(text)], (100, 0, 5))
    }

    /// A response that calls one tool.
    pub fn responses_call(call_id: &str, name: &str, arguments: &str) -> Self {
        Self::responses(&[Output::call(call_id, name, arguments)], (100, 0, 5))
    }

    /// A stream that starts, then fails with `code` (as a usage limit does mid-stream).
    pub fn responses_failed(code: &str, message: &str) -> Self {
        Self::Stream(vec![
            event(&json!({"type": "response.created", "response": {"id": "resp_mock", "status": "in_progress"}})),
            event(
                &json!({"type": "response.failed", "response": {"id": "resp_mock", "status": "failed", "error": {"code": code, "message": message}}}),
            ),
        ])
    }

    /// An HTTP refusal with a Responses error body.
    pub fn responses_error(status: u16, code: &str, message: &str) -> Self {
        let body = json!({"error": {"type": "invalid_request_error", "code": code, "message": message, "param": null}});
        Self::Status { status, headers: Vec::new(), body: body.to_string() }
    }
}
