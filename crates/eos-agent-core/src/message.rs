use bytes::Bytes;
use eos_agent_types::{ContentBlock, Role};
use serde::Serialize;

/// Prompt-cache lifetime for a cache marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    fn marker(self) -> &'static str {
        match self {
            Self::FiveMinutes => r#","cache_control":{"type":"ephemeral"}"#,
            Self::OneHour => r#","cache_control":{"type":"ephemeral","ttl":"1h"}"#,
        }
    }
}

/// One message kept in its wire form: the exact `{"role":…,"content":[…]}`
/// bytes the API receives. It is serialized once when created and shared by
/// every later request (and by forks) without copying.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    role: Role,
    json: Bytes,
    /// When a transcript store recorded it (Unix ms), for messages read back
    /// from one. Never part of the bytes the API receives.
    recorded_ms: Option<u64>,
    /// See [`is_meta`](Self::is_meta).
    meta: bool,
}

#[derive(Serialize)]
struct Wire<'a> {
    role: Role,
    content: &'a [ContentBlock],
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("message has no content blocks")]
    Empty,
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl StoredMessage {
    pub fn new(role: Role, content: &[ContentBlock]) -> Result<Self, MessageError> {
        if content.is_empty() {
            return Err(MessageError::Empty);
        }
        let json = serde_json::to_vec(&Wire { role, content })?;
        // Kept for the agent's lifetime: drop the serializer's spare capacity
        // (up to half the buffer) instead of keeping it with every message.
        Ok(Self { role, json: Bytes::from(json.into_boxed_slice()), recorded_ms: None, meta: false })
    }

    pub fn user_text(text: impl Into<String>) -> Self {
        let block = ContentBlock::Text { text: text.into(), citations: None };
        Self::new(Role::User, std::slice::from_ref(&block)).expect("one text block always serializes")
    }

    /// Re-reads a message written by [`StoredMessage::json`] (e.g. from a
    /// transcript). Bytes already in the canonical layout are kept as they
    /// are (no copy, no re-serialization); anything else is normalized.
    pub fn from_wire(json: Bytes) -> Result<Self, MessageError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw<'a> {
            role: Role,
            #[serde(borrow)]
            content: &'a serde_json::value::RawValue,
        }
        if json.starts_with(br#"{"role":""#)
            && json.ends_with(b"}]}")
            && let Ok(raw) = serde_json::from_slice::<Raw<'_>>(&json)
            && raw.content.get().starts_with("[{")
        {
            return Ok(Self { role: raw.role, json, recorded_ms: None, meta: false });
        }
        #[derive(serde::Deserialize)]
        struct Head {
            role: Role,
            content: Vec<ContentBlock>,
        }
        let head: Head = serde_json::from_slice(&json)?;
        // Normalize, so the cache-marker splice can rely on the canonical layout.
        Self::new(head.role, &head.content)
    }

    /// A `role: "system"` message of one text block.
    pub fn system_text(text: impl Into<String>) -> Self {
        let block = ContentBlock::Text { text: text.into(), citations: None };
        Self::new(Role::System, std::slice::from_ref(&block)).expect("one text block always serializes")
    }

    /// Its text blocks, joined (empty when it has none or does not parse).
    pub fn text(&self) -> String {
        let blocks = self.blocks().unwrap_or_default();
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        texts.join("\n")
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// When the transcript store recorded it (Unix ms), if it was read back
    /// from a store that keeps the time.
    pub fn recorded_ms(&self) -> Option<u64> {
        self.recorded_ms
    }

    pub fn with_recorded_ms(mut self, ms: Option<u64>) -> Self {
        self.recorded_ms = ms;
        self
    }

    /// Context the host put into the conversation, not something the user
    /// said (Claude Code's `isMeta`): no prompt of theirs, and a conversation
    /// of such messages only is not opened yet (the session context comes
    /// with the next input). Kept by the transcript store; never part of the
    /// bytes the API receives.
    pub fn is_meta(&self) -> bool {
        self.meta
    }

    pub fn with_meta(mut self, meta: bool) -> Self {
        self.meta = meta;
        self
    }

    pub fn json(&self) -> &Bytes {
        &self.json
    }

    /// Rough token count (bytes / 4), for budgeting between API reports.
    pub fn estimated_tokens(&self) -> u64 {
        self.json.len() as u64 / 4
    }

    pub fn blocks(&self) -> Result<Vec<ContentBlock>, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Body {
            content: Vec<ContentBlock>,
        }
        Ok(serde_json::from_slice::<Body>(&self.json)?.content)
    }

    /// The message with a cache marker on its last content block. The canonical
    /// layout always ends `…}]}`, so the marker goes in before the last block's
    /// closing brace: one small copy instead of a re-serialization.
    pub fn with_cache_marker(&self, ttl: CacheTtl) -> Bytes {
        let json = &self.json[..];
        let at = json.len() - 3;
        debug_assert_eq!(&json[at..], b"}]}");
        let marker = ttl.marker().as_bytes();
        let mut out = Vec::with_capacity(json.len() + marker.len());
        out.extend_from_slice(&json[..at]);
        out.extend_from_slice(marker);
        out.extend_from_slice(&json[at..]);
        Bytes::from(out)
    }
}

/// The system prompt, serialized once into the `system` array the API takes.
/// Every request of an agent reuses the same bytes, so the cache prefix never
/// drifts by accident.
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    json: Bytes,
}

impl SystemPrompt {
    /// `blocks` are (text, cache marker) pairs, in order.
    pub fn new(blocks: &[(&str, Option<CacheTtl>)]) -> Self {
        let mut out = String::from("[");
        for (i, (text, ttl)) in blocks.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(r#"{"type":"text","text":"#);
            out.push_str(&serde_json::to_string(text).expect("a string always serializes"));
            if let Some(ttl) = ttl {
                out.push_str(ttl.marker());
            }
            out.push('}');
        }
        out.push(']');
        Self { json: Bytes::from(out) }
    }

    pub fn json(&self) -> &Bytes {
        &self.json
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_marker_lands_on_last_block() {
        let msg = StoredMessage::new(
            Role::User,
            &[
                ContentBlock::Text { text: "a".into(), citations: None },
                ContentBlock::Text { text: "b".into(), citations: None },
            ],
        )
        .expect("message");
        let marked = msg.with_cache_marker(CacheTtl::OneHour);
        let v: serde_json::Value = serde_json::from_slice(&marked).expect("valid json");
        assert!(v["content"][0].get("cache_control").is_none());
        assert_eq!(v["content"][1]["cache_control"]["ttl"], "1h");
        assert_eq!(v["content"][1]["text"], "b");
    }

    #[test]
    fn from_wire_normalizes_and_round_trips() {
        let raw = Bytes::from_static(br#"{ "role": "assistant", "content": [ {"type":"text","text":"hi"} ] }"#);
        let msg = StoredMessage::from_wire(raw).expect("parse");
        let again = StoredMessage::from_wire(msg.json().clone()).expect("parse");
        assert_eq!(again.json().as_ptr(), msg.json().as_ptr(), "canonical bytes are kept, not copied");
        assert_eq!(again.role(), Role::Assistant);
        assert_eq!(msg.json(), &Bytes::from_static(br#"{"role":"assistant","content":[{"type":"text","text":"hi"}]}"#));
        assert!(serde_json::from_slice::<serde_json::Value>(&msg.with_cache_marker(CacheTtl::FiveMinutes)).is_ok());
    }

    #[test]
    fn system_prompt_serializes_with_markers() {
        let sys = SystemPrompt::new(&[("You are \"x\".", Some(CacheTtl::OneHour)), ("more", None)]);
        let v: serde_json::Value = serde_json::from_slice(sys.json()).expect("valid json");
        assert_eq!(v[0]["text"], "You are \"x\".");
        assert_eq!(v[0]["cache_control"]["ttl"], "1h");
        assert!(v[1].get("cache_control").is_none());
    }
}
