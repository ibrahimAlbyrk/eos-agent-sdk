//! MCP client: servers from Claude Code's `mcpServers` config, exposed to
//! agents as tools.
//!
//! One [`McpHub`] per process owns every connection. An agent asks it for an
//! [`McpToolset`]; agents with equal server configs share one instance (one
//! child process for stdio), which ends with the last toolset using it.
//!
//! Remote servers that answer 401 are signed in to with OAuth the way Claude
//! Code does it, sharing its stored sign-ins; until then their tools are
//! `mcp__<server>__authenticate` and `mcp__<server>__complete_authentication`
//! (see [`McpHub::with_oauth`]).
//!
//! ```ignore
//! let hub = McpHub::new(McpLimits::default());
//! let config = McpConfig::from_json(&serde_json::json!({"mcpServers": {...}}));
//! let mcp = hub.toolset(&config.servers).await;      // keep alive with the agent; connects in the background
//! let mut tools = builtin_tools;
//! tools.extend(mcp.tools());
//! let toolbox = Toolbox::new(tools);
//! ```

mod config;
mod content;
mod hub;
mod oauth;
mod process;
mod quote;
mod resources;
mod server;
mod sse;
mod tool;

pub use config::{ConfigError, KeyHints, McpConfig, McpScope, McpServerConfig, McpTransport, OAuthConfig};
pub use hub::{McpFailure, McpHub, McpInstructions, McpLimits, McpServerState, McpServerStatus, McpToolset};
pub use oauth::{
    AuthPrompter, AuthRequest, BrowserOpener, McpCredentials, MemorySecrets, OAuthOptions, PrintPrompter,
    STEP_UP_CHOICES, StepUpAnswer, StepUpChoice, StepUpRequest, XaaIdp, step_up_browser_notice,
};
pub use quote::{QUOTE_LIMIT, quoted};
pub use tool::tool_name;
