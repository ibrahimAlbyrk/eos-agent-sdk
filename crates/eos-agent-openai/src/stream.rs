//! Responses stream events as Messages stream events, so the loop's
//! assembler, the transcripts and the host protocol stay as they are.
//!
//! Reasoning comes back as a `thinking` block whose signature carries the
//! item's id and encrypted content (`oai1.<id>.<encrypted>`), which the
//! request side sends back verbatim on the next turn.

use std::borrow::Cow;

use eos_agent_core::ModelError;
use eos_agent_types::{ContentBlock, Delta, MessageStart, Role, StopReason, StreamEvent, Usage};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use crate::errors;

const SEAL: &str = "oai1.";

/// The signature of a reasoning item: its id and encrypted content.
pub(crate) fn seal(id: &str, encrypted: &str) -> String {
    format!("{SEAL}{id}.{encrypted}")
}

/// A reasoning item's id and encrypted content, from a signature this
/// adapter wrote (`None` for anything else, Claude's signatures included).
pub(crate) fn unseal(signature: &str) -> Option<(&str, &str)> {
    let rest = signature.strip_prefix(SEAL)?;
    let (id, encrypted) = rest.split_once('.')?;
    (!id.is_empty() && !encrypted.is_empty()).then_some((id, encrypted))
}

/// Did this adapter write the assistant message `json` (wire bytes)? Known
/// by the reasoning it holds, so a reply without reasoning says no.
pub fn wrote(json: &[u8]) -> bool {
    let sealed = format!(r#""signature":"{SEAL}"#);
    json.windows(sealed.len()).any(|w| w == sealed.as_bytes())
}

/// What one output item became.
#[derive(Debug)]
struct Item {
    /// The item's id, or its output index when it has none.
    key: String,
    kind: Kind,
    /// Our content block index, once one is open.
    block: Option<u32>,
    /// Text or arguments already streamed.
    streamed: bool,
    /// Summary parts seen (a reasoning item), for the separators between them.
    parts: u32,
    /// A custom tool's input, collected until the item is done.
    custom_input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Message,
    Reasoning,
    Function,
    Custom,
    Other,
}

/// Turns Responses events into Messages events.
#[derive(Debug, Default)]
pub(crate) struct Translator {
    items: Vec<Item>,
    next_block: u32,
    started: bool,
    calls: bool,
    refused: bool,
    finished: bool,
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default)]
    output_index: Option<u64>,
    #[serde(borrow, default)]
    item_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    delta: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    item: Option<&'a RawValue>,
    #[serde(borrow, default)]
    response: Option<&'a RawValue>,
}

#[derive(Deserialize, Default)]
struct OutputItem<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    call_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    name: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    arguments: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    input: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    encrypted_content: Option<Cow<'a, str>>,
    #[serde(default)]
    content: Vec<Part<'a>>,
}

#[derive(Deserialize, Default)]
struct Part<'a> {
    #[serde(rename = "type", borrow, default)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    refusal: Option<Cow<'a, str>>,
}

impl Translator {
    /// Did the response reach `response.completed` (or `.incomplete`)?
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// One event's data; what it stands for in Messages events, in order,
    /// goes to `out`.
    pub(crate) fn push(&mut self, data: &str, out: &mut Vec<StreamEvent>) -> Result<(), ModelError> {
        let event: Event<'_> =
            serde_json::from_str(data).map_err(|e| ModelError::Transient(format!("bad Responses event: {e}")))?;
        match event.kind.as_ref() {
            "response.created" | "response.in_progress" => self.start(event.response, out),
            "response.output_item.added" => {
                self.start(None, out);
                let item = parse_item(event.item);
                self.added(&item, event.output_index, out);
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if event.kind == "response.refusal.delta" {
                    self.refused = true;
                }
                let text = event.delta.unwrap_or_default();
                if !text.is_empty() {
                    let at = self.find_or_open(event.item_id.as_deref(), event.output_index, Kind::Message, out);
                    self.items[at].streamed = true;
                    let block = self.items[at].block.unwrap_or_default();
                    out.push(delta(block, Delta::Text(text.into_owned())));
                }
            }
            "response.reasoning_summary_part.added" => {
                let at = self.find_or_open(event.item_id.as_deref(), event.output_index, Kind::Reasoning, out);
                let item = &mut self.items[at];
                item.parts += 1;
                if item.parts > 1 {
                    out.push(delta(item.block.unwrap_or_default(), Delta::Thinking("\n\n".into())));
                }
            }
            "response.reasoning_summary_text.delta" => {
                let text = event.delta.unwrap_or_default();
                if !text.is_empty() {
                    let at = self.find_or_open(event.item_id.as_deref(), event.output_index, Kind::Reasoning, out);
                    self.items[at].streamed = true;
                    out.push(delta(self.items[at].block.unwrap_or_default(), Delta::Thinking(text.into_owned())));
                }
            }
            "response.function_call_arguments.delta" => {
                let json = event.delta.unwrap_or_default();
                if let Some(at) = self.find(event.item_id.as_deref(), event.output_index)
                    && !json.is_empty()
                {
                    self.items[at].streamed = true;
                    out.push(delta(self.items[at].block.unwrap_or_default(), Delta::InputJson(json.into_owned())));
                }
            }
            "response.custom_tool_call_input.delta" => {
                if let Some(at) = self.find(event.item_id.as_deref(), event.output_index) {
                    self.items[at].custom_input.push_str(&event.delta.unwrap_or_default());
                }
            }
            "response.output_item.done" => {
                let item = parse_item(event.item);
                self.done(&item, event.output_index, out);
            }
            "response.completed" => {
                let usage = usage(event.response);
                let reason = if self.calls {
                    StopReason::ToolUse
                } else if self.refused {
                    StopReason::Refusal
                } else {
                    StopReason::EndTurn
                };
                self.finish(reason, usage, out);
            }
            "response.incomplete" => {
                let response: Value =
                    event.response.and_then(|r| serde_json::from_str(r.get()).ok()).unwrap_or_default();
                let reason = match response["incomplete_details"]["reason"].as_str() {
                    Some("content_filter") => StopReason::Refusal,
                    _ => StopReason::MaxTokens,
                };
                self.finish(reason, usage(event.response), out);
            }
            "response.failed" => {
                let response: Value =
                    event.response.and_then(|r| serde_json::from_str(r.get()).ok()).unwrap_or_default();
                return Err(errors::from_error_object(&response["error"]));
            }
            "error" => {
                let value: Value = serde_json::from_str(data).unwrap_or_default();
                let error = if value["error"].is_object() { &value["error"] } else { &value };
                return Err(errors::from_error_object(error));
            }
            // in_progress, content parts, `.done` echoes, metadata, and
            // whatever comes later.
            _ => {}
        }
        Ok(())
    }

    fn start(&mut self, response: Option<&RawValue>, out: &mut Vec<StreamEvent>) {
        if std::mem::replace(&mut self.started, true) {
            return;
        }
        #[derive(Deserialize, Default)]
        struct Head {
            #[serde(default)]
            id: String,
            #[serde(default)]
            model: String,
        }
        let head: Head = response.and_then(|r| serde_json::from_str(r.get()).ok()).unwrap_or_default();
        out.push(StreamEvent::MessageStart(MessageStart {
            id: head.id,
            model: head.model,
            role: Role::Assistant,
            content: Vec::new(),
            stop_reason: None,
            usage: Usage::default(),
        }));
    }

    fn find(&self, id: Option<&str>, index: Option<u64>) -> Option<usize> {
        match id.filter(|i| !i.is_empty()) {
            Some(id) => self.items.iter().rposition(|i| i.key == id),
            None => {
                let index = index?;
                self.items.iter().rposition(|i| i.key.strip_prefix('#').and_then(|n| n.parse().ok()) == Some(index))
            }
        }
    }

    /// The item an event names, made (with its block open) when it is new.
    fn find_or_open(&mut self, id: Option<&str>, index: Option<u64>, kind: Kind, out: &mut Vec<StreamEvent>) -> usize {
        let at = match self.find(id, index) {
            Some(at) => at,
            None => {
                self.items.push(item(key(id, index).unwrap_or_default(), kind));
                self.items.len() - 1
            }
        };
        if self.items[at].block.is_none() {
            let block = match self.items[at].kind {
                Kind::Reasoning => ContentBlock::Thinking { thinking: String::new(), signature: String::new() },
                _ => ContentBlock::Text { text: String::new(), citations: None },
            };
            self.open(at, block, out);
        }
        at
    }

    fn open(&mut self, at: usize, block: ContentBlock, out: &mut Vec<StreamEvent>) {
        let index = self.next_block;
        self.next_block += 1;
        self.items[at].block = Some(index);
        out.push(StreamEvent::ContentBlockStart { index, block });
    }

    fn added(&mut self, raw: &OutputItem<'_>, index: Option<u64>, out: &mut Vec<StreamEvent>) {
        let kind = kind_of(&raw.kind);
        let key = key(raw.id.as_deref(), index).unwrap_or_default();
        self.items.push(item(key, kind));
        let at = self.items.len() - 1;
        match kind {
            Kind::Reasoning => {
                let block = ContentBlock::Thinking { thinking: String::new(), signature: String::new() };
                self.open(at, block, out);
            }
            Kind::Function | Kind::Custom => {
                self.calls = true;
                let empty = RawValue::from_string("{}".into()).expect("`{}` is JSON");
                let block = ContentBlock::ToolUse {
                    id: raw.call_id.as_deref().unwrap_or_default().to_owned(),
                    name: tool_name(raw.name.as_deref().unwrap_or_default()).to_owned(),
                    input: empty,
                };
                self.open(at, block, out);
            }
            // A message's text block opens with its first text, so an empty
            // message leaves no empty block behind.
            Kind::Message | Kind::Other => {}
        }
    }

    fn done(&mut self, raw: &OutputItem<'_>, index: Option<u64>, out: &mut Vec<StreamEvent>) {
        let at = match self.find(raw.id.as_deref(), index) {
            Some(at) => at,
            None => {
                // Done without added: a server that sends whole items only.
                self.added(raw, index, out);
                self.items.len() - 1
            }
        };
        match self.items[at].kind {
            Kind::Message => {
                if !self.items[at].streamed {
                    let text: String = raw
                        .content
                        .iter()
                        .filter_map(|p| match p.kind.as_ref() {
                            "output_text" => p.text.as_deref(),
                            "refusal" => p.refusal.as_deref(),
                            _ => None,
                        })
                        .collect();
                    if raw.content.iter().any(|p| p.kind == "refusal") {
                        self.refused = true;
                    }
                    if !text.is_empty() {
                        let at = self.find_or_open(raw.id.as_deref(), index, Kind::Message, out);
                        out.push(delta(self.items[at].block.unwrap_or_default(), Delta::Text(text)));
                    }
                }
            }
            Kind::Reasoning => {
                let block = self.items[at].block.unwrap_or_default();
                // Without encrypted content (or an id) the item cannot be
                // sent back; an empty signature keeps the block local.
                if let (Some(id), Some(encrypted)) = (raw.id.as_deref(), raw.encrypted_content.as_deref())
                    && !encrypted.is_empty()
                {
                    out.push(delta(block, Delta::Signature(seal(id, encrypted))));
                }
            }
            Kind::Function => {
                if !self.items[at].streamed {
                    let args = raw.arguments.as_deref().filter(|a| !a.trim().is_empty()).unwrap_or("{}");
                    out.push(delta(self.items[at].block.unwrap_or_default(), Delta::InputJson(args.to_owned())));
                }
            }
            Kind::Custom => {
                let input = match raw.input.as_deref() {
                    Some(whole) => whole.to_owned(),
                    None => std::mem::take(&mut self.items[at].custom_input),
                };
                let json = serde_json::json!({ "input": input }).to_string();
                out.push(delta(self.items[at].block.unwrap_or_default(), Delta::InputJson(json)));
            }
            Kind::Other => {}
        }
        if let Some(block) = self.items[at].block.take() {
            out.push(StreamEvent::ContentBlockStop { index: block });
        }
    }

    fn finish(&mut self, reason: StopReason, usage: Option<Usage>, out: &mut Vec<StreamEvent>) {
        self.start(None, out);
        for item in &mut self.items {
            if let Some(index) = item.block.take() {
                out.push(StreamEvent::ContentBlockStop { index });
            }
        }
        out.push(StreamEvent::MessageDelta { stop_reason: Some(reason), stop_sequence: None, usage });
        out.push(StreamEvent::MessageStop);
        self.finished = true;
    }
}

fn item(key: String, kind: Kind) -> Item {
    Item { key, kind, block: None, streamed: false, parts: 0, custom_input: String::new() }
}

fn key(id: Option<&str>, index: Option<u64>) -> Option<String> {
    id.filter(|i| !i.is_empty()).map(str::to_owned).or_else(|| index.map(|i| format!("#{i}")))
}

fn kind_of(kind: &str) -> Kind {
    match kind {
        "message" => Kind::Message,
        "reasoning" => Kind::Reasoning,
        "function_call" => Kind::Function,
        "custom_tool_call" => Kind::Custom,
        _ => Kind::Other,
    }
}

fn parse_item<'a>(raw: Option<&'a RawValue>) -> OutputItem<'a> {
    raw.and_then(|r| serde_json::from_str(r.get()).ok()).unwrap_or_default()
}

/// A tool's own name: namespaced names (`functions.Read`) lose the namespace.
fn tool_name(name: &str) -> &str {
    name.strip_prefix("functions.").or_else(|| name.strip_prefix("functions__")).unwrap_or(name)
}

fn delta(index: u32, delta: Delta) -> StreamEvent {
    StreamEvent::ContentBlockDelta { index, delta }
}

/// Responses usage in Messages terms: cached input is reported apart, as
/// cache reads, so the context size adds up as it does on Anthropic.
fn usage(response: Option<&RawValue>) -> Option<Usage> {
    let response: Value = response.and_then(|r| serde_json::from_str(r.get()).ok())?;
    let u = response.get("usage").filter(|u| u.is_object())?;
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let cached = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(input);
    Some(Usage {
        input_tokens: Some(input - cached),
        output_tokens: u["output_tokens"].as_u64(),
        cache_read_input_tokens: Some(cached),
        cache_creation_input_tokens: Some(0),
        ..Usage::default()
    })
}

#[cfg(test)]
mod tests {
    use eos_agent_core::Assembler;
    use serde_json::json;

    use super::*;

    fn run(events: &[Value]) -> (eos_agent_core::Assembled, Translator) {
        let mut t = Translator::default();
        let mut a = Assembler::default();
        for e in events {
            let mut out = Vec::new();
            t.push(&e.to_string(), &mut out).expect("event");
            for event in out {
                a.push(event);
            }
        }
        (a.finish(), t)
    }

    #[test]
    fn text_reasoning_and_calls_assemble_into_one_message() {
        let (done, t) = run(&[
            json!({"type":"response.created","response":{"id":"resp_1","model":"gpt-5.5"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":0}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"Think"}),
            json!({"type":"response.reasoning_summary_part.added","item_id":"rs_1","summary_index":1}),
            json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","delta":"More"}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"gAAA"}}),
            json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}),
            json!({"type":"response.content_part.added","item_id":"msg_1","part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"Hel"}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":"lo"}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"Hello"}]}}),
            json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Read","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"file_"}),
            json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"path\":\"/a\"}"}),
            json!({"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"Read","arguments":"{\"file_path\":\"/a\"}"}}),
            json!({"type":"response.completed","response":{"usage":{"input_tokens":1000,"input_tokens_details":{"cached_tokens":800},"output_tokens":50,"output_tokens_details":{"reasoning_tokens":30}}}}),
        ]);
        assert!(done.complete && t.finished());
        assert_eq!(done.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(done.content.len(), 3);
        let ContentBlock::Thinking { thinking, signature } = &done.content[0] else { panic!("thinking first") };
        assert_eq!((thinking.as_str(), signature.as_str()), ("Think\n\nMore", "oai1.rs_1.gAAA"));
        assert_eq!(unseal(signature.as_str()), Some(("rs_1", "gAAA")));
        assert!(matches!(&done.content[1], ContentBlock::Text { text, .. } if text == "Hello"));
        let ContentBlock::ToolUse { id, name, input } = &done.content[2] else { panic!("call") };
        assert_eq!((id.as_str(), name.as_str(), input.get()), ("call_1", "Read", r#"{"file_path":"/a"}"#));
        assert_eq!(done.usage.input_tokens, Some(200));
        assert_eq!(done.usage.cache_read_input_tokens, Some(800));
        assert_eq!(done.usage.output_tokens, Some(50));
        assert_eq!(done.usage.context_tokens(), 1000);
    }

    #[test]
    fn whole_items_namespaces_and_incomplete_responses() {
        let (done, _) = run(&[
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"m","content":[{"type":"output_text","text":"Hi"}]}}),
            json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"f","call_id":"c","name":"functions.Grep","arguments":"{}"}}),
            json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}),
        ]);
        assert_eq!(done.stop_reason, Some(StopReason::MaxTokens));
        assert!(matches!(&done.content[1], ContentBlock::ToolUse { name, .. } if name == "Grep"));
        let (done, _) = run(&[
            json!({"type":"response.refusal.delta","item_id":"m","delta":"I can't."}),
            json!({"type":"response.completed","response":{}}),
        ]);
        assert_eq!(done.stop_reason, Some(StopReason::Refusal));
        assert!(matches!(&done.content[0], ContentBlock::Text { text, .. } if text == "I can't."));
    }

    #[test]
    fn reasoning_without_encrypted_content_stays_local() {
        let (done, _) = run(&[
            json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_9"}}),
            json!({"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_9"}}),
            json!({"type":"response.completed","response":{}}),
        ]);
        assert!(matches!(&done.content[0], ContentBlock::Thinking { signature, .. } if signature.is_empty()));
        assert_eq!(done.stop_reason, Some(StopReason::EndTurn));
    }

    #[test]
    fn failures_map_to_model_errors() {
        let mut t = Translator::default();
        let failed = json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}});
        assert!(matches!(t.push(&failed.to_string(), &mut Vec::new()), Err(ModelError::LimitReached { .. })));
        let error = json!({"type":"error","code":"context_length_exceeded","message":"too long"});
        assert!(matches!(t.push(&error.to_string(), &mut Vec::new()), Err(ModelError::ContextWindowExceeded(_))));
        assert_eq!(unseal("EqQBClaude"), None);
        assert_eq!(unseal("oai1..x"), None);
    }
}
