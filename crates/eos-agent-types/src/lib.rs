//! Wire and domain types for the Anthropic Messages API as the engine uses them.
//!
//! Anything the API may add later must survive a round trip: unknown content
//! blocks keep their raw JSON, unknown stop reasons and deltas keep their name.

mod content;
mod stream;
mod usage;

pub use content::{ContentBlock, Role, ToolResultContent};
pub use stream::{ApiError, Delta, MessageStart, StopReason, StreamEvent, StreamParseError};
pub use usage::{CacheCreation, ServerToolUse, Usage};
