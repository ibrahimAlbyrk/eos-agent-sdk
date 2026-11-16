//! A Messages-shaped [`ModelRequest`] as a Responses API body.
//!
//! The history stays in Messages form; each message is read with borrowed
//! raw values and written out as Responses items. Both APIs escape JSON
//! strings the same way, so text, tool results and images are copied as raw
//! slices (large ones are not copied at all: the body holds a slice of the
//! stored message). The system prompt and tool list, shared by many agents,
//! are translated once per distinct value and kept.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Mutex;

use bytes::{Bytes, BytesMut};
use eos_agent_core::{ModelError, ModelRequest, Thinking};
use serde::Deserialize;
use serde_json::value::RawValue;

/// Where function tools go: Sign in with ChatGPT takes them only grouped in
/// a namespace (or as `additional_tools` input items).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolPlacement {
    /// One `functions` namespace in `tools` (Codex's default namespace).
    #[default]
    Namespace,
    /// Plain function tools in `tools` (an API key's usual shape).
    TopLevel,
    /// An `additional_tools` developer item ahead of the history (Codex's
    /// "responses lite" models).
    AdditionalTools,
}

impl ToolPlacement {
    /// `namespace`, `top` or `additional`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "namespace" => Some(Self::Namespace),
            "top" | "top-level" => Some(Self::TopLevel),
            "additional" | "additional_tools" => Some(Self::AdditionalTools),
            _ => None,
        }
    }
}

/// Codex's namespace for top-level function tools.
pub(crate) const NAMESPACE: &str = "functions";

/// Pieces shorter than this are copied into the running buffer; longer ones
/// go into the body as slices of the bytes they came from.
const INLINE: usize = 512;

/// The body as pieces: small JSON joined in one buffer (each flush splits
/// off what was written, so the pieces share its allocation), large values
/// shared with the bytes they came from.
struct Out {
    pieces: Vec<Bytes>,
    buf: BytesMut,
}

impl Out {
    /// For a body of about `size` bytes.
    fn with_capacity(size: usize, pieces: usize) -> Self {
        Self { pieces: Vec::with_capacity(pieces), buf: BytesMut::with_capacity(size.min(1 << 20) + 1024) }
    }

    fn s(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// `part` must lie inside `src`.
    fn raw(&mut self, src: &Bytes, part: &str) {
        if part.len() <= INLINE {
            self.buf.extend_from_slice(part.as_bytes());
        } else {
            self.flush();
            self.pieces.push(src.slice_ref(part.as_bytes()));
        }
    }

    fn shared(&mut self, bytes: Bytes) {
        self.flush();
        self.pieces.push(bytes);
    }

    fn flush(&mut self) {
        if !self.buf.is_empty() {
            self.pieces.push(self.buf.split().freeze());
        }
    }

    fn finish(mut self) -> Vec<Bytes> {
        self.flush();
        self.pieces
    }
}

/// Translations of shared values (system prompts, tool lists), keyed by the
/// bytes themselves: a held clone pins the allocation, so a pointer match is
/// exact. Most-recently-used first.
#[derive(Debug, Default)]
pub(crate) struct Memo {
    entries: Mutex<VecDeque<(Bytes, Bytes)>>,
}

/// Distinct system prompts and tool lists kept translated.
const MEMO_ENTRIES: usize = 64;

impl Memo {
    fn get_or(&self, key: &Bytes, make: impl FnOnce() -> Result<Bytes, ModelError>) -> Result<Bytes, ModelError> {
        let same = |k: &Bytes| k.as_ptr() == key.as_ptr() && k.len() == key.len();
        {
            let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(at) = entries.iter().position(|(k, _)| same(k)) {
                let hit = entries.remove(at).expect("position is in range");
                let value = hit.1.clone();
                entries.push_front(hit);
                return Ok(value);
            }
        }
        let value = make()?;
        let mut entries = self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.push_front((key.clone(), value.clone()));
        entries.truncate(MEMO_ENTRIES);
        Ok(value)
    }
}

/// How the body is shaped beyond what the request says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape<'a> {
    pub(crate) tools: ToolPlacement,
    /// The model as the server names it.
    pub(crate) model: &'a str,
}

pub(crate) fn build(req: &ModelRequest, shape: Shape<'_>, memo: &Memo) -> Result<Vec<Bytes>, ModelError> {
    let size: usize = req.messages.iter().map(Bytes::len).sum();
    let mut out = Out::with_capacity(size, 16 + req.messages.len() / 4);
    out.s("{\"model\":");
    out.s(&quote(shape.model));
    let instructions = memo.get_or(&req.system, || instructions(&req.system))?;
    if instructions.len() > 2 {
        out.s(",\"instructions\":");
        out.shared(instructions);
    }
    let tools = match &req.tools {
        Some(tools) => Some(memo.get_or(tools, || translate_tools(tools))?).filter(|t| t.len() > 2),
        None => None,
    };
    let namespaced = tools.is_some() && shape.tools != ToolPlacement::TopLevel;
    out.s(",\"input\":[");
    let mut first = true;
    if let (Some(tools), ToolPlacement::AdditionalTools) = (&tools, shape.tools) {
        out.s("{\"type\":\"additional_tools\",\"role\":\"developer\",\"tools\":[");
        namespace(&mut out, tools.clone());
        out.s("]}");
        first = false;
    }
    let mut items = Items { out: &mut out, first, namespaced };
    for message in &req.messages {
        items.message(message)?;
    }
    out.s("]");
    if let Some(tools) = tools {
        match shape.tools {
            ToolPlacement::TopLevel => {
                out.s(",\"tools\":");
                out.shared(tools);
            }
            ToolPlacement::Namespace => {
                out.s(",\"tools\":[");
                namespace(&mut out, tools);
                out.s("]");
            }
            ToolPlacement::AdditionalTools => {}
        }
        out.s(",\"tool_choice\":");
        out.s(&tool_choice(req.extra.as_deref()));
        out.s(",\"parallel_tool_calls\":true");
    }
    reasoning(&mut out, req);
    out.s(",\"store\":false,\"stream\":true,\"include\":[\"reasoning.encrypted_content\"]");
    if let Some(key) = &req.cache_key {
        out.s(",\"prompt_cache_key\":");
        out.s(&quote(key));
    }
    out.s("}");
    Ok(out.finish())
}

fn namespace(out: &mut Out, tools: Bytes) {
    out.s("{\"type\":\"namespace\",\"name\":\"");
    out.s(NAMESPACE);
    out.s("\",\"description\":\"\",\"tools\":");
    out.shared(tools);
    out.s("}");
}

/// `s` as a JSON string literal, written straight into `buf`.
fn escape_into(buf: &mut BytesMut, s: &str) {
    struct W<'a>(&'a mut BytesMut);
    impl std::io::Write for W<'_> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(W(buf), s).expect("a string serializes");
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes")
}

/// `reasoning`: the effort as given (the caller clamps it to what the model
/// offers), readable summaries when thinking is to be shown.
fn reasoning(out: &mut Out, req: &ModelRequest) {
    let summaries = matches!(
        req.thinking,
        Some(Thinking::Adaptive { summarized: true } | Thinking::Enabled { summarized: true, .. })
    );
    let effort = match (&req.effort, req.thinking) {
        (Some(e), _) => Some(e.as_str()),
        // A side call that wants no thinking: the least there is.
        (None, Some(Thinking::Disabled)) => Some("low"),
        (None, _) => None,
    };
    if effort.is_none() && !summaries {
        return;
    }
    out.s(",\"reasoning\":{");
    if let Some(effort) = effort {
        out.s("\"effort\":");
        out.s(&quote(effort));
        if summaries {
            out.s(",");
        }
    }
    if summaries {
        out.s("\"summary\":\"auto\"");
    }
    out.s("}");
}

/// The Messages `tool_choice` an extra may carry (compaction turns tools
/// off), in Responses terms; `auto` otherwise.
fn tool_choice(extra: Option<&[u8]>) -> String {
    #[derive(Deserialize)]
    struct Extra<'a> {
        #[serde(borrow, default)]
        tool_choice: Option<Choice<'a>>,
    }
    #[derive(Deserialize)]
    struct Choice<'a> {
        #[serde(rename = "type", borrow)]
        kind: Cow<'a, str>,
        #[serde(borrow, default)]
        name: Option<Cow<'a, str>>,
    }
    let choice = extra.and_then(|e| serde_json::from_slice::<Extra<'_>>(e).ok()).and_then(|e| e.tool_choice);
    match choice {
        Some(c) if c.kind == "none" => "\"none\"".into(),
        Some(c) if c.kind == "any" => "\"required\"".into(),
        Some(Choice { kind, name: Some(name) }) if kind == "tool" => {
            format!("{{\"type\":\"function\",\"name\":{}}}", quote(&name))
        }
        _ => "\"auto\"".into(),
    }
}

/// The `system` blocks' texts, joined, as one JSON string (cache markers
/// mean nothing here).
fn instructions(system: &Bytes) -> Result<Bytes, ModelError> {
    #[derive(Deserialize)]
    struct Block<'a> {
        #[serde(borrow, default)]
        text: Option<Cow<'a, str>>,
    }
    let blocks: Vec<Block<'_>> =
        serde_json::from_slice(system).map_err(|e| ModelError::Invalid(format!("system prompt: {e}")))?;
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b.text.as_deref()).filter(|t| !t.is_empty()).collect();
    Ok(Bytes::from(quote(&texts.join("\n\n"))))
}

/// The Messages `tools` array as Responses function tools (without the
/// surrounding brackets' placement decided: a JSON array).
pub(crate) fn translate_tools(tools: &Bytes) -> Result<Bytes, ModelError> {
    #[derive(Deserialize)]
    struct Def<'a> {
        #[serde(borrow)]
        name: &'a RawValue,
        #[serde(borrow, default)]
        description: Option<&'a RawValue>,
        #[serde(borrow, default)]
        input_schema: Option<&'a RawValue>,
    }
    let defs: Vec<Def<'_>> = serde_json::from_slice(tools).map_err(|e| ModelError::Invalid(format!("tools: {e}")))?;
    let mut out = String::with_capacity(tools.len() + defs.len() * 48);
    out.push('[');
    let mut first = true;
    for def in &defs {
        // Hosted tools (ToolSearch's server side, web search) have no schema
        // and do not exist here.
        let Some(schema) = def.input_schema else { continue };
        if def.name.get() == "\"ToolSearch\"" {
            continue;
        }
        if !std::mem::take(&mut first) {
            out.push(',');
        }
        out.push_str("{\"type\":\"function\",\"name\":");
        out.push_str(def.name.get());
        out.push_str(",\"description\":");
        out.push_str(def.description.map_or("\"\"", RawValue::get));
        out.push_str(",\"strict\":false,\"parameters\":");
        out.push_str(&sanitize_schema(schema));
        out.push('}');
    }
    out.push(']');
    Ok(Bytes::from(out))
}

/// A tool's schema with an object root, as function parameters need.
fn sanitize_schema(schema: &RawValue) -> Cow<'_, str> {
    #[derive(Deserialize)]
    struct Root<'a> {
        #[serde(rename = "type", borrow, default)]
        kind: Option<Cow<'a, str>>,
    }
    let ok = serde_json::from_str::<Root<'_>>(schema.get()).is_ok_and(|r| r.kind.as_deref() == Some("object"));
    if ok {
        return Cow::Borrowed(schema.get());
    }
    let mut value: serde_json::Value = serde_json::from_str(schema.get()).unwrap_or_default();
    if !value.is_object() {
        value = serde_json::json!({});
    }
    value["type"] = "object".into();
    if value.get("properties").is_none() {
        value["properties"] = serde_json::json!({});
    }
    Cow::Owned(value.to_string())
}

/// Writes Responses input items, comma-separated.
struct Items<'o> {
    out: &'o mut Out,
    first: bool,
    /// Function calls name their namespace, as the server reports them.
    namespaced: bool,
}

#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    role: Cow<'a, str>,
    #[serde(borrow)]
    content: &'a RawValue,
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    text: Option<&'a RawValue>,
    #[serde(borrow, default)]
    thinking: Option<&'a RawValue>,
    #[serde(borrow, default)]
    signature: Option<&'a RawValue>,
    #[serde(borrow, default)]
    id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    name: Option<&'a RawValue>,
    #[serde(borrow, default)]
    tool_name: Option<&'a RawValue>,
    #[serde(borrow, default)]
    input: Option<&'a RawValue>,
    #[serde(borrow, default)]
    tool_use_id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    content: Option<&'a RawValue>,
    #[serde(default)]
    is_error: bool,
    #[serde(borrow, default)]
    source: Option<Source<'a>>,
}

#[derive(Deserialize)]
struct Source<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow, default)]
    media_type: Option<&'a RawValue>,
    #[serde(borrow, default)]
    data: Option<&'a RawValue>,
    #[serde(borrow, default)]
    url: Option<&'a RawValue>,
}

/// The inside of a JSON string literal (without its quotes).
fn inner(raw: &RawValue) -> &str {
    let s = raw.get();
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

fn invalid(e: impl std::fmt::Display) -> ModelError {
    ModelError::Invalid(format!("history: {e}"))
}

impl Items<'_> {
    fn open(&mut self) {
        if !self.first {
            self.out.s(",");
        }
        self.first = false;
    }

    fn message(&mut self, src: &Bytes) -> Result<(), ModelError> {
        let message: Message<'_> = serde_json::from_slice(src).map_err(invalid)?;
        let role = message.role.as_ref();
        if message.content.get().starts_with('"') {
            let role = match role {
                "assistant" => "assistant",
                "system" => "developer",
                _ => "user",
            };
            let part = if role == "assistant" { "output_text" } else { "input_text" };
            self.open();
            self.out.s("{\"type\":\"message\",\"role\":\"");
            self.out.s(role);
            self.out.s("\",\"content\":[{\"type\":\"");
            self.out.s(part);
            self.out.s("\",\"text\":");
            self.out.raw(src, message.content.get());
            self.out.s(if role == "assistant" { ",\"annotations\":[]}]}" } else { "}]}" });
            return Ok(());
        }
        let blocks: Vec<Block<'_>> = serde_json::from_str(message.content.get()).map_err(invalid)?;
        match role {
            "assistant" => self.assistant(src, &blocks),
            "system" => {
                self.inputs(src, &blocks, "developer");
                Ok(())
            }
            _ => {
                for block in blocks.iter().filter(|b| b.kind == "tool_result") {
                    self.tool_result(src, block)?;
                }
                self.inputs(src, &blocks, "user");
                Ok(())
            }
        }
    }

    /// One message of the input parts among `blocks` (none: no item).
    fn inputs(&mut self, src: &Bytes, blocks: &[Block<'_>], role: &str) {
        let mut open = false;
        for block in blocks {
            if !matches!(block.kind.as_ref(), "text" | "image" | "document") {
                continue;
            }
            if block.kind == "text" && block.text.is_none_or(|t| t.get() == "\"\"") {
                continue;
            }
            if open {
                self.out.s(",");
            } else {
                self.open();
                self.out.s("{\"type\":\"message\",\"role\":\"");
                self.out.s(role);
                self.out.s("\",\"content\":[");
                open = true;
            }
            self.input_part(src, block);
        }
        if open {
            self.out.s("]}");
        }
    }

    /// A text, image or document block as an input content part.
    fn input_part(&mut self, src: &Bytes, block: &Block<'_>) {
        match block.kind.as_ref() {
            "text" => {
                self.out.s("{\"type\":\"input_text\",\"text\":");
                self.out.raw(src, block.text.map_or("\"\"", RawValue::get));
                self.out.s("}");
            }
            "image" => match &block.source {
                Some(Source { kind, media_type: Some(media), data: Some(data), .. }) if kind == "base64" => {
                    self.out.s("{\"type\":\"input_image\",\"image_url\":\"data:");
                    self.out.raw(src, inner(media));
                    self.out.s(";base64,");
                    self.out.raw(src, inner(data));
                    self.out.s("\"}");
                }
                Some(Source { url: Some(url), .. }) => {
                    self.out.s("{\"type\":\"input_image\",\"image_url\":");
                    self.out.raw(src, url.get());
                    self.out.s("}");
                }
                _ => self.out.s("{\"type\":\"input_text\",\"text\":\"[image]\"}"),
            },
            _ => match &block.source {
                Some(Source { kind, media_type: Some(media), data: Some(data), .. }) if kind == "base64" => {
                    self.out.s("{\"type\":\"input_file\",\"filename\":\"document.pdf\",\"file_data\":\"data:");
                    self.out.raw(src, inner(media));
                    self.out.s(";base64,");
                    self.out.raw(src, inner(data));
                    self.out.s("\"}");
                }
                Some(Source { kind, data: Some(text), .. }) if kind == "text" => {
                    self.out.s("{\"type\":\"input_text\",\"text\":");
                    self.out.raw(src, text.get());
                    self.out.s("}");
                }
                _ => self.out.s("{\"type\":\"input_text\",\"text\":\"[document]\"}"),
            },
        }
    }

    fn tool_result(&mut self, src: &Bytes, block: &Block<'_>) -> Result<(), ModelError> {
        let id = block.tool_use_id.ok_or_else(|| invalid("a tool_result without tool_use_id"))?;
        self.open();
        self.out.s("{\"type\":\"function_call_output\",\"call_id\":");
        self.out.raw(src, id.get());
        self.out.s(",\"output\":");
        match block.content {
            None => self.out.s(if block.is_error { "\"Error\"" } else { "\"\"" }),
            Some(text) if text.get().starts_with('"') => {
                // Responses has no error flag: the text says it.
                if block.is_error {
                    self.out.s("\"Error: ");
                    self.out.raw(src, &text.get()[1..]);
                } else {
                    self.out.raw(src, text.get());
                }
            }
            Some(parts) => {
                let parts: Vec<Block<'_>> = serde_json::from_str(parts.get()).map_err(invalid)?;
                self.out.s("[");
                let mut first = true;
                let mut sep = |out: &mut Out| {
                    if !std::mem::take(&mut first) {
                        out.s(",");
                    }
                };
                if block.is_error {
                    sep(self.out);
                    self.out.s("{\"type\":\"input_text\",\"text\":\"Error:\"}");
                }
                for part in &parts {
                    match part.kind.as_ref() {
                        "text" | "image" | "document" => {
                            sep(self.out);
                            self.input_part(src, part);
                        }
                        "tool_reference" => {
                            sep(self.out);
                            self.out.s("{\"type\":\"input_text\",\"text\":\"Tool loaded: ");
                            self.out.s(part.tool_name.map_or("", inner));
                            self.out.s("\"}");
                        }
                        _ => {}
                    }
                }
                if first {
                    self.out.s("{\"type\":\"input_text\",\"text\":\"\"}");
                }
                self.out.s("]");
            }
        }
        self.out.s("}");
        Ok(())
    }

    /// An assistant message: its texts as message items, its reasoning and
    /// calls as their own items, in order.
    fn assistant(&mut self, src: &Bytes, blocks: &[Block<'_>]) -> Result<(), ModelError> {
        let mut open = false;
        let close = |items: &mut Self, open: &mut bool| {
            if std::mem::take(open) {
                items.out.s("]}");
            }
        };
        for block in blocks {
            match block.kind.as_ref() {
                "text" => {
                    let Some(text) = block.text.filter(|t| t.get() != "\"\"") else { continue };
                    if open {
                        self.out.s(",");
                    } else {
                        self.open();
                        self.out.s("{\"type\":\"message\",\"role\":\"assistant\",\"content\":[");
                        open = true;
                    }
                    self.out.s("{\"type\":\"output_text\",\"text\":");
                    self.out.raw(src, text.get());
                    self.out.s(",\"annotations\":[]}");
                }
                "thinking" => {
                    // Only reasoning this adapter produced can go back; a
                    // Claude signature means nothing to OpenAI.
                    let Some((id, encrypted)) = block.signature.and_then(|s| crate::stream::unseal(inner(s))) else {
                        continue;
                    };
                    close(self, &mut open);
                    self.open();
                    // Ids and encrypted content are base64-ish: nothing to escape.
                    self.out.s("{\"type\":\"reasoning\",\"id\":\"");
                    self.out.raw(src, id);
                    self.out.s("\"");
                    self.out.s(",\"summary\":[");
                    if let Some(thinking) = block.thinking.filter(|t| t.get() != "\"\"") {
                        self.out.s("{\"type\":\"summary_text\",\"text\":");
                        self.out.raw(src, thinking.get());
                        self.out.s("}");
                    }
                    self.out.s("],\"encrypted_content\":\"");
                    self.out.raw(src, encrypted);
                    self.out.s("\"}");
                }
                "tool_use" => {
                    let (Some(id), Some(name)) = (block.id, block.name) else {
                        return Err(invalid("a tool_use without id or name"));
                    };
                    close(self, &mut open);
                    self.open();
                    self.out.s("{\"type\":\"function_call\",\"call_id\":");
                    self.out.raw(src, id.get());
                    self.out.s(",\"name\":");
                    self.out.raw(src, name.get());
                    if self.namespaced {
                        self.out.s(",\"namespace\":\"");
                        self.out.s(NAMESPACE);
                        self.out.s("\"");
                    }
                    self.out.s(",\"arguments\":");
                    // The one copy: Responses takes the input as a string.
                    escape_into(&mut self.out.buf, block.input.map_or("{}", RawValue::get));
                    self.out.s("}");
                }
                // Claude's server tools, redacted thinking and anything newer
                // have no Responses counterpart.
                _ => {}
            }
        }
        close(self, &mut open);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn request(messages: &[Value]) -> ModelRequest {
        ModelRequest {
            model: "gpt-5.5".into(),
            max_tokens: 1000,
            system: Bytes::from_static(br#"[{"type":"text","text":"You are Eos."},{"type":"text","text":"Be brief.","cache_control":{"type":"ephemeral"}}]"#),
            tools: Some(Bytes::from_static(
                br#"[{"name":"Read","description":"Reads a file","input_schema":{"type":"object","properties":{"file_path":{"type":"string"}}}},{"name":"ToolSearch","description":"x","input_schema":{"type":"object"}},{"name":"NoRoot","description":"y","input_schema":{"properties":{}}}]"#,
            )),
            messages: messages.iter().map(|m| Bytes::from(m.to_string())).collect(),
            thinking: Some(Thinking::Adaptive { summarized: true }),
            effort: Some("high".into()),
            extra: None,
            cache_key: Some("root-1".into()),
            betas: &[],
            auth: None,
        }
    }

    fn body(req: &ModelRequest, tools: ToolPlacement) -> Value {
        let pieces = build(req, Shape { tools, model: &req.model }, &Memo::default()).expect("build");
        serde_json::from_slice(&pieces.concat()).expect("valid json")
    }

    #[test]
    fn a_conversation_becomes_responses_items() {
        let big = "x".repeat(2000);
        let req = request(&[
            json!({"role":"user","content":[{"type":"text","text":"hi \"there\""},{"type":"text","text":&big,"cache_control":{"type":"ephemeral"}}]}),
            json!({"role":"assistant","content":[
                {"type":"thinking","thinking":"Let me look.","signature":"oai1.rs_1.ENC=="},
                {"type":"thinking","thinking":"claude's","signature":"EqQBClaude"},
                {"type":"text","text":"Reading."},
                {"type":"tool_use","id":"call_1","name":"Read","input":{"file_path":"/a \"b\""}}]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"call_1","content":"line one"},
                {"type":"text","text":"<system-reminder>note</system-reminder>"}]}),
            json!({"role":"system","content":[{"type":"text","text":"The tool set changed."}]}),
        ]);
        let v = body(&req, ToolPlacement::Namespace);
        assert_eq!(v["model"], "gpt-5.5");
        assert_eq!(v["instructions"], "You are Eos.\n\nBe brief.");
        assert_eq!(v["store"], false);
        assert_eq!(v["stream"], true);
        assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(v["prompt_cache_key"], "root-1");
        assert_eq!(v["reasoning"], json!({"effort": "high", "summary": "auto"}));
        assert_eq!(v["tool_choice"], "auto");
        let items = v["input"].as_array().expect("input");
        let kinds: Vec<&str> = items.iter().map(|i| i["type"].as_str().unwrap_or_default()).collect();
        assert_eq!(
            kinds,
            ["message", "reasoning", "message", "function_call", "function_call_output", "message", "message"]
        );
        assert_eq!(items[0]["content"][0], json!({"type": "input_text", "text": "hi \"there\""}));
        assert_eq!(items[0]["content"][1]["text"], big.as_str());
        assert_eq!(
            items[1],
            json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Let me look."}],"encrypted_content":"ENC=="})
        );
        assert_eq!(items[2]["content"][0], json!({"type":"output_text","text":"Reading.","annotations":[]}));
        assert_eq!(items[3]["arguments"], r#"{"file_path":"/a \"b\""}"#);
        assert_eq!(items[3]["namespace"], "functions");
        assert_eq!(items[4], json!({"type":"function_call_output","call_id":"call_1","output":"line one"}));
        assert_eq!(items[5]["role"], "user");
        assert_eq!(items[6]["role"], "developer");
        assert!(items.iter().all(|i| i["role"] != "system"));
        let tools = &v["tools"][0];
        assert_eq!((tools["type"].as_str(), tools["name"].as_str()), (Some("namespace"), Some("functions")));
        let names: Vec<&str> =
            tools["tools"].as_array().expect("tools").iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, ["Read", "NoRoot"], "ToolSearch is never sent");
        assert_eq!(tools["tools"][1]["parameters"]["type"], "object");
        for unsupported in ["max_output_tokens", "temperature", "metadata", "previous_response_id", "truncation"] {
            assert!(v.get(unsupported).is_none(), "{unsupported}");
        }
    }

    #[test]
    fn large_values_are_shared_not_copied() {
        let big = "y".repeat(4000);
        let req = request(&[json!({"role":"user","content":[{"type":"text","text":big}]})]);
        let pieces =
            build(&req, Shape { tools: ToolPlacement::Namespace, model: &req.model }, &Memo::default()).expect("build");
        let message = &req.messages[0];
        let inside = |p: &Bytes| {
            let range = message.as_ptr() as usize..message.as_ptr() as usize + message.len();
            range.contains(&(p.as_ptr() as usize))
        };
        assert!(pieces.iter().any(|p| p.len() >= 4000 && inside(p)), "the text is a slice of the stored message");
    }

    #[test]
    fn errors_images_and_placements() {
        let req = request(&[json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"c1","content":"boom","is_error":true},
                {"type":"tool_result","tool_use_id":"c2","content":[{"type":"text","text":"see"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBOR"}}]},
                {"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/"}}]})]);
        let v = body(&req, ToolPlacement::TopLevel);
        let items = v["input"].as_array().expect("input");
        assert_eq!(items[0]["output"], "Error: boom");
        assert_eq!(items[1]["output"][1], json!({"type":"input_image","image_url":"data:image/png;base64,iVBOR"}));
        assert_eq!(items[2]["content"][0]["image_url"], "data:image/jpeg;base64,/9j/");
        assert_eq!(v["tools"][0]["type"], "function", "plain function tools");
        let v = body(&req, ToolPlacement::AdditionalTools);
        assert_eq!(v["input"][0]["type"], "additional_tools");
        assert_eq!(v["input"][0]["tools"][0]["type"], "namespace");
        assert!(v.get("tools").is_none());
    }

    #[test]
    fn compaction_turns_tools_off_and_side_calls_think_least() {
        let mut req = request(&[json!({"role":"user","content":[{"type":"text","text":"sum up"}]})]);
        req.extra = Some(Bytes::from_static(br#"{"metadata":{"user_id":"u"},"tool_choice":{"type":"none"}}"#));
        req.effort = None;
        req.thinking = Some(Thinking::Disabled);
        let v = body(&req, ToolPlacement::Namespace);
        assert_eq!(v["tool_choice"], "none");
        assert_eq!(v["reasoning"], json!({"effort": "low"}));
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn shared_values_are_translated_once() {
        let memo = Memo::default();
        let req = request(&[json!({"role":"user","content":[{"type":"text","text":"a"}]})]);
        let first = build(&req, Shape { tools: ToolPlacement::TopLevel, model: &req.model }, &memo).expect("build");
        let second = build(&req, Shape { tools: ToolPlacement::TopLevel, model: &req.model }, &memo).expect("build");
        let tools_piece = |pieces: &[Bytes]| pieces.iter().find(|p| p.starts_with(b"[{\"type\":\"function\"")).cloned();
        let (a, b) = (tools_piece(&first).expect("tools"), tools_piece(&second).expect("tools"));
        assert_eq!(a.as_ptr(), b.as_ptr(), "the second request reuses the translation");
    }
}
