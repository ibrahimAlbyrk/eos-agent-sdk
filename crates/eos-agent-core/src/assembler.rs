use std::sync::Arc;

use eos_agent_types::{ContentBlock, Delta, StopReason, StreamEvent, Usage};
use serde_json::value::RawValue;

use crate::ports::{AgentEvent, BlockKind, ResponseUsage};

/// Builds the assistant message from stream events, one buffer per content
/// block, and turns deltas into host events on the way.
#[derive(Debug, Default)]
pub struct Assembler {
    blocks: Vec<Option<Partial>>,
    stop_reason: Option<StopReason>,
    usage: Usage,
    model: String,
    stopped: bool,
}

/// What one stream event tells the host: up to two events, in order (a
/// block's end, then its tool call).
pub type Emitted = [Option<AgentEvent>; 2];

fn emit(first: Option<AgentEvent>, second: Option<AgentEvent>) -> Emitted {
    [first, second]
}

#[derive(Debug)]
enum Partial {
    Text { text: String, citations: Vec<Box<RawValue>> },
    Thinking { thinking: String, signature: String },
    ToolUse { id: String, name: String, json: String, server: bool },
    Whole(ContentBlock),
}

/// A finished (or cut-off) assistant message.
#[derive(Debug)]
pub struct Assembled {
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
    /// Did the stream reach `message_stop`?
    pub complete: bool,
}

impl Assembler {
    /// Feeds one event; returns what the host should hear about it.
    pub fn push(&mut self, event: StreamEvent) -> Emitted {
        match event {
            StreamEvent::MessageStart(start) => {
                self.usage.merge(&start.usage);
                self.model = start.model;
                emit(None, None)
            }
            StreamEvent::ContentBlockStart { index, block } => {
                let kind = match &block {
                    ContentBlock::Text { .. } => BlockKind::Text,
                    ContentBlock::Thinking { .. } => BlockKind::Thinking,
                    ContentBlock::ToolUse { id, name, .. } => BlockKind::ToolUse { id: id.clone(), name: name.clone() },
                    ContentBlock::ServerToolUse { id, name, .. } => {
                        BlockKind::ServerToolUse { id: id.clone(), name: name.clone() }
                    }
                    other => BlockKind::Other(other.kind().unwrap_or_default().to_owned()),
                };
                let partial = match block {
                    ContentBlock::Text { text, .. } => Partial::Text { text, citations: Vec::new() },
                    ContentBlock::Thinking { thinking, signature } => Partial::Thinking { thinking, signature },
                    ContentBlock::ToolUse { id, name, .. } => {
                        Partial::ToolUse { id, name, json: String::new(), server: false }
                    }
                    ContentBlock::ServerToolUse { id, name, .. } => {
                        Partial::ToolUse { id, name, json: String::new(), server: true }
                    }
                    other => Partial::Whole(other),
                };
                let i = index as usize;
                if self.blocks.len() <= i {
                    self.blocks.resize_with(i + 1, || None);
                }
                self.blocks[i] = Some(partial);
                emit(Some(AgentEvent::BlockStart { index, block: kind }), None)
            }
            StreamEvent::ContentBlockDelta { index, delta } => emit(self.delta(index as usize, delta), None),
            StreamEvent::ContentBlockStop { index } => {
                let call = match self.blocks.get(index as usize) {
                    Some(Some(Partial::ToolUse { id, name, json, server: false })) => Some(AgentEvent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: Arc::from(tool_input(json)),
                    }),
                    _ => None,
                };
                emit(Some(AgentEvent::BlockStop { index }), call)
            }
            StreamEvent::MessageDelta { stop_reason, usage, .. } => {
                if stop_reason.is_some() {
                    self.stop_reason = stop_reason;
                }
                if let Some(usage) = usage {
                    self.usage.merge(&usage);
                }
                emit(None, None)
            }
            StreamEvent::MessageStop => {
                self.stopped = true;
                let usage = ResponseUsage { usage: self.usage.clone(), model: std::mem::take(&mut self.model) };
                emit(Some(AgentEvent::Usage(Box::new(usage))), None)
            }
            StreamEvent::Ping | StreamEvent::Error(_) | StreamEvent::Unknown { .. } => emit(None, None),
        }
    }

    fn delta(&mut self, index: usize, delta: Delta) -> Option<AgentEvent> {
        let block = self.blocks.get_mut(index)?.as_mut()?;
        match (block, delta) {
            (Partial::Text { text, .. }, Delta::Text(more)) => {
                text.push_str(&more);
                Some(AgentEvent::TextDelta { text: more })
            }
            (Partial::Text { citations, .. }, Delta::Citation(c)) => {
                citations.push(c);
                None
            }
            (Partial::Thinking { thinking, .. }, Delta::Thinking(more)) => {
                thinking.push_str(&more);
                Some(AgentEvent::ThinkingDelta { text: more })
            }
            (Partial::Thinking { signature, .. }, Delta::Signature(sig)) => {
                signature.push_str(&sig);
                None
            }
            (Partial::ToolUse { id, json, server, .. }, Delta::InputJson(more)) => {
                json.push_str(&more);
                (!*server).then(|| AgentEvent::ToolInputDelta { id: id.clone(), partial_json: more })
            }
            _ => None,
        }
    }

    pub fn finish(self) -> Assembled {
        let content = self.blocks.into_iter().flatten().map(Partial::into_block).collect();
        Assembled { content, stop_reason: self.stop_reason, usage: self.usage, complete: self.stopped }
    }
}

impl Partial {
    fn into_block(self) -> ContentBlock {
        match self {
            Self::Text { text, citations } => {
                let citations = (!citations.is_empty()).then(|| {
                    let joined = format!("[{}]", citations.iter().map(|c| c.get()).collect::<Vec<_>>().join(","));
                    RawValue::from_string(joined).expect("citations are JSON values")
                });
                ContentBlock::Text { text, citations }
            }
            Self::Thinking { thinking, signature } => ContentBlock::Thinking { thinking, signature },
            Self::ToolUse { id, name, json, server: false } => {
                ContentBlock::ToolUse { id, name, input: tool_input(&json) }
            }
            Self::ToolUse { id, name, json, server: true } => {
                ContentBlock::ServerToolUse { id, name, input: tool_input(&json) }
            }
            Self::Whole(block) => block,
        }
    }
}

/// The streamed input JSON; `{}` when empty or malformed, so the call can still
/// get a result instead of breaking the conversation.
fn tool_input(json: &str) -> Box<RawValue> {
    let json = if json.trim().is_empty() { "{}" } else { json };
    RawValue::from_string(json.to_owned())
        .ok()
        .filter(|v| v.get().starts_with('{'))
        .unwrap_or_else(|| RawValue::from_string("{}".into()).expect("`{}` is JSON"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str, data: &str) -> StreamEvent {
        StreamEvent::parse(name, data).expect("event")
    }

    #[test]
    fn assembles_text_and_tool_use() {
        let mut a = Assembler::default();
        let events = [
            ev("message_start", r#"{"message":{"id":"m","model":"x","role":"assistant","usage":{"input_tokens":5}}}"#),
            ev("content_block_start", r#"{"index":0,"content_block":{"type":"text","text":""}}"#),
            ev("content_block_delta", r#"{"index":0,"delta":{"type":"text_delta","text":"Hel"}}"#),
            ev("content_block_delta", r#"{"index":0,"delta":{"type":"text_delta","text":"lo"}}"#),
            ev("content_block_stop", r#"{"index":0}"#),
            ev(
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}"#,
            ),
            ev(
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            ),
            ev("content_block_delta", r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#),
            ev("content_block_stop", r#"{"index":1}"#),
            ev("message_delta", r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#),
            ev("message_stop", r#"{}"#),
        ];
        let out: Vec<AgentEvent> = events.into_iter().flat_map(|e| a.push(e)).flatten().collect();
        let shape: Vec<String> = out
            .iter()
            .map(|e| match e {
                AgentEvent::BlockStart { index, block } => format!("start {index} {block:?}"),
                AgentEvent::BlockStop { index } => format!("stop {index}"),
                AgentEvent::TextDelta { text } => format!("text {text}"),
                AgentEvent::ToolInputDelta { id, .. } => format!("input {id}"),
                AgentEvent::ToolUse { name, input, .. } => format!("call {name} {}", input.get()),
                AgentEvent::Usage(u) => format!("usage {}", u.model),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "start 0 Text",
                "text Hel",
                "text lo",
                "stop 0",
                r#"start 1 ToolUse { id: "t1", name: "Bash" }"#,
                "input t1",
                "input t1",
                "stop 1",
                r#"call Bash {"command":"ls"}"#,
                "usage x",
            ]
        );
        let done = a.finish();
        assert!(done.complete);
        assert_eq!(done.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(done.usage.input_tokens, Some(5));
        assert_eq!(done.usage.output_tokens, Some(9));
        assert!(matches!(&done.content[0], ContentBlock::Text { text, .. } if text == "Hello"));
        assert!(matches!(&done.content[1], ContentBlock::ToolUse { name, .. } if name == "Bash"));
    }

    #[test]
    fn malformed_tool_json_becomes_empty_object() {
        assert_eq!(tool_input(r#"{"a":"#).get(), "{}");
        assert_eq!(tool_input("").get(), "{}");
    }
}
