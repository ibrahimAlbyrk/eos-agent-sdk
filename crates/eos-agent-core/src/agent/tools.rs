//! Running a response's tool calls: permission, concurrency, results.

use std::sync::Arc;

use eos_agent_types::ContentBlock;
use futures_util::{StreamExt, stream};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::{Agent, Deps, TOOL_INTERRUPTED, until_cancelled};
use crate::BoxFuture;
use crate::ports::{AgentEvent, Permission, ToolCall};
use crate::subagents::AgentOrigin;
use crate::tool::{ToolContext, ToolOutput};
use crate::workspace::Workspace;

impl Agent {
    /// Runs the message's tool calls: consecutive concurrency-safe calls
    /// together, everything else alone, results in call order. Returns the
    /// results, whether the user interrupted and whether a call ended the turn.
    pub(super) async fn run_tools(
        &self,
        content: &[ContentBlock],
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> (Vec<ContentBlock>, bool, bool) {
        let calls: Vec<(&str, &str, &RawValue)> = content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), &**input)),
                _ => None,
            })
            .collect();

        let mut results = Vec::with_capacity(calls.len());
        let mut beside = Vec::new();
        let mut ends_turn = false;
        let mut start = 0;
        while start < calls.len() {
            let safe = |&(_, name, input): &(&str, &str, &RawValue)| {
                self.toolbox.get(name).is_some_and(|t| t.is_concurrency_safe(input))
            };
            let end = if safe(&calls[start]) {
                start + calls[start..].iter().take_while(|c| safe(c)).count()
            } else {
                start + 1
            };
            // Boxed up front: a closure-built async block trips rustc's `Send`
            // inference for higher-ranked lifetimes when spawned on a runtime.
            let pending: Vec<BoxFuture<'_, ToolOutput>> = calls[start..end]
                .iter()
                .map(|&(id, name, input)| Box::pin(self.call_tool(id, name, input, deps, cancel)) as BoxFuture<'_, _>)
                .collect();
            let batch = stream::iter(pending).buffered(self.config.max_parallel_tools.max(1)).collect::<Vec<_>>().await;
            for ((id, _, _), output) in calls[start..end].iter().zip(batch) {
                let output = Arc::new(output);
                deps.events.emit(AgentEvent::ToolResult { id: (*id).to_owned(), output: Arc::clone(&output) }).await;
                let output = Arc::unwrap_or_clone(output);
                ends_turn |= output.ends_turn && !output.is_error;
                beside.extend(output.beside);
                results.push(ContentBlock::ToolResult {
                    tool_use_id: (*id).to_owned(),
                    content: Some(output.content),
                    is_error: output.is_error,
                });
            }
            start = end;
        }
        results.extend(beside);
        (results, cancel.is_cancelled(), ends_turn)
    }

    async fn call_tool(
        &self,
        id: &str,
        name: &str,
        input: &RawValue,
        deps: Deps<'_>,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        if cancel.is_cancelled() {
            return ToolOutput::error(TOOL_INTERRUPTED);
        }
        let Some(tool) = self.toolbox.get(name) else {
            return ToolOutput::error(format!("Error: No such tool available: {name}"));
        };
        let ctx = ToolContext {
            agent_id: &self.id,
            tool_use_id: id,
            cancel,
            extensions: &self.extensions,
            transcript: &self.transcript,
        };
        let review = tool.review(input, ctx).await;
        let workspace = self.extensions.get::<Workspace>();
        let origin = self.extensions.get::<AgentOrigin>();
        let observed = tool.observable_input(input, &self.extensions);
        let call = ToolCall {
            agent_id: &self.id,
            id,
            name,
            input: observed.as_deref().unwrap_or(input),
            sandboxed: tool.runs_sandboxed(input, &self.extensions),
            review: review.as_ref(),
            workspace: workspace.as_deref(),
            origin: origin.as_deref(),
        };
        let updated = match deps.permissions.decide(call).await {
            Permission::Allow => None,
            Permission::AllowWith(input) => Some(input),
            Permission::Deny(reason) => return ToolOutput::error(reason),
        };
        let input = updated.as_deref().unwrap_or(input);
        until_cancelled(cancel, tool.call(input, ctx)).await.unwrap_or_else(|| ToolOutput::error(TOOL_INTERRUPTED))
    }
}

/// Tool calls without results would make the next request invalid.
pub(super) fn without_unanswered_calls(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .filter(|b| match b {
            ContentBlock::ToolUse { .. } | ContentBlock::ServerToolUse { .. } => false,
            ContentBlock::Text { text, .. } => !text.is_empty(),
            ContentBlock::Thinking { signature, .. } => !signature.is_empty(),
            _ => true,
        })
        .collect()
}
