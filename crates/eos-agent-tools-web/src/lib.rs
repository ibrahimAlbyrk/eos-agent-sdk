//! Claude Code's web tools.
//!
//! - [`WebSearchTool`]: one side call through the injected [`ModelClient`]
//!   that forces the API's `web_search_20250305` server tool, then returns the
//!   commentary and links in Claude Code's layout. A per-session budget
//!   ([`SessionSearchBudget`]) caps searches across all agents of a session.
//! - [`WebFetchTool`]: its own HTTP fetching (same-site redirects only, size
//!   and time caps), HTML→markdown, a byte-bounded page cache shared by every
//!   agent, and a small-model answer to the caller's prompt.
//!
//! [`ModelClient`]: eos_agent_core::ModelClient

mod fetch;
mod search;
mod side;

pub use fetch::{
    DEFAULT_CACHE_BYTES, DEFAULT_CACHE_TTL, DEFAULT_FETCH_MODEL, FetchCache, WEB_FETCH, WebFetchConfig, WebFetchTool,
};
pub use search::{
    DEFAULT_SEARCH_BUDGET, SearchBudget, SessionSearchBudget, WEB_SEARCH, WebSearchConfig, WebSearchTool,
};
pub use side::SideModel;

/// The identity line Claude Code (as the Agent SDK) puts first in every
/// request's system prompt, side calls included.
pub const SDK_IDENTITY: &str = "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
