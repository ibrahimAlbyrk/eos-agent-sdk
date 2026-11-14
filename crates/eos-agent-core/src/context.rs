//! What the agent knows besides the conversation: the session context sent
//! with the first message, reminder sources consulted before every request,
//! and the settings for summarizing a conversation that outgrows its window.

use std::sync::Arc;

use eos_agent_types::ContentBlock;

use crate::BoxFuture;
use crate::extensions::Extensions;
use crate::message::StoredMessage;

/// Sent once, with the first user message: `user_prefix` blocks go in front of
/// the user's own text (Claude Code puts CLAUDE.md, the date and similar there,
/// inside `<system-reminder>`), and each of `system_notes` follows as a
/// `role: "system"` message (environment, skills, deferred tools) right
/// before the first request, after the notes queued for it. Keeping these
/// out of the system prompt keeps that prompt identical across sessions, so
/// its cache is shared.
///
/// Both parts are shared, not copied: agents with the same context can hold
/// one prefix and one serialized note between them, and the note's bytes go
/// into each conversation as they are.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub user_prefix: Arc<[ContentBlock]>,
    /// `role: "system"` messages.
    pub system_notes: Vec<StoredMessage>,
    /// What the notes say about MCP servers: later changes are announced
    /// against it.
    pub servers: crate::ServerNotes,
}

impl SessionContext {
    /// The text of each system note, in order.
    pub fn note_texts(&self) -> Vec<String> {
        self.system_notes.iter().map(|note| note.text()).collect()
    }
}

/// Consulted before every model request; whatever it returns reaches the
/// model as reminder text (files changed on disk, the date rolled over, …).
pub trait ReminderSource: Send + Sync + std::fmt::Debug {
    fn collect<'a>(&'a self, extensions: &'a Extensions) -> BoxFuture<'a, Vec<String>>;
}

/// The words of summarizing: what to ask the model, and how its answer
/// becomes the message that replaces the conversation.
pub trait CompactionText: Send + Sync + std::fmt::Debug {
    /// The instruction sent as the last user message; `focus` is the user's
    /// own `/compact` instructions, if any.
    fn instruction(&self, focus: Option<&str>) -> String;

    /// The user message that replaces the conversation, from the model's reply.
    fn seed(&self, summary: &str) -> String;
}

/// When and how a conversation is summarized to free its context window.
#[derive(Debug, Clone)]
pub struct Compaction {
    /// Summarize once the live context reaches this many tokens, and when a
    /// request turns out not to fit the model's window. `None`: never on its
    /// own, only when the host asks ([`Agent::compact`](crate::Agent::compact)).
    pub threshold_tokens: Option<u64>,
    pub text: Arc<dyn CompactionText>,
}
