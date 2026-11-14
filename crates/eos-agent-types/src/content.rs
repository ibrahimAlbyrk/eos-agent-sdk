use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::value::RawValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Mid-conversation system message (environment and tool-set changes).
    System,
}

/// One content block. Types the engine does not model are kept as raw JSON,
/// so a message read from history or the API is sent back byte-for-byte.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text { text: String, citations: Option<Box<RawValue>> },
    Thinking { thinking: String, signature: String },
    RedactedThinking { data: String },
    ToolUse { id: String, name: String, input: Box<RawValue> },
    ServerToolUse { id: String, name: String, input: Box<RawValue> },
    ToolResult { tool_use_id: String, content: Option<ToolResultContent>, is_error: bool },
    Unknown(Box<RawValue>),
}

#[derive(Debug, Clone)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl<'de> Deserialize<'de> for ToolResultContent {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(de)?;
        let json = raw.get();
        if json.starts_with('"') {
            serde_json::from_str(json).map(Self::Text).map_err(D::Error::custom)
        } else {
            serde_json::from_str(json).map(Self::Blocks).map_err(D::Error::custom)
        }
    }
}

impl Serialize for ToolResultContent {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(text) => text.serialize(ser),
            Self::Blocks(blocks) => blocks.serialize(ser),
        }
    }
}

impl ContentBlock {
    /// The block's `type` field as the API spells it (`None` for an unknown block
    /// whose raw JSON has no readable type).
    pub fn kind(&self) -> Option<&str> {
        Some(match self {
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::RedactedThinking { .. } => "redacted_thinking",
            Self::ToolUse { .. } => "tool_use",
            Self::ServerToolUse { .. } => "server_tool_use",
            Self::ToolResult { .. } => "tool_result",
            Self::Unknown(raw) => return type_of(raw.get()),
        })
    }
}

pub(crate) fn type_of(json: &str) -> Option<&str> {
    #[derive(Deserialize)]
    struct Tag<'a> {
        #[serde(borrow, rename = "type")]
        kind: &'a str,
    }
    serde_json::from_str::<Tag<'_>>(json).ok().map(|t| t.kind)
}

// Serde's tagged enums buffer their input, which `RawValue` cannot read from,
// so each known type is parsed by its own struct after the tag is peeked.
mod wire {
    use super::{RawValue, ToolResultContent};
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize)]
    pub(super) struct Text {
        pub text: String,
        #[serde(default)]
        pub citations: Option<Box<RawValue>>,
    }
    #[derive(Deserialize)]
    pub(super) struct Thinking {
        pub thinking: String,
        pub signature: String,
    }
    #[derive(Deserialize)]
    pub(super) struct RedactedThinking {
        pub data: String,
    }
    #[derive(Deserialize)]
    pub(super) struct ToolUse {
        pub id: String,
        pub name: String,
        pub input: Box<RawValue>,
    }
    #[derive(Deserialize)]
    pub(super) struct ToolResult {
        pub tool_use_id: String,
        #[serde(default)]
        pub content: Option<ToolResultContent>,
        #[serde(default)]
        pub is_error: bool,
    }

    #[derive(Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(super) enum Out<'a> {
        Text {
            text: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            citations: Option<&'a RawValue>,
        },
        Thinking {
            thinking: &'a str,
            signature: &'a str,
        },
        RedactedThinking {
            data: &'a str,
        },
        ToolUse {
            id: &'a str,
            name: &'a str,
            input: &'a RawValue,
        },
        ServerToolUse {
            id: &'a str,
            name: &'a str,
            input: &'a RawValue,
        },
        ToolResult {
            tool_use_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            content: Option<&'a ToolResultContent>,
            #[serde(skip_serializing_if = "std::ops::Not::not")]
            is_error: bool,
        },
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        fn parse<'a, T: Deserialize<'a>, E: serde::de::Error>(json: &'a str) -> Result<T, E> {
            serde_json::from_str(json).map_err(E::custom)
        }
        let raw = Box::<RawValue>::deserialize(de)?;
        let json = raw.get();
        let kind = type_of(json).ok_or_else(|| D::Error::missing_field("type"))?;
        Ok(match kind {
            "text" => {
                let b: wire::Text = parse(json)?;
                Self::Text { text: b.text, citations: b.citations }
            }
            "thinking" => {
                let b: wire::Thinking = parse(json)?;
                Self::Thinking { thinking: b.thinking, signature: b.signature }
            }
            "redacted_thinking" => Self::RedactedThinking { data: parse::<wire::RedactedThinking, _>(json)?.data },
            "tool_use" | "server_tool_use" => {
                let b: wire::ToolUse = parse(json)?;
                if kind == "tool_use" {
                    Self::ToolUse { id: b.id, name: b.name, input: b.input }
                } else {
                    Self::ServerToolUse { id: b.id, name: b.name, input: b.input }
                }
            }
            "tool_result" => {
                let b: wire::ToolResult = parse(json)?;
                Self::ToolResult { tool_use_id: b.tool_use_id, content: b.content, is_error: b.is_error }
            }
            _ => Self::Unknown(raw),
        })
    }
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use wire::Out;
        let out = match self {
            Self::Unknown(raw) => return raw.serialize(ser),
            Self::Text { text, citations } => Out::Text { text, citations: citations.as_deref() },
            Self::Thinking { thinking, signature } => Out::Thinking { thinking, signature },
            Self::RedactedThinking { data } => Out::RedactedThinking { data },
            Self::ToolUse { id, name, input } => Out::ToolUse { id, name, input },
            Self::ServerToolUse { id, name, input } => Out::ServerToolUse { id, name, input },
            Self::ToolResult { tool_use_id, content, is_error } => {
                Out::ToolResult { tool_use_id, content: content.as_ref(), is_error: *is_error }
            }
        };
        out.serialize(ser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(json: &str) -> String {
        let block: ContentBlock = serde_json::from_str(json).expect("parse");
        serde_json::to_string(&block).expect("serialize")
    }

    #[test]
    fn known_blocks_round_trip() {
        for json in [
            r#"{"type":"text","text":"hi"}"#,
            r#"{"type":"thinking","thinking":"hmm","signature":"sig"}"#,
            r#"{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}"#,
            r#"{"type":"tool_result","tool_use_id":"t1","content":"ok","is_error":true}"#,
            r#"{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"ok"}]}"#,
        ] {
            assert_eq!(roundtrip(json), json);
        }
    }

    #[test]
    fn unknown_block_is_kept_verbatim() {
        let json = r#"{"type":"web_search_tool_result","tool_use_id":"s1","content":[{"url":"https://x"}]}"#;
        let block: ContentBlock = serde_json::from_str(json).expect("parse");
        assert_eq!(block.kind(), Some("web_search_tool_result"));
        assert_eq!(serde_json::to_string(&block).expect("serialize"), json);
    }

    #[test]
    fn tool_input_stays_unparsed() {
        let block: ContentBlock =
            serde_json::from_str(r#"{"type":"tool_use","id":"t","name":"Bash","input":{"command":"ls"}}"#)
                .expect("parse");
        let ContentBlock::ToolUse { input, .. } = block else { panic!("not tool_use") };
        assert_eq!(input.get(), r#"{"command":"ls"}"#);
    }
}
