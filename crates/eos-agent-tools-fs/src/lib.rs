//! Claude Code's file tools — Read, Write, Edit, NotebookEdit, Glob and Grep —
//! with the same descriptions, schemas, limits and result wording.
//!
//! Every tool needs the calling agent's [`Workspace`](eos_agent_core::Workspace)
//! in its extensions; read/write safety lives in its
//! [`FileStates`](eos_agent_core::FileStates). [`ReadLimits`] tunes Read per agent.

mod common;
mod edit;
mod fsio;
mod glob;
mod grep;
mod notebook;
mod patch;
mod read;
mod restore;
mod spec;
mod state;
mod text;
mod walk;
mod write;

use std::sync::Arc;

use eos_agent_core::Tool;

pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use notebook::NotebookEditTool;
pub use read::{ReadLimits, ReadTool};
pub use write::WriteTool;

/// All six tools, in Claude Code's order.
pub fn tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool::new()),
        Arc::new(WriteTool::new()),
        Arc::new(EditTool::new()),
        Arc::new(GlobTool::new()),
        Arc::new(GrepTool::new()),
        Arc::new(NotebookEditTool::new()),
    ]
}
