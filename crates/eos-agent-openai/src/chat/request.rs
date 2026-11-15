//! A Messages request as a Chat Completions body.

use eos_agent_core::{ModelError, ModelRequest};
use serde_json::{Map, Value, json};

use super::{Compat, MaxTokens};

/// What a thinking block's signature opens with when it carries what a
/// server sent beside a tool call and wants back (`extra_content`).
pub(super) const CARRIER: &str = "cc1.";

fn invalid(what: impl std::fmt::Display) -> ModelError {
    ModelError::Invalid(format!("the request could not be written for the provider: {what}"))
}

/// The body for `request`, with `model` as the provider names it.
pub(super) fn build(request: &ModelRequest, model: &str, compat: &Compat) -> Result<Vec<u8>, ModelError> {
    let mut messages = Vec::new();
    let system: Value = serde_json::from_slice(&request.system).map_err(invalid)?;
    let system = texts(&system).join("\n\n");
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        let message: Value = serde_json::from_slice(message).map_err(invalid)?;
        let blocks = message["content"].as_array().map(Vec::as_slice).unwrap_or_default();
        match message["role"].as_str() {
            Some("assistant") => messages.push(assistant(blocks, compat)),
            Some("system") => {
                messages.push(json!({"role": "system", "content": texts(&message["content"]).join("\n\n")}))
            }
            _ => user(blocks, compat, &mut messages),
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), model.into());
    body.insert("messages".into(), messages.into());
    body.insert("stream".into(), true.into());
    if compat.stream_usage {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    match compat.max_tokens {
        MaxTokens::MaxTokens => body.insert("max_tokens".into(), request.max_tokens.into()),
        MaxTokens::MaxCompletionTokens => body.insert("max_completion_tokens".into(), request.max_tokens.into()),
        MaxTokens::Omit => None,
    };
    if let Some(effort) = request.effort.as_deref().filter(|_| compat.effort) {
        body.insert("reasoning_effort".into(), effort.into());
    }
    let tools = match &request.tools {
        Some(tools) => functions(&serde_json::from_slice::<Value>(tools).map_err(invalid)?),
        None => Vec::new(),
    };
    if !tools.is_empty() {
        body.insert("tools".into(), tools.into());
        let extra: Value = request.extra.as_deref().and_then(|e| serde_json::from_slice(e).ok()).unwrap_or_default();
        if let Some(choice) = tool_choice(&extra["tool_choice"]) {
            body.insert("tool_choice".into(), choice);
        }
    }
    serde_json::to_vec(&Value::Object(body)).map_err(invalid)
}

/// The text of every text block of a `content` array.
fn texts(content: &Value) -> Vec<&str> {
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect()
}

/// The tools a function declares (the ones a provider runs itself have no schema).
fn functions(tools: &Value) -> Vec<Value> {
    let tools = tools.as_array().map(Vec::as_slice).unwrap_or_default();
    let declared = tools.iter().filter(|t| t["input_schema"].is_object() && t["name"].is_string());
    declared
        .map(|t| {
            let mut function = json!({"name": t["name"], "parameters": t["input_schema"]});
            if let Some(description) = t["description"].as_str() {
                function["description"] = description.into();
            }
            json!({"type": "function", "function": function})
        })
        .collect()
}

fn tool_choice(choice: &Value) -> Option<Value> {
    Some(match choice["type"].as_str()? {
        "any" => "required".into(),
        "none" => "none".into(),
        "tool" => json!({"type": "function", "function": {"name": choice["name"]}}),
        _ => "auto".into(),
    })
}

/// What a tool returned, as text.
fn result_text(block: &Value) -> String {
    let text = match &block["content"] {
        Value::String(text) => text.clone(),
        parts @ Value::Array(_) => {
            let said = texts(parts).join("\n");
            let images = parts.as_array().is_some_and(|p| p.iter().any(|b| b["type"] == "image"));
            if images { format!("{said}\n[image omitted]").trim().to_owned() } else { said }
        }
        _ => String::new(),
    };
    if block["is_error"] == true { format!("Error: {text}") } else { text }
}

/// A user message: its tool results as `tool` messages, then the rest.
fn user(blocks: &[Value], compat: &Compat, out: &mut Vec<Value>) {
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("tool_result") => {
                out.push(json!({"role": "tool", "tool_call_id": block["tool_use_id"], "content": result_text(block)}));
            }
            Some("text") => parts.push(json!({"type": "text", "text": block["text"]})),
            Some("image") if compat.images => {
                let source = &block["source"];
                let url = match source["type"].as_str() {
                    Some("url") => source["url"].as_str().unwrap_or_default().to_owned(),
                    _ => format!(
                        "data:{};base64,{}",
                        source["media_type"].as_str().unwrap_or("image/png"),
                        source["data"].as_str().unwrap_or_default()
                    ),
                };
                parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
            }
            Some("image") => {
                parts.push(json!({"type": "text", "text": "[image omitted: the model cannot read images]"}))
            }
            Some("document") => parts.push(json!({"type": "text", "text": "[document omitted]"})),
            _ => {}
        }
    }
    if parts.is_empty() {
        return;
    }
    // Plain text goes as a string, which every server takes.
    let content = if parts.iter().all(|p| p["type"] == "text") {
        Value::from(parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n\n"))
    } else {
        Value::from(parts)
    };
    out.push(json!({"role": "user", "content": content}));
}

fn assistant(blocks: &[Value], compat: &Compat) -> Value {
    let mut said = Vec::new();
    let mut reasoning = Vec::new();
    let mut calls: Vec<Value> = Vec::new();
    // What the server sent beside a call, by the call's id.
    let mut carried: Vec<(String, Value)> = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => said.extend(block["text"].as_str()),
            Some("thinking") => {
                let carrier = block["signature"].as_str().and_then(|s| s.strip_prefix(CARRIER));
                if let Some(Value::Object(kept)) = carrier.and_then(|c| serde_json::from_str(c).ok()) {
                    carried.extend(
                        kept.get("id").and_then(Value::as_str).map(str::to_owned).zip(kept.get("extra").cloned()),
                    );
                }
                reasoning.extend(block["thinking"].as_str().filter(|t| !t.is_empty()));
            }
            Some("tool_use") => {
                let arguments = serde_json::to_string(&block["input"]).unwrap_or_else(|_| "{}".into());
                calls.push(json!({
                    "id": block["id"],
                    "type": "function",
                    "function": {"name": block["name"], "arguments": arguments},
                }));
            }
            _ => {}
        }
    }
    for (id, extra) in carried {
        if let Some(call) = calls.iter_mut().find(|c| c["id"] == id.as_str()) {
            call["extra_content"] = extra;
        }
    }
    let mut message = json!({"role": "assistant", "content": said.join("\n\n")});
    if compat.replay_reasoning && !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.join("\n\n").into();
    }
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    message
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    fn request(messages: &[Value], tools: Option<Value>) -> ModelRequest {
        ModelRequest {
            model: "p/m".into(),
            max_tokens: 100,
            system: Bytes::from(json!([{"type": "text", "text": "Be brief."}]).to_string()),
            tools: tools.map(|t| Bytes::from(t.to_string())),
            messages: messages.iter().map(|m| Bytes::from(m.to_string())).collect(),
            thinking: None,
            effort: Some("high".into()),
            extra: None,
            cache_key: None,
            betas: &[],
            auth: None,
        }
    }

    #[test]
    fn a_tool_turn_becomes_chat_messages() {
        let carrier = format!("{CARRIER}{}", json!({"id": "c1", "extra": {"google": {"thought_signature": "SIG"}}}));
        let messages = [
            json!({"role": "user", "content": [{"type": "text", "text": "read it"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "I should read.", "signature": ""},
                {"type": "text", "text": "Reading."},
                {"type": "thinking", "thinking": "", "signature": carrier},
                {"type": "tool_use", "id": "c1", "name": "Read", "input": {"path": "a"}}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": "boom"}], "is_error": true},
                {"type": "text", "text": "go on"}]}),
        ];
        let tools = json!([
            {"name": "Read", "description": "Reads.", "input_schema": {"type": "object"}},
            {"type": "web_search_20250305", "name": "web_search"},
        ]);
        let compat = Compat { replay_reasoning: true, effort: true, ..Compat::default() };
        let body: Value = serde_json::from_slice(&build(&request(&messages, Some(tools)), "m", &compat).expect("body"))
            .expect("json");
        assert_eq!(body["model"], "m");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1), "a tool the provider runs is left out");
        assert_eq!(body["tools"][0]["function"]["name"], "Read");
        let sent = body["messages"].as_array().expect("messages");
        assert_eq!(sent[0], json!({"role": "system", "content": "Be brief."}));
        assert_eq!(sent[1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(sent[2]["content"], "Reading.");
        assert_eq!(sent[2]["reasoning_content"], "I should read.");
        assert_eq!(sent[2]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(sent[2]["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "SIG");
        assert_eq!(sent[3], json!({"role": "tool", "tool_call_id": "c1", "content": "Error: boom"}));
        assert_eq!(sent[4], json!({"role": "user", "content": "go on"}));

        let plain = Compat { images: false, max_tokens: MaxTokens::Omit, stream_usage: false, ..Compat::default() };
        let body: Value =
            serde_json::from_slice(&build(&request(&messages, None), "m", &plain).expect("body")).expect("json");
        assert!(body.get("max_tokens").is_none() && body.get("reasoning_effort").is_none());
        assert!(body.get("stream_options").is_none() && body.get("tools").is_none());
        assert!(body["messages"][1]["content"].as_str().is_some_and(|c| c.contains("[image omitted")));
        assert!(body["messages"][2].get("reasoning_content").is_none());
    }
}
