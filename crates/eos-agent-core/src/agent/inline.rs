//! Claude Code's `mid-conversation-tool-changes` and `inline-tools` betas:
//! the `tools` array keeps the tools of the first request (so its cache
//! holds), and a tool sent in full that arrives later (an MCP server done
//! connecting, signed in to, or listing a new `alwaysLoad` tool) comes in a
//! `tool_addition` block of a mid-conversation system message; with inline
//! tools the block carries its definition, a changed definition is sent
//! again, and a tool that goes away is withdrawn with a `tool_removal` block.
//!
//! When the API refuses either beta the request is sent again without it, as
//! Claude Code does: inline tools give way to tools added by reference, those
//! to the `tools` array (for this conversation, or for every agent of the
//! process when the beta header itself was refused).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use eos_agent_types::{ContentBlock, Role};

use super::deferred::{Note, REPLACED, SURFACED};
use crate::message::StoredMessage;
use crate::model::ModelError;
use crate::tool::Toolbox;

pub const MID_CONVERSATION_TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";
pub const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";

const TOOL_SEARCH: &str = "ToolSearch";
/// The block that stands, in the recorded conversation, for a definition
/// identical to the one the tool had when it was withdrawn (Claude Code's
/// `sameAs` pointer); the request carries the definition itself.
const SAME_AS_EARLIER: &str = "same_as_earlier";

/// How tools that arrive after the first request reach the model, least to
/// most capable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolChanges {
    /// Added to the `tools` array (no betas).
    #[default]
    InTools,
    /// `mid-conversation-tool-changes`: a `tool_addition` block names the
    /// tool, which the `tools` array declares with `defer_loading`.
    ByReference,
    /// With `inline-tools` too: the block carries the definition.
    ByValue,
}

impl ToolChanges {
    /// The betas a request needs for these blocks.
    pub fn betas(self) -> &'static [&'static str] {
        match self {
            Self::InTools => &[],
            Self::ByReference => &[MID_CONVERSATION_TOOL_CHANGES_BETA],
            Self::ByValue => &[MID_CONVERSATION_TOOL_CHANGES_BETA, INLINE_TOOLS_BETA],
        }
    }
}

/// Claude Code's process-wide latches for refused tool-change betas, shared
/// by every agent of a host: the beta header refused (no agent sends either
/// beta again), inline tools refused by the platform, and the models the
/// API said take no tool changes.
#[derive(Debug, Default)]
pub struct ToolChangeRefusals {
    header: AtomicBool,
    inline: AtomicBool,
    models: Mutex<BTreeSet<String>>,
}

impl ToolChangeRefusals {
    /// `mode` as far as earlier refusals let `model` use it.
    pub fn allowed(&self, mode: ToolChanges, model: &str) -> ToolChanges {
        let refused_model = || self.models.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(model);
        if mode == ToolChanges::InTools || self.header.load(Ordering::Relaxed) || refused_model() {
            ToolChanges::InTools
        } else if mode == ToolChanges::ByValue && self.inline.load(Ordering::Relaxed) {
            ToolChanges::ByReference
        } else {
            mode
        }
    }

    fn refuse_model(&self, model: &str) {
        self.models.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(model.to_owned());
    }
}

/// Why the API refused `mid-conversation-tool-changes` (Claude Code's `LRe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeRefusal {
    BlockTypeUnknown,
    HeaderRejected,
    AfterPausedTurn,
    ModelUnsupported,
    TooManyDefinitions,
    PfcOnly,
    UnknownToolReference,
    Other,
}

/// Why the API refused `inline-tools` (Claude Code's `PRe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlineRefusal {
    UnsupportedOnPlatform,
    HeaderRejected,
    KindNotEnabled,
    NameConflict,
}

/// The status and message of a refused request (`"400: …"`).
fn refused_request(error: &ModelError) -> Option<(u16, &str)> {
    let ModelError::Invalid(text) = error else { return None };
    let (status, message) = text.split_once(": ")?;
    Some((status.parse().ok()?, message))
}

/// Claude Code's `uxi`: the message names `beta` as a refused beta header.
fn header_refused(message: &str, beta: &str) -> bool {
    (message.contains(beta) && (message.contains("anthropic-beta") || message.contains("anthropic_beta")))
        || message.contains(&format!("beta_header:{beta}"))
}

/// Claude Code's `Lne`: an `Input tag '…'` the API did not know matches.
fn unknown_input_tag(message: &str, matches: impl Fn(&str) -> bool) -> bool {
    const TAG: &str = "Input tag '";
    let mut rest = message;
    while let Some(at) = rest.find(TAG) {
        rest = &rest[at + TAG.len()..];
        let len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(0);
        if len > 0 && rest[len..].starts_with('\'') && matches(&rest[..len]) {
            return true;
        }
    }
    false
}

/// `\bword\b`.
fn has_word(message: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    message.match_indices(word).any(|(at, _)| {
        !message[..at].chars().next_back().is_some_and(is_word)
            && !message[at + word.len()..].chars().next().is_some_and(is_word)
    })
}

/// Claude Code's `Xio`: a refusal of a `cache_control` field.
fn cache_control_refused(message: &str) -> bool {
    if !message.contains("cache_control") || message.contains("empty text block") || message.contains("tool_result") {
        return false;
    }
    let system_n = message.match_indices("system.").any(|(at, _)| {
        message[at + 7..].chars().next().is_some_and(|c| c.is_ascii_digit())
            && !message[..at].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    });
    let lower = message.to_ascii_lowercase();
    !system_n
        && !has_word(&lower, "ttl")
        && ["not permitted", "cannot be set", "unknown name", "unknown field", "unrecognized", "additional propert"]
            .iter()
            .any(|p| lower.contains(p))
}

fn change_refusal(status: u16, message: &str) -> Option<ChangeRefusal> {
    if (status == 400 || status == 422) && unknown_input_tag(message, |t| t == "tool_addition" || t == "tool_removal") {
        return Some(ChangeRefusal::BlockTypeUnknown);
    }
    if status != 400 {
        return None;
    }
    if header_refused(message, MID_CONVERSATION_TOOL_CHANGES_BETA) {
        return Some(ChangeRefusal::HeaderRejected);
    }
    if !has_word(message, "tool_addition") || cache_control_refused(message) {
        return None;
    }
    Some(if message.contains("paused assistant turn") {
        ChangeRefusal::AfterPausedTurn
    } else if message.contains("not supported on this model") || message.contains("requires a model that supports") {
        ChangeRefusal::ModelUnsupported
    } else if message.contains("exceed") {
        ChangeRefusal::TooManyDefinitions
    } else if message.contains("PFC-only") {
        ChangeRefusal::PfcOnly
    } else if message.contains("references") || message.contains("names a declared") {
        ChangeRefusal::UnknownToolReference
    } else {
        ChangeRefusal::Other
    })
}

fn inline_refusal(status: u16, message: &str) -> Option<InlineRefusal> {
    if (status == 400 || status == 422) && unknown_input_tag(message, |t| t == "tool_definition") {
        return Some(InlineRefusal::UnsupportedOnPlatform);
    }
    if status != 400 {
        return None;
    }
    let names_additions = message.contains("tool definitions") || message.contains("tool_addition");
    if header_refused(message, INLINE_TOOLS_BETA) {
        Some(InlineRefusal::HeaderRejected)
    } else if message.contains("are not available on this platform") && names_additions {
        Some(InlineRefusal::UnsupportedOnPlatform)
    } else if message.contains("cannot yet be defined in a message") {
        Some(InlineRefusal::KindNotEnabled)
    } else if message.contains("is already used by a")
        && (message.contains("tool.definition") || message.contains("tool_addition"))
    {
        Some(InlineRefusal::NameConflict)
    } else {
        None
    }
}

/// A tool the conversation's blocks brought in.
#[derive(Debug)]
struct Surfaced {
    by_value: bool,
    /// By value: the definition sent; by reference: the deferred declaration
    /// the `tools` array keeps.
    definition: Bytes,
}

/// What changed since the last request.
#[derive(Debug, Default)]
pub(super) struct Changes {
    /// Sorted names, for the note.
    pub(super) added: Vec<String>,
    pub(super) replaced: Vec<String>,
    pub(super) removed: Vec<String>,
    /// Removals, then additions (Claude Code's order).
    pub(super) blocks: Vec<Bytes>,
}

impl Changes {
    pub(super) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[derive(Debug)]
pub(super) struct Inline {
    mode: ToolChanges,
    /// Inline tools were refused in this conversation (kept through a
    /// summary, as Claude Code keeps it).
    inline_refused: bool,
    /// Tool changes were refused in this conversation (until a summary).
    changes_refused: bool,
    /// The tools of the first request (Claude Code's declared-tools latch).
    latched: Option<BTreeSet<String>>,
    surfaced: BTreeMap<String, Surfaced>,
    /// Tools sent by value and then withdrawn, with the definition they had
    /// (Claude Code's `definedByValue` of a withdrawn tool).
    withdrawn: BTreeMap<String, Bytes>,
}

impl Default for Inline {
    fn default() -> Self {
        Self {
            mode: ToolChanges::InTools,
            inline_refused: false,
            changes_refused: false,
            latched: None,
            surfaced: BTreeMap::new(),
            withdrawn: BTreeMap::new(),
        }
    }
}

impl Inline {
    /// The conversation was summarized: the tools it defined are gone with
    /// it; a refusal of inline tools holds, one of tool changes does not.
    pub(super) fn after_summary(&self) -> Self {
        Self { inline_refused: self.inline_refused, ..Self::default() }
    }

    /// The mode this conversation's requests use.
    pub(super) fn mode(&self) -> ToolChanges {
        self.mode
    }

    /// `mode` within what this conversation's refusals allow.
    fn ceiling(&self, mode: ToolChanges) -> ToolChanges {
        if self.changes_refused {
            ToolChanges::InTools
        } else if self.inline_refused {
            mode.min(ToolChanges::ByReference)
        } else {
            mode
        }
    }

    pub(super) fn latch(&mut self, mode: ToolChanges, toolbox: &Toolbox, history: &[StoredMessage]) {
        let mode = self.ceiling(mode);
        if self.latched.is_none() && mode != ToolChanges::InTools {
            for message in history.iter().filter(|m| m.role() == Role::System) {
                if !contains(message.json(), b"\"tool_") {
                    continue;
                }
                for block in message.blocks().unwrap_or_default() {
                    if let ContentBlock::Unknown(raw) = block {
                        self.replay(raw.get(), toolbox);
                    }
                }
            }
            let by_value = |name: &str| self.surfaced.get(name).is_some_and(|s| s.by_value);
            self.latched = Some(toolbox.names().filter(|n| !by_value(n)).map(str::to_owned).collect());
        }
        if mode < ToolChanges::ByValue {
            // Tools the conversation defined by value are declared in the
            // `tools` array from here on (Claude Code's fallback from inline
            // tools), and none is withdrawn by block any more.
            let defined: Vec<String> =
                self.surfaced.iter().filter(|(_, s)| s.by_value).map(|(n, _)| n.clone()).collect();
            for name in defined {
                self.surfaced.remove(&name);
                if let Some(latched) = &mut self.latched {
                    latched.insert(name);
                }
            }
            self.withdrawn.clear();
        }
        self.mode = mode;
    }

    /// Takes in a block recorded in the conversation.
    fn replay(&mut self, block: &str, toolbox: &Toolbox) {
        let Ok(block) = serde_json::from_str::<serde_json::Value>(block) else { return };
        let tool = &block["tool"];
        match (block["type"].as_str(), tool["type"].as_str()) {
            (Some("tool_addition"), Some("tool_definition")) => {
                let Some(name) = tool["definition"]["name"].as_str() else { return };
                let definition = Bytes::from(tool["definition"].to_string());
                self.withdrawn.remove(name);
                self.surfaced.insert(name.to_owned(), Surfaced { by_value: true, definition });
            }
            (Some("tool_addition"), Some(SAME_AS_EARLIER)) => {
                let Some(name) = tool["name"].as_str() else { return };
                let definition = self
                    .withdrawn
                    .remove(name)
                    .or_else(|| toolbox.get(name).map(|t| t.spec().clone().undeferred().json().clone()));
                if let Some(definition) = definition {
                    self.surfaced.insert(name.to_owned(), Surfaced { by_value: true, definition });
                }
            }
            (Some("tool_addition"), Some("tool_reference")) => {
                let Some(name) = tool["name"].as_str() else { return };
                if let Some(t) = toolbox.get(name) {
                    let definition = t.spec().clone().deferred().json().clone();
                    self.surfaced.insert(name.to_owned(), Surfaced { by_value: false, definition });
                }
            }
            (Some("tool_removal"), _) => {
                if let Some(name) = tool["name"].as_str()
                    && let Some(s) = self.surfaced.remove(name)
                    && s.by_value
                {
                    self.withdrawn.insert(name.to_owned(), s.definition);
                }
            }
            _ => {}
        }
    }

    /// Takes in a refused request: true when it should be sent again without
    /// the beta the API refused (Claude Code's `retry:inline-tools-strip` and
    /// `retry:tool-change-strip`), the refusal latched in `refusals` too when
    /// it holds beyond this conversation.
    pub(super) fn refused(&mut self, error: &ModelError, model: &str, refusals: &ToolChangeRefusals) -> bool {
        let Some((status, message)) = refused_request(error) else { return false };
        let change = if self.mode >= ToolChanges::ByReference { change_refusal(status, message) } else { None };
        let inline = if self.mode == ToolChanges::ByValue { inline_refusal(status, message) } else { None };
        if change != Some(ChangeRefusal::HeaderRejected)
            && let Some(refusal) = inline
        {
            self.inline_refused = true;
            if matches!(refusal, InlineRefusal::HeaderRejected | InlineRefusal::UnsupportedOnPlatform) {
                refusals.inline.store(true, Ordering::Relaxed);
            }
            return true;
        }
        let Some(refusal) = change else { return false };
        self.changes_refused = true;
        match refusal {
            ChangeRefusal::HeaderRejected | ChangeRefusal::BlockTypeUnknown => {
                refusals.header.store(true, Ordering::Relaxed);
                if refusal == ChangeRefusal::HeaderRejected && inline == Some(InlineRefusal::HeaderRejected) {
                    refusals.inline.store(true, Ordering::Relaxed);
                }
            }
            ChangeRefusal::ModelUnsupported => refusals.refuse_model(model),
            _ => {}
        }
        true
    }

    /// Tools sent in full that arrived since, definitions that changed, and
    /// tools sent by value that went away (with ToolSearch absent, every new
    /// tool counts as sent in full).
    pub(super) fn changes(&mut self, toolbox: &Toolbox, tool_search_absent: bool) -> Changes {
        let mut out = Changes::default();
        if self.mode == ToolChanges::InTools {
            return out;
        }
        let Some(latched) = &self.latched else { return out };
        let by_value = self.mode == ToolChanges::ByValue;
        let mut additions = Vec::new();
        for tool in toolbox.tools() {
            let spec = tool.spec();
            let name = spec.name();
            if name == TOOL_SEARCH || latched.contains(name) {
                continue;
            }
            match self.surfaced.get_mut(name) {
                Some(s) if s.by_value => {
                    let definition = spec.clone().undeferred().json().clone();
                    if !same_json(&s.definition, &definition) {
                        s.definition = definition.clone();
                        out.replaced.push(name.to_owned());
                        additions.push(addition(name, Some(&definition)));
                    }
                }
                Some(_) => {}
                None if tool_search_absent || !spec.is_deferred() => {
                    let surfaced = if by_value {
                        Surfaced { by_value: true, definition: spec.clone().undeferred().json().clone() }
                    } else {
                        Surfaced { by_value: false, definition: spec.clone().deferred().json().clone() }
                    };
                    // Back with the definition it was withdrawn with: the
                    // recorded block points to it (Claude Code's `sameAs`).
                    let earlier = self.withdrawn.remove(name);
                    let block = match earlier {
                        Some(earlier) if by_value && same_json(&earlier, &surfaced.definition) => pointer(name),
                        _ => addition(name, by_value.then_some(&surfaced.definition)),
                    };
                    additions.push(block);
                    out.added.push(name.to_owned());
                    self.surfaced.insert(name.to_owned(), surfaced);
                }
                None => {}
            }
        }
        let gone: Vec<String> = self
            .surfaced
            .iter()
            .filter(|(name, s)| s.by_value && toolbox.get(name).is_none())
            .map(|(name, _)| name.clone())
            .collect();
        for name in gone {
            if let Some(s) = self.surfaced.remove(&name) {
                self.withdrawn.insert(name.clone(), s.definition);
            }
            out.blocks.push(reference("tool_removal", &name));
            out.removed.push(name);
        }
        // Additions in the note's order: new tools, then updated ones.
        let order = |name: &str| (out.replaced.iter().any(|r| r == name), name.to_owned());
        let mut additions: Vec<(String, Bytes)> = additions;
        additions.sort_by_key(|(name, _)| order(name));
        out.blocks.extend(additions.into_iter().map(|(_, b)| b));
        out.added.sort();
        out.replaced.sort();
        out
    }

    /// The tools the `tools` array declares: the toolbox without the tools
    /// sent by value, with those sent by reference declared deferred (kept
    /// even after they go away, as the conversation still names them), each
    /// with the name it is declared under.
    pub(super) fn declared<'a>(&'a self, toolbox: &'a Toolbox) -> Vec<Declared<'a>> {
        let inline = self.mode != ToolChanges::InTools;
        let mut out = Vec::with_capacity(toolbox.tools().len());
        for tool in toolbox.tools() {
            let spec = tool.spec();
            match self.surfaced.get(spec.name()).filter(|_| inline) {
                Some(s) if s.by_value => {}
                Some(s) => out.push(Declared { name: spec.name(), json: &s.definition, deferred: false }),
                None => out.push(Declared { name: spec.name(), json: spec.json(), deferred: spec.is_deferred() }),
            }
        }
        if inline {
            for (name, s) in &self.surfaced {
                if !s.by_value && toolbox.get(name).is_none() {
                    out.push(Declared { name, json: &s.definition, deferred: false });
                }
            }
        }
        out
    }

    /// The conversation as this request sends it, when it has to differ from
    /// the recorded one: definitions behind `same_as_earlier` pointers filled
    /// in, and the blocks of a beta no longer sent (with the paragraphs that
    /// name their tools) left out. `None`: the recorded conversation as is.
    pub(super) fn wire_history(&self, history: &[StoredMessage]) -> Option<Vec<StoredMessage>> {
        let has_blocks = |m: &StoredMessage| m.role() == Role::System && contains(m.json(), b"\"tool_");
        if !history.iter().any(has_blocks) {
            return None;
        }
        let mut defined: HashMap<String, serde_json::Value> = HashMap::new();
        let mut out: Option<Vec<StoredMessage>> = None;
        for (i, message) in history.iter().enumerate() {
            if !has_blocks(message) {
                if let Some(out) = &mut out {
                    out.push(message.clone());
                }
                continue;
            }
            let rewritten = self.rewrite(message, &mut defined);
            if rewritten.is_some() && out.is_none() {
                out = Some(history[..i].to_vec());
            }
            if let Some(out) = &mut out {
                match rewritten {
                    Some(Some(m)) => out.push(m),
                    Some(None) => {}
                    None => out.push(message.clone()),
                }
            }
        }
        out
    }

    /// One system message for the wire: `None` unchanged, `Some(None)` left
    /// out (nothing remains of it).
    fn rewrite(
        &self,
        message: &StoredMessage,
        defined: &mut HashMap<String, serde_json::Value>,
    ) -> Option<Option<StoredMessage>> {
        let blocks = message.blocks().ok()?;
        let mut texts = Vec::new();
        let mut tool_blocks: Vec<serde_json::Value> = Vec::new();
        let mut pointers = false;
        let mut by_value = false;
        for block in blocks {
            match block {
                ContentBlock::Text { text, .. } => texts.push(text),
                ContentBlock::Unknown(raw) => {
                    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(raw.get()) else { continue };
                    match value["tool"]["type"].as_str() {
                        Some("tool_definition") => {
                            by_value = true;
                            if let Some(name) = value["tool"]["definition"]["name"].as_str() {
                                defined.insert(name.to_owned(), value["tool"]["definition"].clone());
                            }
                        }
                        Some(SAME_AS_EARLIER) => {
                            by_value = true;
                            pointers = true;
                            let definition = value["tool"]["name"].as_str().and_then(|n| defined.get(n)).cloned();
                            let Some(definition) = definition else { continue };
                            value["tool"] = serde_json::json!({"type": "tool_definition", "definition": definition});
                        }
                        _ => by_value |= value["type"] == "tool_removal",
                    }
                    tool_blocks.push(value);
                }
                _ => return None,
            }
        }
        let strip = self.mode == ToolChanges::InTools || (self.mode == ToolChanges::ByReference && by_value);
        if !strip && !pointers {
            return None;
        }
        let text = texts.join("\n");
        let (text, blocks) = if strip {
            let kept: Vec<&str> =
                text.split("\n\n").filter(|p| !p.starts_with(SURFACED) && !p.starts_with(REPLACED)).collect();
            (kept.join("\n\n"), Vec::new())
        } else {
            (text, tool_blocks.iter().map(|b| Bytes::from(b.to_string())).collect())
        };
        if text.is_empty() && blocks.is_empty() {
            return Some(None);
        }
        Some(Some(Note { text: (!text.is_empty()).then_some(text), blocks }.message()))
    }
}

/// One entry of the `tools` array.
#[derive(Debug, Clone, Copy)]
pub(super) struct Declared<'a> {
    pub(super) name: &'a str,
    pub(super) json: &'a Bytes,
    /// A deferred tool: declared only once ToolSearch loaded it.
    pub(super) deferred: bool,
}

/// A `tool_addition` block: the definition (by value) or the name.
fn addition(name: &str, definition: Option<&Bytes>) -> (String, Bytes) {
    let block = match definition {
        Some(definition) => {
            let mut json = Vec::from(&br#"{"type":"tool_addition","tool":{"type":"tool_definition","definition":"#[..]);
            json.extend_from_slice(definition);
            json.extend_from_slice(b"}}");
            Bytes::from(json)
        }
        None => reference("tool_addition", name),
    };
    (name.to_owned(), block)
}

/// The recorded stand-in for a definition identical to the earlier one.
fn pointer(name: &str) -> (String, Bytes) {
    let quoted = serde_json::to_string(name).unwrap_or_default();
    let block = format!(r#"{{"type":"tool_addition","tool":{{"type":"{SAME_AS_EARLIER}","name":{quoted}}}}}"#);
    (name.to_owned(), Bytes::from(block))
}

/// A block of `kind` naming tool `name`.
fn reference(kind: &str, name: &str) -> Bytes {
    let name = serde_json::to_string(name).unwrap_or_default();
    Bytes::from(format!(r#"{{"type":"{kind}","tool":{{"type":"tool_reference","name":{name}}}}}"#))
}

/// Equal as JSON (a definition read back from the conversation may order
/// its members differently).
fn same_json(a: &[u8], b: &[u8]) -> bool {
    a == b
        || serde_json::from_slice::<serde_json::Value>(a)
            .is_ok_and(|a| serde_json::from_slice::<serde_json::Value>(b).is_ok_and(|b| a == b))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::value::RawValue;

    use super::*;
    use crate::BoxFuture;
    use crate::tool::{Tool, ToolContext, ToolOutput, ToolSpec};

    struct Named(ToolSpec);

    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        fn call<'a>(&'a self, _input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::text("") })
        }
    }

    /// `mcp__x__d*` tools are deferred.
    fn toolbox(names: &[&str]) -> Toolbox {
        let tools = names.iter().map(|n| {
            let spec = ToolSpec::new(n, "d", serde_json::json!({"type": "object"}));
            let spec = if n.starts_with("mcp__x__d") { spec.deferred() } else { spec };
            Arc::new(Named(spec)) as Arc<dyn Tool>
        });
        Toolbox::new(tools.collect())
    }

    fn names(inline: &Inline, toolbox: &Toolbox) -> Vec<String> {
        inline.declared(toolbox).iter().map(|d| d.name.to_owned()).collect()
    }

    fn text(block: &Bytes) -> &str {
        std::str::from_utf8(block).expect("utf8")
    }

    fn note(changes: Changes) -> StoredMessage {
        Note { text: Some(format!("{SURFACED}\nx")), blocks: changes.blocks }.message()
    }

    #[test]
    fn late_tools_sent_in_full_come_as_blocks() {
        let mut inline = Inline::default();
        inline.latch(ToolChanges::ByValue, &toolbox(&["Read", "ToolSearch"]), &[]);
        let later = toolbox(&["Read", "ToolSearch", "mcp__x__dlist", "mcp__x__pin"]);
        let changes = inline.changes(&later, false);
        assert_eq!(changes.added, ["mcp__x__pin"], "deferred tools go through ToolSearch");
        assert_eq!(names(&inline, &later), ["Read", "ToolSearch", "mcp__x__dlist"]);
        assert!(inline.changes(&later, false).is_empty());
        let gone = inline.changes(&toolbox(&["Read", "ToolSearch"]), false);
        assert_eq!(gone.removed, ["mcp__x__pin"]);
        assert_eq!(
            text(&gone.blocks[0]),
            r#"{"type":"tool_removal","tool":{"type":"tool_reference","name":"mcp__x__pin"}}"#
        );
    }

    #[test]
    fn by_reference_the_tool_is_declared_deferred() {
        let mut inline = Inline::default();
        inline.latch(ToolChanges::ByReference, &toolbox(&["Read"]), &[]);
        let later = toolbox(&["Read", "mcp__x__pin"]);
        let changes = inline.changes(&later, false);
        assert_eq!(
            text(&changes.blocks[0]),
            r#"{"type":"tool_addition","tool":{"type":"tool_reference","name":"mcp__x__pin"}}"#
        );
        let declared = inline.declared(&later);
        let pin: serde_json::Value = serde_json::from_slice(declared[1].json).expect("json");
        assert_eq!(pin["defer_loading"], true);
        assert!(!declared[1].deferred, "declared from the start, not once loaded");
        // Claude Code withdraws only what it defined by value.
        assert!(inline.changes(&toolbox(&["Read"]), false).is_empty(), "no removal by reference");
        assert_eq!(names(&inline, &toolbox(&["Read"])), ["Read", "mcp__x__pin"]);
    }

    #[test]
    fn a_resumed_conversation_keeps_its_inline_tools_out_of_the_array() {
        let mut first = Inline::default();
        first.latch(ToolChanges::ByValue, &toolbox(&["Read"]), &[]);
        let later = toolbox(&["Read", "mcp__x__pin"]);
        let history = vec![note(first.changes(&later, false))];
        let mut resumed = Inline::default();
        resumed.latch(ToolChanges::ByValue, &later, &history);
        assert_eq!(names(&resumed, &later), ["Read"]);
        assert!(resumed.changes(&later, false).is_empty());
    }

    #[test]
    fn a_tool_back_with_the_same_definition_points_to_it() {
        let mut inline = Inline::default();
        inline.latch(ToolChanges::ByValue, &toolbox(&["Read"]), &[]);
        let with = toolbox(&["Read", "mcp__x__pin"]);
        let mut history = vec![note(inline.changes(&with, false))];
        history.push(note(inline.changes(&toolbox(&["Read"]), false)));
        let back = inline.changes(&with, false);
        assert_eq!(back.added, ["mcp__x__pin"]);
        assert_eq!(
            text(&back.blocks[0]),
            r#"{"type":"tool_addition","tool":{"type":"same_as_earlier","name":"mcp__x__pin"}}"#
        );
        history.push(note(back));
        // The request carries the definition itself.
        let wire = inline.wire_history(&history).expect("rewritten");
        let sent: serde_json::Value = serde_json::from_slice(wire[2].json()).expect("json");
        assert_eq!(sent["content"][1]["tool"]["type"], "tool_definition");
        assert_eq!(sent["content"][1]["tool"]["definition"]["name"], "mcp__x__pin");
        assert!(inline.wire_history(&history[..2]).is_none(), "no pointer, nothing to rewrite");
        // A resumed conversation reads the pointer back.
        let mut resumed = Inline::default();
        resumed.latch(ToolChanges::ByValue, &with, &history);
        assert_eq!(names(&resumed, &with), ["Read"]);
        assert!(resumed.changes(&with, false).is_empty());
        // A changed definition is sent afresh.
        let mut other = Inline::default();
        other.latch(ToolChanges::ByValue, &toolbox(&["Read"]), &[]);
        other.changes(&with, false);
        other.changes(&toolbox(&["Read"]), false);
        let changed = Toolbox::new(vec![
            Arc::new(Named(ToolSpec::new("Read", "d", serde_json::json!({"type": "object"})))) as Arc<dyn Tool>,
            Arc::new(Named(ToolSpec::new("mcp__x__pin", "new", serde_json::json!({"type": "object"})))),
        ]);
        assert!(text(&other.changes(&changed, false).blocks[0]).contains(r#""type":"tool_definition""#));
    }

    #[test]
    fn refusals_fall_back_and_strip_the_blocks() {
        let refusals = ToolChangeRefusals::default();
        let mut inline = Inline::default();
        inline.latch(ToolChanges::ByValue, &toolbox(&["Read"]), &[]);
        let later = toolbox(&["Read", "mcp__x__pin"]);
        let history = vec![StoredMessage::user_text("hi"), note(inline.changes(&later, false))];
        let platform = ModelError::Invalid("400: Inline tool definitions are not available on this platform".into());
        assert!(inline.refused(&platform, "claude-opus-5-5", &refusals));
        inline.latch(ToolChanges::ByValue, &later, &history);
        assert_eq!(inline.mode(), ToolChanges::ByReference);
        assert_eq!(names(&inline, &later), ["Read", "mcp__x__pin"], "declared in the array from now on");
        assert_eq!(refusals.allowed(ToolChanges::ByValue, "m"), ToolChanges::ByReference, "for every agent");
        let wire = inline.wire_history(&history).expect("rewritten");
        assert_eq!(wire.len(), 1, "nothing is left of the note: {wire:?}");
        // The API then refuses tool additions on this model.
        let model = ModelError::Invalid("400: tool_addition blocks are not supported on this model".into());
        assert!(inline.refused(&model, "claude-x", &refusals));
        inline.latch(ToolChanges::ByValue, &later, &history);
        assert_eq!(inline.mode(), ToolChanges::InTools);
        assert_eq!(refusals.allowed(ToolChanges::ByReference, "claude-x"), ToolChanges::InTools);
        assert_eq!(refusals.allowed(ToolChanges::ByReference, "claude-y"), ToolChanges::ByReference);
        assert!(!inline.refused(&model, "claude-x", &refusals), "nothing left to refuse");
        assert_eq!(inline.after_summary().ceiling(ToolChanges::ByValue), ToolChanges::ByReference);
    }

    #[test]
    fn refusals_are_read_as_claude_code_reads_them() {
        let change = |s, m| change_refusal(s, m);
        let header = "Unexpected value(s) `mid-conversation-tool-changes-2026-07-01` for the `anthropic-beta` header";
        assert_eq!(change(400, header), Some(ChangeRefusal::HeaderRejected));
        assert_eq!(
            change(422, "messages.3.content.1: Input tag 'tool_addition' found"),
            Some(ChangeRefusal::BlockTypeUnknown)
        );
        assert_eq!(change(400, "tool_addition after a paused assistant turn"), Some(ChangeRefusal::AfterPausedTurn));
        assert_eq!(change(400, "tool_addition: too many, exceeds 20"), Some(ChangeRefusal::TooManyDefinitions));
        assert_eq!(change(400, "tool_addition references unknown tool x"), Some(ChangeRefusal::UnknownToolReference));
        assert_eq!(change(400, "tool_additions are odd"), None, "a whole word");
        assert_eq!(change(400, "tool_addition.cache_control: Extra inputs are not permitted"), None);
        assert_eq!(change(500, "tool_addition"), None);
        let inline = |s, m| inline_refusal(s, m);
        assert_eq!(inline(400, "`inline-tools-2026-09-15` in anthropic-beta"), Some(InlineRefusal::HeaderRejected));
        assert_eq!(
            inline(400, "Input tag 'tool_definition' found using 'type'"),
            Some(InlineRefusal::UnsupportedOnPlatform)
        );
        assert_eq!(inline(400, "MCP tools cannot yet be defined in a message"), Some(InlineRefusal::KindNotEnabled));
        assert_eq!(
            inline(400, "tool_addition: name x is already used by a declared tool"),
            Some(InlineRefusal::NameConflict)
        );
        assert_eq!(inline(400, "something else"), None);
    }
}
