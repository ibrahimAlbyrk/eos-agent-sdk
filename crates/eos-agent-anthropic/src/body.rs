use bytes::Bytes;
use eos_agent_core::{ModelRequest, Thinking};

/// The request body as a list of pieces: the pre-serialized system prompt,
/// tools and messages are shared, not copied (a request of a thousand agents
/// with the same tools holds one copy of their definitions), with the small
/// pieces of JSON between them.
pub(crate) fn build(req: &ModelRequest) -> Vec<Bytes> {
    let mut out = Vec::with_capacity(req.messages.len() * 2 + 12);
    let model = serde_json::to_string(&req.model).expect("a string serializes");
    out.push(Bytes::from(format!("{{\"model\":{model},\"max_tokens\":{},\"stream\":true,\"system\":", req.max_tokens)));
    out.push(req.system.clone());
    if let Some(tools) = &req.tools {
        out.push(Bytes::from_static(b",\"tools\":"));
        out.push(tools.clone());
    }
    out.push(Bytes::from_static(b",\"messages\":["));
    for (i, m) in req.messages.iter().enumerate() {
        if i > 0 {
            out.push(Bytes::from_static(b","));
        }
        out.push(m.clone());
    }
    let mut tail = String::from("]");
    match req.thinking {
        Some(Thinking::Adaptive { summarized }) => {
            let display = if summarized { "summarized" } else { "omitted" };
            tail.push_str(&format!(",\"thinking\":{{\"type\":\"adaptive\",\"display\":\"{display}\"}}"));
        }
        Some(Thinking::Enabled { budget_tokens, summarized }) => {
            let display = if summarized { "summarized" } else { "omitted" };
            tail.push_str(&format!(
                ",\"thinking\":{{\"budget_tokens\":{budget_tokens},\"type\":\"enabled\",\"display\":\"{display}\"}}"
            ));
        }
        Some(Thinking::Disabled) => tail.push_str(",\"thinking\":{\"type\":\"disabled\"}"),
        None => {}
    }
    if let Some(effort) = &req.effort {
        let effort = serde_json::to_string(effort).expect("a string serializes");
        tail.push_str(&format!(",\"output_config\":{{\"effort\":{effort}}}"));
    }
    out.push(Bytes::from(tail));
    if let Some(extra) = &req.extra {
        let inner = extra.strip_prefix(b"{").and_then(|e| e.strip_suffix(b"}")).unwrap_or(&[]);
        if inner.iter().any(|b| !b.is_ascii_whitespace()) {
            out.push(Bytes::from_static(b","));
            out.push(extra.slice_ref(inner));
        }
    }
    out.push(Bytes::from_static(b"}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_json() {
        let req = ModelRequest {
            model: "claude-x".into(),
            max_tokens: 100,
            system: Bytes::from_static(br#"[{"type":"text","text":"s"}]"#),
            tools: Some(Bytes::from_static(br#"[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#)),
            messages: vec![
                Bytes::from_static(br#"{"role":"user","content":[{"type":"text","text":"a"}]}"#),
                Bytes::from_static(br#"{"role":"assistant","content":[{"type":"text","text":"b"}]}"#),
            ],
            thinking: Some(Thinking::Adaptive { summarized: true }),
            effort: Some("high".into()),
            extra: Some(Bytes::from_static(br#"{"metadata":{"user_id":"u"}}"#)),
            cache_key: None,
            betas: &[],
            auth: None,
        };
        let v: serde_json::Value = serde_json::from_slice(&build(&req).concat()).expect("valid json");
        assert_eq!(v["model"], "claude-x");
        assert_eq!(v["stream"], true);
        assert_eq!(v["messages"][1]["role"], "assistant");
        assert_eq!(v["thinking"]["display"], "summarized");
        assert_eq!(v["output_config"]["effort"], "high");
        assert_eq!(v["metadata"]["user_id"], "u");
        assert_eq!(v["tools"][0]["name"], "t");
    }
}
