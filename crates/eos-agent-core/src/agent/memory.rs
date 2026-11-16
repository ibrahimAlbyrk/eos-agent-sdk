use std::collections::BTreeSet;

use eos_agent_types::{ContentBlock, Role, Usage};

use crate::message::StoredMessage;
use crate::ports::{AgentEvent, TranscriptStore};

use super::{Agent, Deps, RewindError, TurnError, deferred, loaded_tools};

impl Agent {
    /// Drops the in-memory history of an idle agent whose transcript store
    /// holds it; [`Agent::restore_history`] puts it back before the next turn.
    pub fn release_history(&mut self) {
        self.transcript = Vec::new();
    }

    /// Puts back a history released by [`Agent::release_history`], as read
    /// from the transcript store.
    pub fn restore_history(&mut self, history: Vec<StoredMessage>) {
        self.transcript = history.into_iter().map(StoredMessage::fitting_images).collect();
    }

    /// Cuts the conversation back to its first `keep` messages (a rewind):
    /// the later ones are forgotten here and in `store`, and the agent goes
    /// on as one resumed from what is left (rewound to nothing, or to the
    /// host's context only, as a new conversation with its session context).
    /// A cut may not keep a tool call without its result. What the tools
    /// remember of the dropped messages (files read, tasks listed) stays.
    pub async fn truncate(&mut self, keep: usize, store: Option<&dyn TranscriptStore>) -> Result<(), RewindError> {
        check_cut(&self.transcript, keep)?;
        if keep == self.transcript.len() {
            return Ok(());
        }
        if let Some(store) = store {
            store.replace(&self.id, &self.transcript[..keep]).await?;
        }
        self.transcript.truncate(keep);
        self.after_cut();
        Ok(())
    }

    /// Replaces the conversation with `seed`, which runs no turn: the host's
    /// own summary of it (Claude Code's compaction seed, sent with
    /// `shouldQuery: false`). The seed is the host's context
    /// ([`StoredMessage::is_meta`]): the conversation opens at the next input
    /// after it, with the session context as a new one does, and a rewind to
    /// that input keeps the seed.
    pub async fn replace_context(&mut self, seed: Vec<ContentBlock>, deps: Deps<'_>) -> Result<(), TurnError> {
        let seed = StoredMessage::new(Role::User, &seed)?.with_meta(true);
        if let Some(store) = deps.store {
            store.replace(&self.id, std::slice::from_ref(&seed)).await?;
        }
        self.transcript = vec![seed.clone()];
        self.after_cut();
        deps.events.emit(AgentEvent::Message(seed)).await;
        Ok(())
    }

    /// What a cut leaves: only the deferred tools the kept messages loaded,
    /// and a conversation back before its opening opens again with its
    /// session context.
    fn after_cut(&mut self) {
        let mut loaded = BTreeSet::new();
        for message in &self.transcript {
            loaded_tools(message, &mut loaded);
        }
        self.kept.retain(|name, _| loaded.contains(name));
        self.loaded = loaded;
        self.announced = deferred::Announced::default();
        if self.unopened() {
            // The next input opens the conversation again.
            self.session_notes_due = false;
            if let Some(context) = &self.session {
                self.announced.prime(&self.toolbox, &context.servers);
            }
        }
        self.last_usage = Usage::default();
    }

    /// Nothing but the host's context so far: the next input opens the
    /// conversation.
    pub(super) fn unopened(&self) -> bool {
        self.transcript.iter().all(StoredMessage::is_meta)
    }
}

/// Whether `history` can be cut to its first `keep` messages: the last one
/// kept may not be the model's with a tool call (its results come next).
pub fn check_cut(history: &[StoredMessage], keep: usize) -> Result<(), RewindError> {
    if keep > history.len() {
        return Err(RewindError::OutOfRange { len: history.len() });
    }
    let calls_tools = |m: &StoredMessage| {
        m.role() == Role::Assistant
            && m.blocks().is_ok_and(|blocks| blocks.iter().any(|b| matches!(b, ContentBlock::ToolUse { .. })))
    };
    match history[..keep].last() {
        Some(last) if keep < history.len() && calls_tools(last) => Err(RewindError::SplitsToolCall { keep }),
        _ => Ok(()),
    }
}
