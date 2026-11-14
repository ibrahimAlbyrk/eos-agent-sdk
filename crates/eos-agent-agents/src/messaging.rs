//! SendMessage and ListAgents: talking to agents the caller started.

use std::sync::Arc;

use eos_agent_core::{BoxFuture, SubagentInfo, Subagents, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// The address of the main conversation.
const MAIN: &str = "main";

/// The answer to a message that is not plain text.
const PLAIN_TEXT_ONLY: &str = "Not sent. In this session SendMessage carries only plain text, and only to agents of \
                               this session: an agent launched in it with the Agent tool or, from a subagent, \
                               \"main\". Set `to` to such an agent's name, to the agentId from its spawn result, or \
                               to \"main\".";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendInput {
    to: String,
    /// Text; anything else is refused.
    #[serde(default)]
    message: serde_json::Value,
    #[serde(rename = "summary")]
    _summary: Option<String>,
    /// Cross-session subscriptions; there are no other sessions here.
    #[serde(rename = "notify_when_idle")]
    _notify_when_idle: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SendResult<'a> {
    success: bool,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resumed_agent_id: Option<&'a str>,
}

impl SendResult<'_> {
    fn output(&self) -> ToolOutput {
        // A refused send is an ordinary result (`success: false`), not a tool error.
        ToolOutput::text(serde_json::to_string(self).unwrap_or_default())
    }
}

fn failure(message: impl Into<String>) -> ToolOutput {
    SendResult { success: false, message: message.into(), resumed_agent_id: None }.output()
}

pub struct SendMessageTool {
    spec: ToolSpec,
    subagents: Arc<dyn Subagents>,
}

impl std::fmt::Debug for SendMessageTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendMessageTool").finish_non_exhaustive()
    }
}

impl SendMessageTool {
    /// Deferred, as in Claude Code: the model loads it through tool search.
    pub fn new(subagents: Arc<dyn Subagents>) -> Self {
        let spec = crate::spec(
            "SendMessage",
            include_str!("../prompts/SendMessage.md"),
            include_str!("../prompts/SendMessage.schema.json"),
        )
        .deferred()
        .with_search_hint("send a message to another agent of this session");
        Self { spec, subagents }
    }

    async fn run(&self, input: SendInput, ctx: ToolContext<'_>) -> ToolOutput {
        let message = match input.message {
            serde_json::Value::String(text) => text,
            serde_json::Value::Null => String::new(),
            _ => return failure(PLAIN_TEXT_ONLY),
        };
        let mut to = input.to.trim();
        let agents = if to == MAIN { Vec::new() } else { self.subagents.list(ctx.agent_id).await };
        // `name [ref]`, as ListAgents prints a named agent, addresses it by id.
        let by_ref = crate::listing::split_ref(to).and_then(|(name, r)| crate::listing::by_ref(&agents, name, r));
        if let Some(agent) = by_ref {
            to = &agent.agent_id;
        }
        let target = if to == MAIN { None } else { addressed(&agents, to) };
        if let Err(e) = self.subagents.send(ctx.agent_id, to, message).await {
            return failure(e);
        }
        let result = match &target {
            _ if to == MAIN => queued("Message queued for the main conversation's next turn.".to_owned()),
            Some(agent) if !agent.running => SendResult {
                success: true,
                message: format!("Resuming agent {}", agent.name.as_deref().unwrap_or(&agent.agent_id)),
                resumed_agent_id: Some(&agent.agent_id),
            },
            _ => queued(format!(
                "Message queued for delivery to {} at its next tool round. If it finishes first, it is resumed with \
                 the message and notifies again, so wait for that notification before resending.",
                target.as_ref().and_then(|a| a.name.as_deref()).unwrap_or(to)
            )),
        };
        result.output()
    }
}

fn queued(message: String) -> SendResult<'static> {
    SendResult { success: true, message, resumed_agent_id: None }
}

/// The agent `to` names: by id, else the latest one with that name.
fn addressed(agents: &[SubagentInfo], to: &str) -> Option<SubagentInfo> {
    agents
        .iter()
        .find(|a| a.agent_id == to)
        .or_else(|| agents.iter().rev().find(|a| a.name.as_deref() == Some(to)))
        .cloned()
}

impl Tool for SendMessageTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match serde_json::from_str::<SendInput>(input.get()) {
                Ok(input) => self.run(input, ctx).await,
                Err(e) => crate::invalid_input(&e),
            }
        })
    }
}

pub struct ListAgentsTool {
    spec: ToolSpec,
    subagents: Arc<dyn Subagents>,
}

impl std::fmt::Debug for ListAgentsTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListAgentsTool").finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListInput {
    /// "Not available in this build" in Claude Code as well.
    #[serde(rename = "channel")]
    _channel: Option<String>,
    #[serde(rename = "q")]
    _q: Option<String>,
}

impl ListAgentsTool {
    pub fn new(subagents: Arc<dyn Subagents>) -> Self {
        let spec = crate::spec(
            "ListAgents",
            include_str!("../prompts/ListAgents.md"),
            include_str!("../prompts/ListAgents.schema.json"),
        )
        .with_search_hint("list agents you can SendMessage to");
        Self { spec, subagents }
    }
}

impl Tool for ListAgentsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        true
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            if let Err(e) = serde_json::from_str::<ListInput>(input.get()) {
                return crate::invalid_input(&e);
            }
            let agents = self.subagents.list(ctx.agent_id).await;
            let session = self.subagents.session_of(ctx.agent_id);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
            ToolOutput::text(crate::listing::listing(session.as_deref(), &agents, now))
        })
    }
}
