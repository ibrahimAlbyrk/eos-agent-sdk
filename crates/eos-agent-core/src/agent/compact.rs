use bytes::Bytes;
use eos_agent_types::{ContentBlock, Role, Usage};
use tokio_util::sync::CancellationToken;

use super::{Agent, Deps, Streamed, TurnError};
use crate::message::StoredMessage;
use crate::model::ModelError;
use crate::ports::AgentEvent;

/// How many times a summary request that does not fit drops the older half.
const MAX_TRIMS: usize = 2;

impl Agent {
    /// The first message at or after `at` that a request can start with: a
    /// user message that answers no tool call (`None` when there is none).
    fn later_start(&self, at: usize) -> Option<usize> {
        (at.max(1)..self.transcript.len()).find(|&i| {
            let m = &self.transcript[i];
            m.role() == Role::User
                && m.blocks().is_ok_and(|b| !b.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
        })
    }

    /// Does the agent summarize on its own (at a threshold, or a request
    /// that did not fit)?
    pub(super) fn compacts_itself(&self) -> bool {
        self.config.compaction.as_ref().is_some_and(|c| c.threshold_tokens.is_some())
    }

    /// Has the live context reached the compaction threshold?
    pub(super) fn needs_compaction(&self) -> bool {
        let Some(threshold) = self.config.compaction.as_ref().and_then(|c| c.threshold_tokens) else { return false };
        let since_last_reply = match self.transcript.last() {
            Some(m) if m.role() != Role::Assistant => m.estimated_tokens(),
            _ => 0,
        };
        let used = self.last_usage.context_tokens() + self.last_usage.output_tokens.unwrap_or(0) + since_last_reply;
        used >= threshold
    }

    /// Replaces the conversation with a model-written summary of it. `focus`
    /// adds the user's own instructions for what the summary should keep.
    /// Returns false when there is nothing to summarize or no summary came back.
    pub async fn compact(
        &mut self,
        focus: Option<&str>,
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> Result<bool, TurnError> {
        let Some(compaction) = self.config.compaction.clone() else { return Ok(false) };
        if self.transcript.is_empty() {
            return Ok(false);
        }
        let instruction = compaction.text.instruction(focus.filter(|f| !f.trim().is_empty()));
        let before_tokens = self.last_usage.context_tokens();
        deps.events.emit(AgentEvent::Compacting { before_tokens }).await;

        // Same system prompt, tools and history as a normal request, so the
        // whole conversation is read from cache; only tool use is turned off.
        let build = |agent: &Self, dropped: usize| {
            let mut request = agent.build_request();
            request.messages.drain(..dropped.min(request.messages.len()));
            request.push_user(StoredMessage::user_text(instruction.clone()).json().clone());
            request.extra = Some(with_member(request.extra.as_ref(), r#""tool_choice":{"type":"none"}"#));
            request
        };
        let mut request = build(self, 0);

        let (mut trims, mut dropped) = (0, 0);
        let streamed = loop {
            match self.request(request.clone(), deps, cancel).await {
                Err(TurnError::Model(error)) if self.tool_changes_refused(&error) => request = build(self, dropped),
                // Too long to summarize whole: summarize the later half.
                Err(TurnError::Model(ModelError::ContextWindowExceeded(_))) if trims < MAX_TRIMS => {
                    let kept = self.transcript.len() - dropped;
                    let Some(from) = self.later_start(dropped + kept / 2) else {
                        return Err(ModelError::ContextWindowExceeded("nothing left to drop".into()).into());
                    };
                    request.messages.drain(..from - dropped);
                    // Counted in the transcript, cut in the request: what was cut must not split a call from its result.
                    super::pairing::repair_wire(&mut request.messages);
                    dropped = from;
                    trims += 1;
                }
                other => break other?,
            }
        };
        let summary = match streamed {
            Streamed::Done(done) => text_of(&done.content),
            Streamed::Cancelled(_) => return Ok(false),
        };
        if summary.trim().is_empty() {
            return Err(ModelError::Transient("the summary came back empty".into()).into());
        }

        // The summary opens the conversation again as a first input does: the
        // session context's prefix (CLAUDE.md, the date) in front of it, and
        // its system notes before the next request.
        let mut blocks: Vec<ContentBlock> = self.session.as_ref().map(|s| s.user_prefix.to_vec()).unwrap_or_default();
        blocks.push(ContentBlock::Text { text: compaction.text.seed(&summary), citations: None });
        let seed = StoredMessage::new(Role::User, &blocks)?;
        if let Some(store) = deps.store {
            store.replace(&self.id, std::slice::from_ref(&seed)).await?;
        }
        self.transcript.clear();
        self.transcript.push(seed.clone());
        self.session_notes_due = self.session.is_some();
        self.announced.forget_inline();
        self.latch_tool_changes();
        self.last_usage = Usage::default();
        deps.events.emit(AgentEvent::Message(seed)).await;
        deps.events.emit(AgentEvent::Compacted { before_tokens }).await;
        Ok(true)
    }
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Adds one `"key":value` member to a JSON object given as bytes.
fn with_member(object: Option<&Bytes>, member: &str) -> Bytes {
    let inner = object
        .and_then(|o| o.strip_prefix(b"{"))
        .and_then(|o| o.strip_suffix(b"}"))
        .filter(|i| i.iter().any(|b| !b.is_ascii_whitespace()));
    let mut out = Vec::with_capacity(member.len() + inner.map_or(0, <[u8]>::len) + 3);
    out.push(b'{');
    if let Some(inner) = inner {
        out.extend_from_slice(inner);
        out.push(b',');
    }
    out.extend_from_slice(member.as_bytes());
    out.push(b'}');
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_members_to_objects() {
        assert_eq!(&with_member(None, r#""a":1"#)[..], br#"{"a":1}"#);
        let existing = Bytes::from_static(br#"{"b":2}"#);
        assert_eq!(&with_member(Some(&existing), r#""a":1"#)[..], br#"{"b":2,"a":1}"#);
    }
}
