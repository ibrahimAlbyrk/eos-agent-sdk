//! One MCP tool as an agent [`Tool`].

use std::path::PathBuf;
use std::sync::{Arc, Weak};

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec, Workspace};
use serde_json::value::RawValue;
use serde_json::{Map, Value};

use crate::content::{self, MAX_RESULT_SIZE_CHARS_CEILING};
use crate::server::Server;

/// Claude Code cuts longer tool descriptions.
const MAX_DESCRIPTION_CHARS: usize = 2048;

/// `mcp__<server>__<tool>`, each part with characters outside
/// `[A-Za-z0-9_-]` replaced by `_` (Claude Code's normalization).
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", normalize(server), normalize(tool))
}

pub(crate) fn normalize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

pub(crate) struct McpTool {
    spec: ToolSpec,
    server: Weak<Server>,
    /// The name the server knows the tool by.
    remote_name: Arc<str>,
    read_only: bool,
    max_tokens: usize,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool").field("name", &self.spec.name()).finish()
    }
}

impl McpTool {
    /// `defer_by_default` is the server's own setting (off with `alwaysLoad`).
    pub(crate) fn build(
        server: &Server,
        weak: Weak<Server>,
        tool: &rmcp::model::Tool,
        max_output_tokens: usize,
    ) -> Arc<dyn Tool> {
        let name = tool_name(&server.config.name, &tool.name);
        let description = truncate_description(tool.description.as_deref().unwrap_or_default());
        let schema = Value::Object((*tool.input_schema).clone());
        let meta = tool.meta.as_ref().map(|m| &m.0);
        let meta_str = |key: &str| meta.and_then(|m| m.get(key)).and_then(Value::as_str);
        let meta_bool = |key: &str| meta.and_then(|m| m.get(key)).and_then(Value::as_bool);

        let mut spec = ToolSpec::new(&name, &description, schema);
        let always_load = meta_bool("anthropic/alwaysLoad").unwrap_or(server.config.always_load);
        if !always_load {
            spec = spec.deferred();
        }
        if let Some(hint) = meta_str("anthropic/searchHint") {
            spec = spec.with_search_hint(hint);
        }
        let max_tokens = meta
            .and_then(|m| m.get("anthropic/maxResultSizeChars"))
            .and_then(Value::as_u64)
            .filter(|chars| *chars > 0)
            .map_or(max_output_tokens, |chars| {
                usize::try_from(chars).unwrap_or(usize::MAX).min(MAX_RESULT_SIZE_CHARS_CEILING) / 4
            });
        let read_only = tool.annotations.as_ref().and_then(|a| a.read_only_hint).unwrap_or(false);
        Arc::new(Self { spec, server: weak, remote_name: Arc::from(tool.name.as_ref()), read_only, max_tokens })
    }
}

fn truncate_description(text: &str) -> String {
    match text.char_indices().nth(MAX_DESCRIPTION_CHARS) {
        Some((end, _)) => format!("{}… [truncated]", &text[..end]),
        None => text.to_owned(),
    }
}

/// Where binary results are written: the agent's scratch space when it has a
/// [`Workspace`], else the system temp dir.
pub(crate) fn blob_dir(ctx: &ToolContext<'_>) -> PathBuf {
    ctx.extensions
        .get::<Workspace>()
        .map_or_else(|| std::env::temp_dir().join("eos-mcp"), |ws| ws.scratch().to_path_buf())
}

impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn is_concurrency_safe(&self, _input: &RawValue) -> bool {
        self.read_only
    }

    fn call<'a>(&'a self, input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(server) = self.server.upgrade() else {
                return ToolOutput::error("This MCP server is no longer connected.");
            };
            let arguments = match serde_json::from_str::<Value>(input.get()) {
                Ok(Value::Object(map)) => map,
                Ok(Value::Null) => Map::new(),
                Ok(_) => return ToolOutput::error("The tool input must be a JSON object."),
                Err(e) => return ToolOutput::error(format!("The tool input is not valid JSON: {e}")),
            };
            match server.call_tool(&self.remote_name, arguments, ctx.cancel, ctx.agent_id).await {
                Ok(result) => {
                    content::tool_output(&server.config.name, &result, self.max_tokens, &blob_dir(&ctx)).await
                }
                Err(message) => ToolOutput::error(message),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_normalized() {
        assert_eq!(tool_name("my server", "get.thing"), "mcp__my_server__get_thing");
        assert_eq!(tool_name("a-b_c", "x"), "mcp__a-b_c__x");
    }

    #[test]
    fn long_descriptions_are_cut() {
        let long = "é".repeat(MAX_DESCRIPTION_CHARS + 5);
        let cut = truncate_description(&long);
        assert!(cut.ends_with("… [truncated]"));
        assert_eq!(cut.chars().count(), MAX_DESCRIPTION_CHARS + "… [truncated]".chars().count());
    }
}
