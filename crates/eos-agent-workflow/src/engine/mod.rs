//! The script engine behind a workflow run, behind a small interface so it
//! can be swapped: a [`ScriptEngine`] checks syntax and opens [`Realm`]s; a
//! realm runs one script body and talks to the run only through a [`Host`]
//! (calls out, with JSON payloads) and its own methods (answers in). The
//! hooks themselves (`agent`, `parallel`, …) are plain JavaScript in
//! `prelude.js`, so another engine only has to provide the same two-way
//! string channel.

mod quickjs;
mod rewrite;
mod slash;

use std::sync::Arc;
use std::time::Duration;

pub use quickjs::QuickJs;

/// Claude Code's limit for one synchronous stretch of script (node:vm's
/// `timeout`): awaiting an agent or a timer does not count.
pub const SLICE_LIMIT: Duration = Duration::from_secs(30);

/// A host call the script made that failed; thrown in the script as an
/// `Error` (or `TypeError`) with this message and, when set, this name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub message: String,
    pub name: Option<String>,
    pub type_error: bool,
}

impl HostError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), name: None, type_error: false }
    }

    pub fn named(name: &str, message: impl Into<String>) -> Self {
        Self { message: message.into(), name: Some(name.to_owned()), type_error: false }
    }

    pub fn type_error(message: impl Into<String>) -> Self {
        Self { message: message.into(), name: None, type_error: true }
    }
}

/// What a realm calls while its script runs. `op` names the hook, `payload`
/// is JSON (empty for none); the answer is a JSON value.
pub trait Host: Send + Sync {
    fn call(&self, realm: u32, op: &str, payload: &str) -> Result<String, HostError>;
}

/// One realm to open: its body (the script after `meta`), the JSON
/// configuration the prelude reads, and its id in host calls.
#[derive(Debug, Clone)]
pub struct RealmSetup {
    pub id: u32,
    pub body: String,
    /// `{"total": number|null, "args": "<json>"?, "child": {"name": …}?}`.
    pub config: String,
    pub slice_limit: Duration,
}

/// Why a realm stopped running script for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fatal {
    /// A synchronous stretch ran past the slice limit.
    TimedOut(Duration),
    /// The engine itself failed (out of memory, an internal error).
    Engine(String),
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // node:vm's wording, which Claude Code passes on.
            Self::TimedOut(limit) => write!(f, "Script execution timed out after {}ms", limit.as_millis()),
            Self::Engine(message) => f.write_str(message),
        }
    }
}

/// A running script, owned by the thread that opened it. Every method runs the script's pending jobs before it
/// returns; the script reports its end through the host (`done` / `fail`).
pub trait Realm {
    /// Runs the body up to its first wait.
    fn start(&mut self) -> Result<(), Fatal>;

    /// Resolves (`ok`) or rejects host call `call` with a JSON value
    /// (`{"name", "message"}` when rejecting).
    fn settle(&mut self, call: u64, ok: bool, json: &str) -> Result<(), Fatal>;

    fn fire_timer(&mut self, id: u64) -> Result<(), Fatal>;

    /// From now on hooks never settle and timers never fire (a stopped run).
    fn abort(&mut self);

    /// Bytes the engine holds for this realm.
    fn memory_bytes(&self) -> usize;
}

/// Opens realms.
pub trait ScriptEngine: Send + Sync + std::fmt::Debug {
    /// `Err("SyntaxError: …")` when the body does not compile.
    fn check_syntax(&self, body: &str) -> Result<(), SyntaxError>;

    fn realm(&self, setup: RealmSetup, host: Arc<dyn Host>) -> Result<Box<dyn Realm>, Fatal>;

    /// Claude Code's regex-or-division check of a body that compiles: why
    /// one of its `/` would read differently to the engine than to the
    /// parser, if one would.
    fn ambiguous_slash(&self, body: &str) -> Option<String> {
        let _ = body;
        None
    }
}

/// A body that does not compile: the engine's message and, when it knows,
/// the 1-based line and column in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

impl std::fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SyntaxError: {}", self.message)
    }
}
