use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use eos_agent_types::{ContentBlock, ToolResultContent};
use serde::Serialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::BoxFuture;
use crate::extensions::Extensions;
use crate::message::StoredMessage;

/// A tool's definition as the API receives it, serialized once.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    name: Arc<str>,
    json: Bytes,
    deferred: bool,
    search_hint: Option<Arc<str>>,
}

impl ToolSpec {
    pub fn new(name: &str, description: &str, input_schema: serde_json::Value) -> Self {
        #[derive(Serialize)]
        struct Def<'a> {
            name: &'a str,
            description: &'a str,
            input_schema: serde_json::Value,
        }
        let json = serde_json::to_vec(&Def { name, description, input_schema }).expect("a tool definition serializes");
        Self { name: name.into(), json: Bytes::from(json), deferred: false, search_hint: None }
    }

    /// A definition taken verbatim (e.g. from an MCP server or a captured fixture).
    pub fn from_json(name: &str, json: Bytes) -> Self {
        Self { name: name.into(), json, deferred: false, search_hint: None }
    }

    /// Sent with `defer_loading`: the model sees only its name until it loads
    /// the definition through tool search, which keeps rarely used tools out
    /// of the context.
    pub fn deferred(mut self) -> Self {
        if !self.deferred {
            let json = &self.json[..];
            let mut out = Vec::with_capacity(json.len() + 24);
            out.extend_from_slice(&json[..json.len() - 1]);
            out.extend_from_slice(br#","defer_loading":true}"#);
            self.json = Bytes::from(out);
            self.deferred = true;
        }
        self
    }

    pub fn is_deferred(&self) -> bool {
        self.deferred
    }

    /// Sent in full again (for models without tool search).
    pub fn undeferred(mut self) -> Self {
        if !self.deferred {
            return self;
        }
        const MEMBER: &[u8] = br#","defer_loading":true}"#;
        self.json = match self.json.strip_suffix(MEMBER) {
            Some(head) => {
                let mut out = Vec::with_capacity(head.len() + 1);
                out.extend_from_slice(head);
                out.push(b'}');
                Bytes::from(out)
            }
            None => {
                let mut def: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_slice(&self.json).unwrap_or_default();
                def.remove("defer_loading");
                Bytes::from(serde_json::to_vec(&def).expect("a tool definition serializes"))
            }
        };
        self.deferred = false;
        self
    }

    /// Words that help tool search find this tool.
    pub fn with_search_hint(mut self, hint: &str) -> Self {
        self.search_hint = Some(hint.into());
        self
    }

    pub fn search_hint(&self) -> Option<&str> {
        self.search_hint.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn json(&self) -> &Bytes {
        &self.json
    }
}

/// What a tool hands back to the model.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: ToolResultContent,
    pub is_error: bool,
    /// The turn ends once this call's result is recorded (Claude Code's
    /// `endsTurn`: a subagent that delivered its structured answer is done).
    pub ends_turn: bool,
    /// Blocks that go into the conversation beside the result, after the
    /// results of the response's calls (Claude Code's `contentBlocks`: images
    /// the user attached to an answer).
    pub beside: Vec<ContentBlock>,
    /// What the call did, structured, for the host only (never sent to the
    /// model): a JSON object in the shape of Claude Code's `toolUseResult`
    /// (an edit's `structuredPatch` hunks). It reaches the host with the
    /// call's `ToolResult` event.
    pub details: Option<Arc<RawValue>>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: ToolResultContent::Text(text.into()),
            is_error: false,
            ends_turn: false,
            beside: Vec::new(),
            details: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(text) }
    }

    pub fn blocks(blocks: Vec<ContentBlock>) -> Self {
        Self { content: ToolResultContent::Blocks(blocks), ..Self::text(String::new()) }
    }

    /// This output with `blocks` beside it.
    pub fn with_beside(mut self, blocks: Vec<ContentBlock>) -> Self {
        self.beside = blocks;
        self
    }

    /// This output with `details` for the host (anything that serializes to
    /// a JSON object).
    pub fn with_details(mut self, details: &impl Serialize) -> Self {
        self.details = serde_json::value::to_raw_value(details).ok().map(Arc::from);
        self
    }

    /// This output, ending the turn once recorded.
    pub fn ending_turn(mut self) -> Self {
        self.ends_turn = true;
        self
    }
}

/// What the permission gate is shown instead of a call's raw input: the
/// input with what it refers to filled in (the script a workflow call names
/// by path), and Claude Code's prompt for it.
#[derive(Debug, Clone)]
pub struct ToolReview {
    pub message: String,
    pub input: Arc<RawValue>,
}

/// What a tool sees of the agent calling it.
#[derive(Debug, Clone, Copy)]
pub struct ToolContext<'a> {
    pub agent_id: &'a str,
    pub tool_use_id: &'a str,
    pub cancel: &'a CancellationToken,
    /// The calling agent's own state for tools.
    pub extensions: &'a Extensions,
    /// The conversation so far, ending with the message that holds this call
    /// (what a fork starts from).
    pub transcript: &'a [StoredMessage],
}

/// A capability the model can call. Tools are stateless and shared by every
/// agent; per-agent state goes in [`ToolContext::extensions`], dependencies
/// come in through the tool's constructor.
pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> &ToolSpec;

    /// May this call run alongside other concurrency-safe calls?
    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        false
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput>;

    /// Will this call of the agent with these `extensions` run inside the
    /// OS sandbox? The permission gate sees it as
    /// [`ToolCall::sandboxed`](crate::ToolCall::sandboxed).
    fn runs_sandboxed(&self, _input: &RawValue, _extensions: &Extensions) -> bool {
        false
    }

    /// The input as the permission gate sees it, when it differs from the
    /// model's (Claude Code's `backfillObservableInput`): a path made
    /// absolute, so a host's rule for a path holds however the model wrote
    /// it. The tool still runs with the model's input unless the gate
    /// answers with another.
    fn observable_input(&self, _input: &RawValue, _extensions: &Extensions) -> Option<Arc<RawValue>> {
        None
    }

    /// What the permission gate should review for this call, when it is not
    /// the raw input alone; see [`ToolReview`].
    fn review<'a>(&'a self, _input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, Option<ToolReview>> {
        Box::pin(async { None })
    }

    /// Rebuilds the agent's state for this tool from a conversation it is
    /// resuming (`history`, oldest first; messages read back from a store
    /// carry [`StoredMessage::recorded_ms`]). Called once, before the first
    /// turn of an agent that starts with history.
    fn restore<'a>(&'a self, _history: &'a [StoredMessage], _extensions: &'a Extensions) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// An agent's tool set, with the `tools` array serialized once. Agents that
/// share a toolbox share one copy of every definition.
#[derive(Clone, Default)]
pub struct Toolbox {
    tools: Vec<Arc<dyn Tool>>,
    index: HashMap<Arc<str>, usize>,
    json: Option<Bytes>,
}

impl std::fmt::Debug for Toolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.tools.iter().map(|t| t.spec().name())).finish()
    }
}

impl Toolbox {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut index = HashMap::with_capacity(tools.len());
        let mut json = Vec::new();
        for (i, tool) in tools.iter().enumerate() {
            index.insert(Arc::clone(&tool.spec().name), i);
            json.push(if i == 0 { b'[' } else { b',' });
            json.extend_from_slice(tool.spec().json());
        }
        let json = (!tools.is_empty()).then(|| {
            json.push(b']');
            Bytes::from(json)
        });
        Self { tools, index, json }
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.index.get(name).map(|&i| &self.tools[i])
    }

    pub fn json(&self) -> Option<&Bytes> {
        self.json.as_ref()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|t| t.spec().name())
    }

    /// The tools in request order (to build a narrower toolbox from).
    pub fn tools(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }
}
