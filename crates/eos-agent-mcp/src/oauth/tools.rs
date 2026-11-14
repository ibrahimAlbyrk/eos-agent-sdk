//! What an agent sees of a server that needs a sign-in (Claude Code's auth
//! stubs): `mcp__<server>__authenticate` starts the sign-in and returns the
//! address for the user; `mcp__<server>__complete_authentication` takes the
//! callback address when the browser could not deliver it.

use std::sync::{Arc, Weak};

use eos_agent_core::{BoxFuture, Tool, ToolContext, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::config::McpTransport;
use crate::server::Server;
use crate::tool::tool_name;

const SCHEMA_URL: &str = "https://json-schema.org/draft/2020-12/schema";
const AUTHENTICATE: &str = "authenticate";
const COMPLETE: &str = "complete_authentication";

/// The two tools standing in for a server's own until it is signed in to.
pub(crate) fn auth_tools(server: &Server, weak: &Weak<Server>) -> Vec<Arc<dyn Tool>> {
    let name = &server.config.name;
    let (kind, url) = match &server.config.transport {
        McpTransport::Http { url, .. } => ("http", Some(url)),
        McpTransport::Sse { url, .. } => ("sse", Some(url)),
        McpTransport::Stdio { .. } => ("stdio", None),
    };
    let origin = url.and_then(|u| reqwest::Url::parse(u).ok()).map(|u| u.origin().ascii_serialization());
    let reached = match origin {
        Some(o) if o != kind => format!("{kind} at {}", o.chars().take(256).collect::<String>()),
        _ => kind.to_owned(),
    };
    let start = tool_name(name, AUTHENTICATE);
    let start_text = format!(
        "The \"{name}\" MCP server ({reached}) is installed but requires authentication. Call this tool to start the \
         OAuth flow — you'll receive an authorization URL to share with the user. Once the user completes \
         authorization in their browser, the server's real tools will become available automatically."
    );
    let complete_text = format!(
        "Complete an in-progress OAuth flow for the \"{name}\" MCP server by submitting the callback URL. Call \
         `{start}` first to start the flow and get the authorization URL. After the user authorizes in their \
         browser, the browser is redirected to a `http://localhost:<port>/callback?code=...&state=...` URL — on \
         remote sessions that page fails to load, but the URL in the address bar is still valid. Pass that full URL \
         here as `callback_url`."
    );
    let defer = |spec: ToolSpec| if server.config.always_load { spec } else { spec.deferred() };
    let start_spec = ToolSpec::new(
        &start,
        &start_text,
        json!({"$schema": SCHEMA_URL, "additionalProperties": false, "properties": {}, "type": "object"}),
    );
    let complete_spec = ToolSpec::new(
        &tool_name(name, COMPLETE),
        &complete_text,
        json!({
            "$schema": SCHEMA_URL,
            "additionalProperties": false,
            "properties": {"callback_url": {
                "description": "The full callback URL from the browser address bar after authorizing, e.g. \
                                http://localhost:<port>/callback?code=...&state=...",
                "type": "string",
            }},
            "required": ["callback_url"],
            "type": "object",
        }),
    );
    vec![
        Arc::new(Authenticate { spec: defer(start_spec), server: weak.clone() }),
        Arc::new(Complete { spec: defer(complete_spec), server: weak.clone(), start }),
    ]
}

struct Authenticate {
    spec: ToolSpec,
    server: Weak<Server>,
}

struct Complete {
    spec: ToolSpec,
    server: Weak<Server>,
    /// The authenticate tool's name, for the messages.
    start: String,
}

impl std::fmt::Debug for Authenticate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.spec.name())
    }
}

impl std::fmt::Debug for Complete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.spec.name())
    }
}

const GONE: &str = "This MCP server is no longer connected.";

impl Tool for Authenticate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, _input: &'a RawValue, ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(server) = self.server.upgrade() else { return ToolOutput::error(GONE) };
            let name = server.config.name.clone();
            if let McpTransport::Sse { .. } = server.config.transport {
                return ToolOutput::text(format!(
                    "Server \"{name}\" uses sse transport which does not support OAuth from this tool. Ask the user to \
                     run /mcp and authenticate manually."
                ));
            }
            let Some((flow, reused)) = server.start_sign_in(Some(ctx.agent_id.to_owned()), false) else {
                return ToolOutput::text(format!(
                    "Server \"{name}\" does not use OAuth. Ask the user to run /mcp and authenticate manually."
                ));
            };
            let preface = if reused {
                format!(
                    "A sign-in for {name} that started earlier is still in progress, so the link below is that \
                     sign-in's link, not a new one. If the user says the sign-in page showed an error before they \
                     could approve access, ask them to run /mcp and sign in there instead.\n\n"
                )
            } else {
                String::new()
            };
            let complete = tool_name(&name, COMPLETE);
            ToolOutput::text(match flow.auth_url().await {
                Ok(Some(url)) => format!(
                    "{preface}Ask the user to open this URL in their browser to authorize the {name} MCP server:\n\n\
                     {url}\n\nOnce they complete the flow, the server's tools will become available automatically.\n\n\
                     If the browser shows a connection error on the redirect page, ask the user to paste the full URL \
                     from the address bar and call `{complete}` with it."
                ),
                Ok(None) => {
                    format!("Authentication completed silently for {name}. The server's tools should now be available.")
                }
                Err(e) => format!(
                    "Failed to start OAuth flow for {name}: {e}. Ask the user to run /mcp and authenticate manually."
                ),
            })
        })
    }
}

#[derive(Deserialize)]
struct CompleteInput {
    callback_url: String,
}

impl Tool for Complete {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, input: &'a RawValue, _ctx: ToolContext<'a>) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(server) = self.server.upgrade() else { return ToolOutput::error(GONE) };
            let name = server.config.name.clone();
            let input: CompleteInput = match serde_json::from_str(input.get()) {
                Ok(i) => i,
                Err(e) => return ToolOutput::error(format!("Invalid input: {e}")),
            };
            let start = &self.start;
            let Some(flow) = server.active_sign_in() else {
                return ToolOutput::text(format!(
                    "No OAuth flow is in progress for {name}. Call `{start}` first, then retry with the callback URL."
                ));
            };
            let has_code = reqwest::Url::parse(&input.callback_url)
                .is_ok_and(|u| u.query_pairs().any(|(k, _)| k == "code" || k == "error"));
            if !has_code {
                return ToolOutput::text(
                    "Invalid callback URL: missing authorization code. Ask the user to paste the full redirect URL \
                     from their browser's address bar, including the `?code=...&state=...` query string.",
                );
            }
            if !flow.submit(&input.callback_url) {
                return ToolOutput::text(format!(
                    "That callback URL belongs to a different sign-in attempt for {name} (its state does not match \
                     the flow in progress), or carries no authorization code. The current flow is still waiting: ask \
                     the user for the URL from the page this sign-in opened, then retry."
                ));
            }
            ToolOutput::text(match flow.wait().await {
                Ok(()) => {
                    server.signed_in().await;
                    format!("Authentication complete for {name}. The server's tools should now be available.")
                }
                Err(e) if e == "Authentication was cancelled" => format!(
                    "The OAuth flow for {name} was cancelled (a newer attempt may have superseded it). Call `{start}` \
                     again to restart."
                ),
                Err(e) => format!("Authentication failed for {name}: {e}"),
            })
        })
    }
}
