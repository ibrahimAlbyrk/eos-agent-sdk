//! Every tool call paired with its result. The API takes a conversation only
//! when the tool calls of each model message are answered at the start of
//! the very next message (a user message, one result per call) and every
//! result answers a call of the message right before it.
//!
//! The conversation is recorded so that this holds, and a request is built
//! through [`repaired`], so that whatever a transcript holds (one written
//! by an older engine, a turn cut off by a quit, a carried-over record) the
//! request is valid: an unanswered call gets a result saying it was cut off,
//! a result found later is moved up to its call, a result with no call
//! before it is left out.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use eos_agent_types::{ContentBlock, Role, ToolResultContent};
use serde::Deserialize;

use crate::message::StoredMessage;

/// The result of a call the conversation holds no result for.
pub(super) const UNANSWERED: &str =
    "This call was cut off before its result was recorded: it may not have run, or run only in part.";
/// What is left of a model message whose only blocks were calls that cannot be sent.
const CALL_REMOVED: &str = "[Tool call removed: it was never answered]";
/// What is left of a user message whose only blocks were results of no call.
const RESULT_REMOVED: &str = "[Tool result removed: its call is not in the conversation]";

#[derive(Deserialize)]
struct Body<'a> {
    #[serde(borrow)]
    content: Vec<Head<'a>>,
}

/// What pairing needs of one content block (the rest is skipped unread).
#[derive(Deserialize)]
struct Head<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    tool_use_id: Option<Cow<'a, str>>,
}

impl Head<'_> {
    fn is_call(&self) -> bool {
        self.kind == "tool_use"
    }

    fn is_result(&self) -> bool {
        self.kind == "tool_result"
    }

    /// A call the API runs itself: its result is in the same message.
    fn is_server_call(&self) -> bool {
        matches!(&*self.kind, "server_tool_use" | "mcp_tool_use")
    }

    fn id(&self) -> &str {
        self.id.as_deref().unwrap_or_default()
    }

    fn answers(&self) -> Option<&str> {
        self.tool_use_id.as_deref()
    }
}

fn heads(message: &StoredMessage) -> Option<Vec<Head<'_>>> {
    serde_json::from_slice::<Body<'_>>(message.json()).ok().map(|b| b.content)
}

/// The history with its tool calls and results paired as the API takes
/// them; `None` when they already are (the common case: nothing is copied).
pub(super) fn repaired(history: &[StoredMessage]) -> Option<Vec<StoredMessage>> {
    if is_paired(history) { None } else { Some(repair(history)) }
}

/// [`repaired`] on a request's messages as they go out.
pub(super) fn repair_wire(messages: &mut Vec<bytes::Bytes>) {
    let stored: Option<Vec<StoredMessage>> =
        messages.iter().map(|b| StoredMessage::from_wire(b.clone()).ok()).collect();
    if let Some(fixed) = stored.as_deref().and_then(repaired) {
        *messages = fixed.iter().map(|m| m.json().clone()).collect();
    }
}

/// Does every call have its result at the start of the next message, and
/// every result its call in the message before?
pub(super) fn is_paired(history: &[StoredMessage]) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut owed: Option<Vec<&str>> = None;
    let heads: Vec<Option<Vec<Head<'_>>>> = history.iter().map(heads).collect();
    for (i, (message, heads)) in history.iter().zip(&heads).enumerate() {
        let Some(heads) = heads else {
            if owed.is_some() {
                return false;
            }
            continue;
        };
        match message.role() {
            Role::Assistant => {
                if owed.is_some() {
                    return false;
                }
                let last = i + 1 == history.len();
                let mut calls = Vec::new();
                for head in heads {
                    if head.is_result() {
                        return false;
                    }
                    if head.is_call() {
                        if !seen.insert(head.id()) {
                            return false;
                        }
                        calls.push(head.id());
                    } else if head.is_server_call() && !last && !server_answered(heads, head.id()) {
                        return false;
                    }
                }
                owed = (!calls.is_empty()).then_some(calls);
            }
            Role::User => {
                let leading = heads.iter().take_while(|h| h.is_result()).count();
                if heads[leading..].iter().any(Head::is_result) {
                    return false;
                }
                let answered: Vec<&str> = heads[..leading].iter().map(|h| h.answers().unwrap_or_default()).collect();
                match owed.take() {
                    Some(calls) => {
                        let unique: HashSet<&str> = answered.iter().copied().collect();
                        if unique.len() != answered.len()
                            || answered.len() != calls.len()
                            || !calls.iter().all(|c| unique.contains(c))
                        {
                            return false;
                        }
                    }
                    None if leading > 0 => return false,
                    None => {}
                }
            }
            Role::System => {
                if owed.is_some() {
                    return false;
                }
            }
        }
    }
    owed.is_none()
}

/// A server call's result block sits beside it (any block naming it that is
/// not a client tool's result).
fn server_answered(heads: &[Head<'_>], id: &str) -> bool {
    heads.iter().any(|h| !h.is_result() && h.answers() == Some(id))
}

fn repair(history: &[StoredMessage]) -> Vec<StoredMessage> {
    let mut out: Vec<StoredMessage> = Vec::with_capacity(history.len() + 1);
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < history.len() {
        let message = &history[i];
        i += 1;
        if message.role() != Role::Assistant {
            // Not right after a model message with calls: no result belongs here.
            push_without_results(&mut out, message);
            continue;
        }
        let (message, calls) = sendable_calls(message, &mut seen, i == history.len());
        out.push(message);
        if calls.is_empty() {
            continue;
        }
        let end = history[i..].iter().position(|m| m.role() == Role::Assistant).map_or(history.len(), |p| i + p);
        // Each call's first result before the next model message, wherever it was put.
        let mut found: HashMap<String, ContentBlock> = HashMap::new();
        for later in history[i..end].iter().filter(|m| m.role() == Role::User) {
            for block in later.blocks().unwrap_or_default() {
                if let ContentBlock::ToolResult { tool_use_id, .. } = &block
                    && calls.contains(tool_use_id)
                    && !found.contains_key(tool_use_id)
                {
                    found.insert(tool_use_id.clone(), block);
                }
            }
        }
        let mut answer: Vec<ContentBlock> =
            calls.iter().map(|id| found.remove(id).unwrap_or_else(|| unanswered(id))).collect();
        let mut rest = i;
        if let Some(next) = history.get(i).filter(|m| m.role() == Role::User)
            && let Ok(blocks) = next.blocks()
        {
            answer.extend(blocks.into_iter().filter(|b| !matches!(b, ContentBlock::ToolResult { .. })));
            rest = i + 1;
        }
        out.extend(StoredMessage::new(Role::User, &answer).ok());
        for later in &history[rest..end] {
            push_without_results(&mut out, later);
        }
        i = end;
    }
    out
}

/// A synthetic result for call `id`.
pub(super) fn unanswered(id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_owned(),
        content: Some(ToolResultContent::Text(UNANSWERED.to_owned())),
        is_error: true,
    }
}

/// A model message as it can be sent, with the ids of its calls: without
/// results (only a user message holds them), calls whose id an earlier
/// call already had, or server calls whose result is not beside them
/// (unless it is the last message: a paused turn the API goes on with).
fn sendable_calls(message: &StoredMessage, seen: &mut HashSet<String>, last: bool) -> (StoredMessage, Vec<String>) {
    let Some(heads) = heads(message) else { return (message.clone(), Vec::new()) };
    let mut calls = Vec::new();
    let mut clean = true;
    let mut ids = HashSet::new();
    for head in &heads {
        if head.is_result() || (head.is_server_call() && !last && !server_answered(&heads, head.id())) {
            clean = false;
        } else if head.is_call() {
            if seen.contains(head.id()) || !ids.insert(head.id()) {
                clean = false;
            } else {
                calls.push(head.id().to_owned());
            }
        }
    }
    drop(heads);
    if clean {
        seen.extend(calls.iter().cloned());
        return (message.clone(), calls);
    }
    let Ok(blocks) = message.blocks() else { return (message.clone(), Vec::new()) };
    let answered: HashSet<String> = blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Unknown(raw) => {
                serde_json::from_str::<Head<'_>>(raw.get()).ok()?.answers().map(str::to_owned)
            }
            _ => None,
        })
        .collect();
    let mut calls = Vec::new();
    let mut kept: Vec<ContentBlock> = Vec::with_capacity(blocks.len());
    for block in blocks {
        let keep = match &block {
            ContentBlock::ToolResult { .. } => false,
            ContentBlock::ToolUse { id, .. } => {
                let fresh = seen.insert(id.clone());
                if fresh {
                    calls.push(id.clone());
                }
                fresh
            }
            ContentBlock::ServerToolUse { id, .. } => last || answered.contains(id),
            ContentBlock::Unknown(raw) => serde_json::from_str::<Head<'_>>(raw.get())
                .map_or(true, |h| !h.is_server_call() || last || answered.contains(h.id())),
            _ => true,
        };
        if keep {
            kept.push(block);
        }
    }
    if kept.is_empty() {
        kept.push(ContentBlock::Text { text: CALL_REMOVED.to_owned(), citations: None });
    }
    match StoredMessage::new(Role::Assistant, &kept) {
        Ok(message) => (message, calls),
        Err(_) => (message.clone(), Vec::new()),
    }
}

/// `message` without tool results; one left empty is dropped, or noted where
/// dropping it would leave the conversation opening with, or two model
/// messages in a row around, nothing of the user's.
fn push_without_results(out: &mut Vec<StoredMessage>, message: &StoredMessage) {
    if message.role() != Role::User || !heads(message).is_some_and(|h| h.iter().any(Head::is_result)) {
        out.push(message.clone());
        return;
    }
    let Ok(blocks) = message.blocks() else { return };
    let kept: Vec<ContentBlock> =
        blocks.into_iter().filter(|b| !matches!(b, ContentBlock::ToolResult { .. })).collect();
    if !kept.is_empty() {
        out.extend(StoredMessage::new(Role::User, &kept).ok());
    } else if out.last().is_none_or(|m| m.role() == Role::Assistant) {
        out.push(StoredMessage::user_text(RESULT_REMOVED));
    }
}

/// The calls of the conversation's last model message that nothing after it
/// answers (a turn that ended, or was cut off, before their results).
pub(super) fn unanswered_calls(history: &[StoredMessage]) -> Vec<String> {
    let Some(at) = history.iter().rposition(|m| m.role() == Role::Assistant) else { return Vec::new() };
    let Some(model) = heads(&history[at]) else { return Vec::new() };
    let calls: Vec<&str> = model.iter().filter(|h| h.is_call()).map(Head::id).collect();
    if calls.is_empty() {
        return Vec::new();
    }
    let answered: Vec<Vec<Head<'_>>> = history[at + 1..].iter().filter_map(heads).collect();
    let answered: HashSet<&str> =
        answered.iter().flatten().filter(|h| h.is_result()).filter_map(Head::answers).collect();
    let mut unique = HashSet::new();
    calls.into_iter().filter(|c| !answered.contains(c) && unique.insert(*c)).map(str::to_owned).collect()
}
