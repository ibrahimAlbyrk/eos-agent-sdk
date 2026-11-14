//! How a user turn's input joins the conversation, with or without a model
//! request after it.

use eos_agent_types::{ContentBlock, Role, StopReason};

use super::{Agent, Deps, INTERRUPTED, TurnError, TurnOutcome};
use crate::message::StoredMessage;
use crate::ports::AgentEvent;

impl Agent {
    /// Adds the user's input; the input that opens a conversation (the
    /// first, or the first after the host's seed) carries the session
    /// context's user prefix (its system notes follow before the
    /// conversation's first request: [`push_session_notes`]).
    ///
    /// [`push_session_notes`]: Self::push_session_notes
    pub(super) async fn push_input(&mut self, input: Vec<ContentBlock>, deps: Deps<'_>) -> Result<(), TurnError> {
        // Kept, so a conversation rewound to its start opens the same way.
        let prefix = self.session.as_ref().filter(|_| self.unopened()).map(|s| s.user_prefix.clone());
        let message = match prefix {
            Some(prefix) => {
                let mut first = Vec::with_capacity(prefix.len() + input.len());
                first.extend(prefix.iter().cloned());
                first.extend(input);
                self.session_notes_due = true;
                StoredMessage::new(Role::User, &first)?
            }
            None => StoredMessage::new(Role::User, &input)?,
        };
        self.push(message, deps).await
    }

    /// The session context's `role: "system"` notes, once, ahead of the
    /// first request of a conversation [`push_input`](Self::push_input)
    /// opened, after everything else that request carries (the input,
    /// queued notes): the API takes a system turn only right before the
    /// model's reply.
    pub(super) async fn push_session_notes(&mut self, deps: Deps<'_>) -> Result<(), TurnError> {
        if !std::mem::take(&mut self.session_notes_due) {
            return Ok(());
        }
        let notes = self.session.as_ref().map(|s| s.system_notes.clone()).unwrap_or_default();
        for note in notes {
            self.push(note, deps).await?;
        }
        Ok(())
    }

    /// A user turn that makes no model request: a slash command that ran
    /// locally (Claude Code's `shouldQuery: false`). `input` joins the
    /// conversation as [`run_turn`](Self::run_turn)'s would (the session's
    /// system notes wait for the first request), and the model reads it
    /// with the next turn. `interrupted`: the command was stopped,
    /// noted as an interrupted request is.
    pub async fn record_turn(
        &mut self,
        input: Vec<ContentBlock>,
        interrupted: bool,
        deps: Deps<'_>,
    ) -> Result<TurnOutcome, TurnError> {
        self.push_input(input, deps).await?;
        if interrupted {
            return self.interrupted(INTERRUPTED, deps).await;
        }
        deps.events.emit(AgentEvent::TurnEnded { stop_reason: StopReason::EndTurn, stated: true }).await;
        Ok(TurnOutcome::Finished(StopReason::EndTurn))
    }
}
