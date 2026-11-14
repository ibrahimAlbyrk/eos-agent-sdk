use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

use crate::content::{ContentBlock, Role, type_of};
use crate::usage::Usage;

/// Why the model stopped. Reasons the API adds later keep their name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Compaction,
    Refusal,
    ModelContextWindowExceeded,
    Other(String),
}

impl StopReason {
    const NAMES: [(&'static str, StopReason); 8] = [
        ("end_turn", Self::EndTurn),
        ("max_tokens", Self::MaxTokens),
        ("stop_sequence", Self::StopSequence),
        ("tool_use", Self::ToolUse),
        ("pause_turn", Self::PauseTurn),
        ("compaction", Self::Compaction),
        ("refusal", Self::Refusal),
        ("model_context_window_exceeded", Self::ModelContextWindowExceeded),
    ];

    pub fn as_str(&self) -> &str {
        match self {
            Self::Other(name) => name,
            known => Self::NAMES.iter().find(|(_, r)| r == known).map_or("", |(n, _)| n),
        }
    }

    pub fn from_name(name: &str) -> Self {
        Self::NAMES.iter().find(|(n, _)| *n == name).map_or_else(|| Self::Other(name.to_owned()), |(_, r)| r.clone())
    }
}

impl Serialize for StopReason {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for StopReason {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let name = std::borrow::Cow::<str>::deserialize(de)?;
        Ok(Self::from_name(&name))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageStart {
    pub id: String,
    pub model: String,
    pub role: Role,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// An incremental change to one content block.
#[derive(Debug, Clone)]
pub enum Delta {
    Text(String),
    InputJson(String),
    Thinking(String),
    Signature(String),
    Citation(Box<RawValue>),
    Unknown(Box<RawValue>),
}

/// One server-sent event of a streamed Messages response.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    MessageStart(MessageStart),
    ContentBlockStart {
        index: u32,
        block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: Delta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        stop_sequence: Option<String>,
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    Error(ApiError),
    /// An event type this version does not know; ignored by the loop.
    Unknown {
        name: String,
    },
}

#[derive(Debug, thiserror::Error)]
#[error("bad `{event}` event: {source}")]
pub struct StreamParseError {
    pub event: String,
    #[source]
    pub source: serde_json::Error,
}

mod wire {
    use super::*;

    #[derive(Deserialize)]
    pub(super) struct Start {
        pub message: MessageStart,
    }
    #[derive(Deserialize)]
    pub(super) struct BlockStart {
        pub index: u32,
        pub content_block: ContentBlock,
    }
    #[derive(Deserialize)]
    pub(super) struct BlockDelta<'a> {
        pub index: u32,
        #[serde(borrow)]
        pub delta: &'a RawValue,
    }
    #[derive(Deserialize)]
    pub(super) struct Index {
        pub index: u32,
    }
    #[derive(Deserialize, Default)]
    pub(super) struct MessageDeltaBody {
        #[serde(default)]
        pub stop_reason: Option<StopReason>,
        #[serde(default)]
        pub stop_sequence: Option<String>,
    }
    #[derive(Deserialize)]
    pub(super) struct MessageDelta {
        #[serde(default)]
        pub delta: MessageDeltaBody,
        #[serde(default)]
        pub usage: Option<Usage>,
    }
    #[derive(Deserialize)]
    pub(super) struct Error {
        pub error: ApiError,
    }
    #[derive(Deserialize)]
    pub(super) struct Field<'a> {
        #[serde(default, borrow)]
        pub text: Option<std::borrow::Cow<'a, str>>,
        #[serde(default, borrow)]
        pub partial_json: Option<std::borrow::Cow<'a, str>>,
        #[serde(default, borrow)]
        pub thinking: Option<std::borrow::Cow<'a, str>>,
        #[serde(default, borrow)]
        pub signature: Option<std::borrow::Cow<'a, str>>,
        #[serde(default)]
        pub citation: Option<Box<RawValue>>,
    }

    #[derive(Serialize)]
    #[serde(tag = "type")]
    pub(super) enum OutDelta<'a> {
        #[serde(rename = "text_delta")]
        Text { text: &'a str },
        #[serde(rename = "input_json_delta")]
        InputJson { partial_json: &'a str },
        #[serde(rename = "thinking_delta")]
        Thinking { thinking: &'a str },
        #[serde(rename = "signature_delta")]
        Signature { signature: &'a str },
        #[serde(rename = "citations_delta")]
        Citations { citation: &'a RawValue },
    }

    #[derive(Serialize)]
    pub(super) struct OutMessageDelta<'a> {
        pub stop_reason: Option<&'a StopReason>,
        pub stop_sequence: Option<&'a str>,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(super) enum Out<'a> {
        MessageStart {
            message: &'a MessageStart,
        },
        ContentBlockStart {
            index: u32,
            content_block: &'a ContentBlock,
        },
        ContentBlockDelta {
            index: u32,
            delta: &'a Delta,
        },
        ContentBlockStop {
            index: u32,
        },
        MessageDelta {
            delta: OutMessageDelta<'a>,
            #[serde(skip_serializing_if = "Option::is_none")]
            usage: Option<&'a Usage>,
        },
        MessageStop,
        Ping,
        Error {
            error: &'a ApiError,
        },
    }
}

impl Serialize for Delta {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use wire::OutDelta;
        match self {
            Self::Text(text) => OutDelta::Text { text }.serialize(ser),
            Self::InputJson(partial_json) => OutDelta::InputJson { partial_json }.serialize(ser),
            Self::Thinking(thinking) => OutDelta::Thinking { thinking }.serialize(ser),
            Self::Signature(signature) => OutDelta::Signature { signature }.serialize(ser),
            Self::Citation(citation) => OutDelta::Citations { citation }.serialize(ser),
            Self::Unknown(raw) => raw.serialize(ser),
        }
    }
}

impl Delta {
    fn parse(raw: &RawValue) -> Result<Self, serde_json::Error> {
        let json = raw.get();
        let field: wire::Field<'_> = serde_json::from_str(json)?;
        let owned = |v: Option<std::borrow::Cow<'_, str>>| v.map(std::borrow::Cow::into_owned).unwrap_or_default();
        Ok(match type_of(json) {
            Some("text_delta") => Self::Text(owned(field.text)),
            Some("input_json_delta") => Self::InputJson(owned(field.partial_json)),
            Some("thinking_delta") => Self::Thinking(owned(field.thinking)),
            Some("signature_delta") => Self::Signature(owned(field.signature)),
            Some("citations_delta") if field.citation.is_some() => {
                Self::Citation(field.citation.unwrap_or_else(|| raw.to_owned()))
            }
            _ => Self::Unknown(raw.to_owned()),
        })
    }
}

impl StreamEvent {
    /// Parses one SSE event from its `event:` name and `data:` payload. The
    /// name selects the shape, so the payload is read exactly once.
    pub fn parse(event: &str, data: &str) -> Result<Self, StreamParseError> {
        let wrap = |source| StreamParseError { event: event.to_owned(), source };
        Ok(match event {
            "message_start" => Self::MessageStart(serde_json::from_str::<wire::Start>(data).map_err(wrap)?.message),
            "content_block_start" => {
                let s: wire::BlockStart = serde_json::from_str(data).map_err(wrap)?;
                Self::ContentBlockStart { index: s.index, block: s.content_block }
            }
            "content_block_delta" => {
                let d: wire::BlockDelta<'_> = serde_json::from_str(data).map_err(wrap)?;
                Self::ContentBlockDelta { index: d.index, delta: Delta::parse(d.delta).map_err(wrap)? }
            }
            "content_block_stop" => {
                Self::ContentBlockStop { index: serde_json::from_str::<wire::Index>(data).map_err(wrap)?.index }
            }
            "message_delta" => {
                let d: wire::MessageDelta = serde_json::from_str(data).map_err(wrap)?;
                Self::MessageDelta {
                    stop_reason: d.delta.stop_reason,
                    stop_sequence: d.delta.stop_sequence,
                    usage: d.usage,
                }
            }
            "message_stop" => Self::MessageStop,
            "ping" => Self::Ping,
            "error" => Self::Error(serde_json::from_str::<wire::Error>(data).map_err(wrap)?.error),
            other => Self::Unknown { name: other.to_owned() },
        })
    }

    /// The SSE `event:` name.
    pub fn name(&self) -> &str {
        match self {
            Self::MessageStart(_) => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error(_) => "error",
            Self::Unknown { name } => name,
        }
    }

    /// The SSE `data:` payload, as the API would send it. `None` for unknown events.
    pub fn to_json(&self) -> Option<String> {
        use wire::Out;
        let out = match self {
            Self::MessageStart(message) => Out::MessageStart { message },
            Self::ContentBlockStart { index, block } => Out::ContentBlockStart { index: *index, content_block: block },
            Self::ContentBlockDelta { index, delta } => Out::ContentBlockDelta { index: *index, delta },
            Self::ContentBlockStop { index } => Out::ContentBlockStop { index: *index },
            Self::MessageDelta { stop_reason, stop_sequence, usage } => Out::MessageDelta {
                delta: wire::OutMessageDelta {
                    stop_reason: stop_reason.as_ref(),
                    stop_sequence: stop_sequence.as_deref(),
                },
                usage: usage.as_ref(),
            },
            Self::MessageStop => Out::MessageStop,
            Self::Ping => Out::Ping,
            Self::Error(error) => Out::Error { error },
            Self::Unknown { .. } => return None,
        };
        serde_json::to_string(&out).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_standard_sequence() {
        let events = [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","model":"claude-x","content":[],"stop_reason":null,"usage":{"input_tokens":3,"output_tokens":1}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ];
        let parsed: Vec<_> = events.iter().map(|(e, d)| StreamEvent::parse(e, d).expect("parse")).collect();
        assert!(matches!(&parsed[2], StreamEvent::ContentBlockDelta { index: 0, delta: Delta::Text(t) } if t == "Hi"));
        assert!(matches!(&parsed[4], StreamEvent::MessageDelta { stop_reason: Some(StopReason::EndTurn), .. }));
    }

    #[test]
    fn unknown_stop_reason_and_delta_survive() {
        let e = StreamEvent::parse("message_delta", r#"{"delta":{"stop_reason":"brand_new"}}"#).expect("parse");
        assert!(
            matches!(e, StreamEvent::MessageDelta { stop_reason: Some(StopReason::Other(ref s)), .. } if s == "brand_new")
        );
        let d = r#"{"index":1,"delta":{"type":"future_delta","x":1}}"#;
        let StreamEvent::ContentBlockDelta { delta, .. } = StreamEvent::parse("content_block_delta", d).expect("parse")
        else {
            panic!("not a delta")
        };
        assert_eq!(serde_json::to_string(&delta).expect("serialize"), r#"{"type":"future_delta","x":1}"#);
    }

    #[test]
    fn serializes_back_to_api_shape() {
        let e = StreamEvent::ContentBlockDelta { index: 2, delta: Delta::InputJson("{\"a\"".into()) };
        assert_eq!(
            e.to_json().as_deref(),
            Some(
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\""}}"#
            )
        );
        let reparsed = StreamEvent::parse(e.name(), &e.to_json().expect("json")).expect("parse");
        assert!(
            matches!(reparsed, StreamEvent::ContentBlockDelta { index: 2, delta: Delta::InputJson(ref s) } if s == "{\"a\"")
        );
    }
}
