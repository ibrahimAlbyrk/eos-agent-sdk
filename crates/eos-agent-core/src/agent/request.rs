//! Building a request from the conversation and streaming it, retried on
//! transient failures.

use bytes::Bytes;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use eos_agent_types::{ContentBlock, Role};

use super::inline::{Declared, ToolChanges};
use super::{Agent, Deps, Streamed, TurnError, until_cancelled};
use crate::assembler::Assembler;
use crate::message::StoredMessage;
use crate::model::{ModelError, ModelRequest};
use crate::ports::AgentEvent;

const TOOL_SEARCH: &str = "ToolSearch";
/// Claude Code's `DeferredToolPlaceholder` (its `tengu_deferred_stub_tool`,
/// on by default): a deferred entry that keeps deferred loading on while no
/// other deferred tool is declared.
const PLACEHOLDER: &str = "DeferredToolPlaceholder";
const PLACEHOLDER_JSON: &str = r#"{"name":"DeferredToolPlaceholder","description":"Reserved placeholder that keeps deferred tool loading active; never call this tool.","input_schema":{"type":"object","properties":{}},"defer_loading":true}"#;

impl Agent {
    /// The request the agent would send now: its system prompt, tools and
    /// conversation as the model would get them.
    pub fn build_request(&self) -> ModelRequest {
        // A turn on a model without system turns (a command's model) gets the
        // tools in the array, the notes folded into user text.
        let inline = self.config.system_turns;
        let rewritten;
        let history: &[StoredMessage] = if !inline && self.transcript.iter().any(|m| m.role() == Role::System) {
            rewritten = fold_system_turns(&self.transcript);
            &rewritten
        } else if let Some(wire) = inline.then(|| self.announced.wire_history(&self.transcript)).flatten() {
            rewritten = wire;
            &rewritten
        } else {
            &self.transcript
        };
        let placed;
        let history = match system_turns_before_replies(history) {
            Some(fixed) => {
                placed = fixed;
                &placed
            }
            None => history,
        };
        let mut messages: Vec<Bytes> = history.iter().map(|m| m.json().clone()).collect();
        if let (Some(ttl), Some(last), Some(slot)) = (self.config.cache_ttl, history.last(), messages.last_mut()) {
            *slot = last.with_cache_marker(ttl);
        }
        ModelRequest {
            model: self.config.model.clone(),
            max_tokens: self.config.max_tokens,
            system: self.system.json().clone(),
            tools: self.wire_tools(inline),
            messages,
            thinking: self.config.thinking,
            effort: self.config.effort.clone(),
            extra: self.config.extra.clone(),
            cache_key: self.config.cache_key.clone(),
            betas: if inline { self.announced.tool_changes().betas() } else { &[] },
            auth: self.config.auth.clone(),
        }
    }

    /// The `tools` array as Claude Code declares it: with ToolSearch, a
    /// deferred tool only once ToolSearch loaded it (kept after its source
    /// goes away) and the placeholder that keeps deferred loading on; with
    /// the tool-change betas, without the tools the conversation defines.
    fn wire_tools(&self, tool_changes: bool) -> Option<Bytes> {
        let mode = if tool_changes { self.announced.tool_changes() } else { ToolChanges::InTools };
        let tool_search = self.toolbox.get(TOOL_SEARCH).is_some();
        let placeholder = (tool_search
            || (mode != ToolChanges::InTools && self.config.tool_source.is_some() && self.toolbox.json().is_some()))
            && self.toolbox.get(PLACEHOLDER).is_none();
        let declared = self.announced.declared(&self.toolbox);
        let shown = |d: &&Declared<'_>| !(tool_search && d.deferred && !self.loaded.contains(d.name));
        let mut parts: Vec<&[u8]> = declared.iter().filter(shown).map(|d| &d.json[..]).collect();
        let mut same = parts.len() == self.toolbox.tools().len() && mode == ToolChanges::InTools && !placeholder;
        if tool_search {
            for (name, json) in &self.kept {
                if self.toolbox.get(name).is_none() && declared.iter().all(|d| d.name != name.as_str()) {
                    parts.push(json);
                    same = false;
                }
            }
        }
        if placeholder {
            parts.insert(parts.len().saturating_sub(1), PLACEHOLDER_JSON.as_bytes());
        }
        if parts.is_empty() {
            return None;
        }
        if same {
            return self.toolbox.json().cloned();
        }
        let mut json = Vec::with_capacity(parts.iter().map(|p| p.len() + 1).sum::<usize>() + 1);
        for (i, part) in parts.iter().enumerate() {
            json.push(if i == 0 { b'[' } else { b',' });
            json.extend_from_slice(part);
        }
        json.push(b']');
        Some(Bytes::from(json))
    }

    /// One model call, retried on transient failures until it streams to the end.
    pub(super) async fn request(
        &self,
        request: ModelRequest,
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> Result<Streamed, TurnError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            deps.events.emit(AgentEvent::RequestStarted { attempt }).await;
            let mut assembler = Assembler::default();
            let mut emitted = false;
            let error = match until_cancelled(cancel, deps.model.stream(&request)).await {
                None => return Ok(Streamed::Cancelled(assembler.finish())),
                Some(Err(e)) => e,
                Some(Ok(mut events)) => loop {
                    let Some(next) = until_cancelled(cancel, events.next()).await else {
                        return Ok(Streamed::Cancelled(assembler.finish()));
                    };
                    match next {
                        Some(Ok(event)) => {
                            for mut out in assembler.push(event).into_iter().flatten() {
                                match &mut out {
                                    AgentEvent::Usage(u) if u.model.is_empty() => u.model.clone_from(&request.model),
                                    AgentEvent::Usage(_) => {}
                                    _ => emitted = true,
                                }
                                deps.events.emit(out).await;
                            }
                        }
                        Some(Err(e)) => break e,
                        None => {
                            let assembled = assembler.finish();
                            if assembled.complete {
                                return Ok(Streamed::Done(assembled));
                            }
                            break ModelError::Transient("stream ended before message_stop".into());
                        }
                    }
                },
            };

            if let ModelError::LimitReached { info } = &error {
                deps.events.emit(AgentEvent::RateLimit(Box::new(info.clone()))).await;
            }
            if !error.is_retryable() || attempt > self.config.retry.max_retries {
                return Err(error.into());
            }
            if emitted {
                deps.events.emit(AgentEvent::StreamRestarted).await;
            }
            let delay = self.config.retry.delay(attempt, error.retry_after());
            deps.events.emit(AgentEvent::Retrying { attempt, delay, error: error.to_string() }).await;
            if until_cancelled(cancel, deps.clock.sleep(delay)).await.is_none() {
                return Ok(Streamed::Cancelled(Assembler::default().finish()));
            }
        }
    }
}

/// The history as Claude Code sends it to a model that takes no
/// mid-conversation `role: "system"` turns (Claude 3 and the Claude 4 models
/// before Opus 4.8): each system note becomes a `<system-reminder>` block at
/// the end of the user message before it, or a user message of its own.
/// The API takes a mid-conversation system turn only right before the
/// model's reply, or last. Between two replies the conversation keeps its
/// order, with the system turns moved after the user's messages (a history
/// recorded otherwise, as an older engine could, still goes out valid).
/// `None` when they already are.
fn system_turns_before_replies(history: &[StoredMessage]) -> Option<Vec<StoredMessage>> {
    if !history.windows(2).any(|w| w[0].role() == Role::System && w[1].role() == Role::User) {
        return None;
    }
    let mut out = Vec::with_capacity(history.len());
    let mut held = Vec::new();
    for message in history {
        match message.role() {
            Role::System => held.push(message.clone()),
            Role::User => out.push(message.clone()),
            Role::Assistant => {
                out.append(&mut held);
                out.push(message.clone());
            }
        }
    }
    out.append(&mut held);
    Some(out)
}

fn fold_system_turns(history: &[StoredMessage]) -> Vec<StoredMessage> {
    let mut out: Vec<StoredMessage> = Vec::with_capacity(history.len());
    for message in history {
        if message.role() != Role::System {
            out.push(message.clone());
            continue;
        }
        let reminder = ContentBlock::Text {
            text: format!("<system-reminder>\n{}\n</system-reminder>", message.text()),
            citations: None,
        };
        let merged = out.last().filter(|m| m.role() == Role::User).and_then(|m| {
            let mut blocks = m.blocks().ok()?;
            blocks.push(reminder.clone());
            StoredMessage::new(Role::User, &blocks).ok()
        });
        match merged {
            Some(merged) => *out.last_mut().expect("a user message") = merged,
            None => out.extend(StoredMessage::new(Role::User, std::slice::from_ref(&reminder)).ok()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_turns_fold_into_user_messages() {
        let history = vec![
            StoredMessage::user_text("hi"),
            StoredMessage::system_text("env"),
            StoredMessage::new(Role::Assistant, &[ContentBlock::Text { text: "ok".into(), citations: None }])
                .expect("message"),
            StoredMessage::system_text("later"),
        ];
        let folded = fold_system_turns(&history);
        let json: Vec<serde_json::Value> =
            folded.iter().map(|m| serde_json::from_slice(m.json()).expect("json")).collect();
        assert_eq!(folded.len(), 3);
        assert_eq!(json[0]["content"][1]["text"], "<system-reminder>\nenv\n</system-reminder>");
        assert_eq!(json[0]["content"][0]["text"], "hi");
        assert_eq!(json[2]["role"], "user");
        assert_eq!(json[2]["content"][0]["text"], "<system-reminder>\nlater\n</system-reminder>");
    }
}
