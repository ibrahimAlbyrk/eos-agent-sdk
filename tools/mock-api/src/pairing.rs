//! The Messages API's tool pairing rule, enforced as the API does: every
//! `tool_use` of an assistant message is answered by a `tool_result` in the
//! very next message, every `tool_result` answers a `tool_use` of the
//! message right before it, and no two calls share an id. A request that
//! breaks it is refused with the API's 400 and nothing is taken from the
//! script, so every test that drives an agent through the mock also checks
//! that the agent never sends such a conversation.

use std::collections::HashSet;

use serde_json::Value;

/// Why `body` would be refused (the error's message), when it would.
pub fn violation(body: &[u8]) -> Option<String> {
    let request: Value = serde_json::from_slice(body).ok()?;
    let messages = request.get("messages")?.as_array()?;
    let mut seen: HashSet<&str> = HashSet::new();
    for (i, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or_default();
        let blocks = blocks_of(message);
        let previous_calls = match i.checked_sub(1).map(|p| &messages[p]) {
            Some(prev) if prev.get("role").and_then(Value::as_str) == Some("assistant") => calls(prev),
            _ => Vec::new(),
        };
        for (k, block) in blocks.iter().enumerate() {
            match kind(block) {
                "tool_use" if role == "assistant" => {
                    let id = id_of(block, "id");
                    if !seen.insert(id) {
                        return Some(format!("messages.{i}.content.{k}: `tool_use` ids must be unique"));
                    }
                }
                "tool_result" => {
                    let id = id_of(block, "tool_use_id");
                    if role != "user" || !previous_calls.contains(&id) {
                        return Some(format!(
                            "messages.{i}.content.{k}: unexpected `tool_use_id` found in `tool_result` blocks: {id}. \
                             Each `tool_result` block must have a corresponding `tool_use` block in the previous message."
                        ));
                    }
                }
                _ => {}
            }
        }
        if role != "assistant" {
            continue;
        }
        let own = calls(message);
        if own.is_empty() {
            continue;
        }
        let answered: HashSet<&str> = messages
            .get(i + 1)
            .filter(|next| next.get("role").and_then(Value::as_str) == Some("user"))
            .map(|next| {
                blocks_of(next).iter().filter(|b| kind(b) == "tool_result").map(|b| id_of(b, "tool_use_id")).collect()
            })
            .unwrap_or_default();
        let missing: Vec<&str> = own.into_iter().filter(|id| !answered.contains(id)).collect();
        if !missing.is_empty() {
            return Some(format!(
                "messages.{i}: `tool_use` ids were found without `tool_result` blocks immediately after: {}. \
                 Each `tool_use` block must have a corresponding `tool_result` block in the next message.",
                missing.join(", ")
            ));
        }
    }
    None
}

/// The API's answer to a request [`violation`] refuses.
pub(crate) fn refusal(message: &str) -> String {
    serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message},
        "request_id": "req_mock_pairing",
    })
    .to_string()
}

fn blocks_of(message: &Value) -> Vec<&Value> {
    message.get("content").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default()
}

fn calls(message: &Value) -> Vec<&str> {
    blocks_of(message).into_iter().filter(|b| kind(b) == "tool_use").map(|b| id_of(b, "id")).collect()
}

fn kind(block: &Value) -> &str {
    block.get("type").and_then(Value::as_str).unwrap_or_default()
}

fn id_of<'a>(block: &'a Value, field: &str) -> &'a str {
    block.get(field).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(messages: Value) -> Vec<u8> {
        json!({"model": "m", "messages": messages}).to_string().into_bytes()
    }

    #[test]
    fn a_call_needs_its_result_in_the_next_message() {
        let call =
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "Write", "input": {}}]});
        let note = json!({"role": "user", "content": [{"type": "text", "text": "a note"}]});
        let result =
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]});
        let user = json!({"role": "user", "content": [{"type": "text", "text": "hi"}]});
        assert_eq!(violation(&body(json!([user, call, result]))), None);
        let refused = violation(&body(json!([user, call, note, result]))).expect("refused");
        assert!(
            refused.starts_with(
                "messages.1: `tool_use` ids were found without `tool_result` blocks immediately after: t1."
            ),
            "{refused}"
        );
        assert!(violation(&body(json!([user, call]))).is_some());
        let orphan = violation(&body(json!([user, result]))).expect("refused");
        assert!(orphan.starts_with("messages.1.content.0: unexpected `tool_use_id`"), "{orphan}");
        let twice = violation(&body(json!([user, call, result, call, result]))).expect("refused");
        assert!(twice.contains("ids must be unique"), "{twice}");
    }
}
