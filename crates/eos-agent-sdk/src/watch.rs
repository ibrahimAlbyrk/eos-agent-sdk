//! Artifact watch notes reach agents through the registry: a version
//! published elsewhere becomes a note for the agent's next request, a comment
//! sent to Claude wakes it. Only top-level agents hold watches, as only
//! Claude Code's main conversation does. MCP step-up questions likewise
//! reach only top-level agents' users.

use std::sync::{Arc, Weak};

use eos_agent_artifacts::{ComposeRequest, Composed, WatchNotifier};
use eos_agent_core::{
    Assembler, BoxFuture, ModelClient, ModelError, ModelRequest, PermissionGate, SystemPrompt, ToolCall,
};
use eos_agent_mcp::{AuthPrompter, AuthRequest, StepUpAnswer, StepUpRequest};
use eos_agent_types::{ContentBlock, StopReason, StreamEvent};
use futures_util::StreamExt;

use crate::models;
use crate::registry::Registry;

pub(crate) struct AgentNotes {
    pub(crate) registry: Weak<Registry>,
    /// Writes Claude Code's automatic comment replies with the agent's model.
    pub(crate) client: Arc<dyn ModelClient>,
    /// Sees each automatic reply before it is posted, as an ArtifactComments call.
    pub(crate) gate: Arc<dyn PermissionGate>,
}

impl WatchNotifier for AgentNotes {
    fn notify<'a>(&'a self, agent_id: &'a str, text: String, wake: bool) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Some(handle) = self.registry.upgrade().and_then(|r| r.handle(agent_id)) else { return false };
            handle.notify(text, wake).await.is_ok()
        })
    }

    fn can_watch(&self, agent_id: &str) -> bool {
        self.registry.upgrade().is_some_and(|r| r.is_top_level(agent_id))
    }

    /// One tool-less request on the agent's model (or the small fast
    /// model), thinking off where the model allows it, as Claude Code's
    /// comment composers send it.
    fn compose<'a>(&'a self, agent_id: &'a str, request: &'a ComposeRequest) -> BoxFuture<'a, Option<Composed>> {
        Box::pin(async move {
            let agent_model = self.registry.upgrade()?.infos(Some(agent_id)).into_iter().next()?.model;
            let model = if request.small_model { small_fast_model() } else { agent_model };
            let (max_tokens, effort) = match request.effort {
                Some((level, tokens)) if models::effort_for(&model, Some(level)).is_some() => {
                    (tokens, Some(level.to_owned()))
                }
                _ => (request.max_tokens, None),
            };
            let blocks: Vec<serde_json::Value> = request
                .blocks
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    let mut block = serde_json::json!({ "type": "text", "text": text });
                    if i == 0 && request.cache_first {
                        block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                    }
                    block
                })
                .collect();
            // One block goes as plain text, as Claude Code sends a prompt without the page.
            let content = match blocks.as_slice() {
                [one] if one.get("cache_control").is_none() => one["text"].clone(),
                _ => serde_json::Value::Array(blocks),
            };
            let message = serde_json::json!({ "role": "user", "content": content });
            let off = models::thinking_off(&model, effort);
            let request = ModelRequest {
                model,
                max_tokens,
                system: SystemPrompt::new(&[(request.system.as_str(), None)]).json().clone(),
                tools: None,
                messages: vec![serde_json::to_vec(&message).ok()?.into()],
                thinking: off.thinking,
                effort: off.effort,
                extra: None,
                cache_key: None,
                betas: &[],
                auth: None,
            };
            let mut events = match self.client.stream(&request).await {
                Ok(events) => events,
                Err(ModelError::LimitReached { .. }) => return Some(Composed::UsageLimited),
                Err(_) => return Some(Composed::Failed),
            };
            let mut assembler = Assembler::default();
            while let Some(event) = events.next().await {
                match event {
                    Ok(StreamEvent::Error(_)) => return Some(Composed::Failed),
                    Ok(event) => {
                        assembler.push(event);
                    }
                    Err(ModelError::LimitReached { .. }) => return Some(Composed::UsageLimited),
                    Err(_) => return Some(Composed::Failed),
                }
            }
            let done = assembler.finish();
            if done.stop_reason == Some(StopReason::MaxTokens) {
                return Some(Composed::Truncated);
            }
            let text: String = done
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            Some(Composed::Text(text))
        })
    }

    fn output_cap(&self, agent_id: &str) -> Option<u32> {
        let model = self.registry.upgrade()?.infos(Some(agent_id)).into_iter().next()?.model;
        Some(models::limits(&model).0)
    }

    fn may_call<'a>(&'a self, agent_id: &'a str, tool: &'a str, input: &'a serde_json::Value) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Ok(raw) = serde_json::value::to_raw_value(input) else { return false };
            let call = ToolCall {
                agent_id,
                id: "artifact_auto_react",
                name: tool,
                input: &raw,
                sandboxed: false,
                review: None,
                workspace: None,
                origin: None,
            };
            self.gate.decide(call).await.is_allowed()
        })
    }
}

/// Claude Code's `qE`: `ANTHROPIC_SMALL_FAST_MODEL`, else Haiku.
fn small_fast_model() -> String {
    std::env::var("ANTHROPIC_SMALL_FAST_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| models::resolve_model("haiku"))
}

/// MCP sign-ins go to the host's prompter; Claude Code's step-up question
/// only for a top-level agent's call (a subagent's call fails and its server
/// waits for a sign-in).
pub(crate) struct MainAgentsAsk {
    pub(crate) inner: Arc<dyn AuthPrompter>,
    pub(crate) registry: Weak<Registry>,
}

impl AuthPrompter for MainAgentsAsk {
    fn authorize(&self, request: &AuthRequest) {
        self.inner.authorize(request);
    }

    fn ask_step_up<'a>(&'a self, request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        let top =
            request.agent_id.as_deref().is_some_and(|id| self.registry.upgrade().is_some_and(|r| r.is_top_level(id)));
        if top { self.inner.ask_step_up(request) } else { Box::pin(async { StepUpAnswer::Unanswered }) }
    }
}
