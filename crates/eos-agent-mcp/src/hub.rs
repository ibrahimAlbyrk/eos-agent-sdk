//! The process-wide registry of server connections, and the per-agent view
//! of them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use eos_agent_core::Tool;
use tokio::sync::Semaphore;

use crate::config::{McpServerConfig, McpTransport};
use crate::oauth::{AuthContext, AuthPrompter, McpCredentials, OAuthOptions, PrintPrompter};
use crate::resources::{ListResources, ReadResource};
use crate::server::Server;

/// Claude Code's numbers unless noted.
#[derive(Debug, Clone)]
pub struct McpLimits {
    /// Starting a server through listing its tools (`MCP_TIMEOUT`).
    pub connect_timeout: Duration,
    /// Stdio servers starting at once (`MCP_SERVER_CONNECTION_BATCH_SIZE`).
    pub stdio_connects: usize,
    /// Remote servers connecting at once.
    pub remote_connects: usize,
    /// A connection nobody used for this long is closed; the next call
    /// reconnects. Claude Code uses the same numbers as a per-call no-progress
    /// timeout; here they free idle processes and sockets.
    pub stdio_idle_timeout: Duration,
    pub remote_idle_timeout: Duration,
    /// Cap on one tool result (`MAX_MCP_OUTPUT_TOKENS`, chars/4 estimate).
    pub max_output_tokens: usize,
}

impl Default for McpLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            stdio_connects: 3,
            remote_connects: 20,
            stdio_idle_timeout: Duration::from_secs(30 * 60),
            remote_idle_timeout: Duration::from_secs(5 * 60),
            max_output_tokens: 25_000,
        }
    }
}

/// Owns every MCP connection of the process. Agents whose configs are equal
/// share one server instance (one child process for stdio); the instance
/// ends when the last [`McpToolset`] holding it is dropped.
pub struct McpHub {
    pub(crate) limits: McpLimits,
    pub(crate) stdio_slots: Semaphore,
    pub(crate) remote_slots: Semaphore,
    pub(crate) auth: Arc<AuthContext>,
    shared: Mutex<HashMap<McpServerConfig, Weak<Server>>>,
}

impl std::fmt::Debug for McpHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpHub").field("limits", &self.limits).field("shared_servers", &self.shared_servers()).finish()
    }
}

/// A server that could not be reached; its tools are missing from the set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpFailure {
    pub server: String,
    pub error: String,
}

/// A server's `instructions` from its `initialize` result, for the prompt's
/// "# MCP Server Instructions" section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpInstructions {
    pub server: String,
    pub text: Arc<str>,
}

impl McpHub {
    /// MCP sign-ins are kept in memory and their addresses printed to stderr;
    /// see [`with_oauth`](Self::with_oauth).
    pub fn new(limits: McpLimits) -> Arc<Self> {
        Self::with_oauth(limits, McpCredentials::in_memory(), Arc::new(PrintPrompter::default()))
    }

    /// `credentials`: where MCP sign-ins are kept (Claude Code's credentials
    /// store to share them with it); `prompter`: how the user is sent to
    /// authorize a server.
    pub fn with_oauth(limits: McpLimits, credentials: McpCredentials, prompter: Arc<dyn AuthPrompter>) -> Arc<Self> {
        Self::with_oauth_options(limits, credentials, prompter, OAuthOptions::default())
    }

    /// [`with_oauth`](Self::with_oauth), with Claude Code's `xaaIdp` setting
    /// for cross-app access and the HTTP client OAuth requests go through.
    pub fn with_oauth_options(
        limits: McpLimits,
        credentials: McpCredentials,
        prompter: Arc<dyn AuthPrompter>,
        options: OAuthOptions,
    ) -> Arc<Self> {
        Arc::new(Self {
            stdio_slots: Semaphore::new(limits.stdio_connects.max(1)),
            remote_slots: Semaphore::new(limits.remote_connects.max(1)),
            limits,
            auth: Arc::new(AuthContext::new(credentials, prompter, options)),
            shared: Mutex::default(),
        })
    }

    /// Every server in `configs` (a config whose name repeats an earlier one
    /// is ignored), connecting as Claude Code connects them: in the
    /// background, so an agent starts without waiting (servers still
    /// connecting are [`McpToolset::pending`] and their tools join the set
    /// when they are listed), except servers configured with `alwaysLoad`
    /// (all of them with `MCP_CONNECTION_NONBLOCKING=0`), which are waited for
    /// up to `MCP_CONNECT_TIMEOUT_MS` (5 s) before connecting goes on in the
    /// background. Unreachable servers end up in [`McpToolset::failures`].
    pub async fn toolset(self: &Arc<Self>, configs: &[McpServerConfig]) -> McpToolset {
        let mut servers: Vec<Arc<Server>> = Vec::with_capacity(configs.len());
        for config in configs {
            if !servers.iter().any(|s| s.config.name == config.name) {
                servers.push(self.server_for(config));
            }
        }
        let entries = servers
            .iter()
            .map(|s| Entry {
                name: s.config.name.clone(),
                transport: transport_name(&s.config.transport),
                slot: Slot::Pending,
            })
            .collect();
        let state = Arc::new(SetState {
            entries: Mutex::new(entries),
            settled: AtomicU64::new(0),
            changed: tokio::sync::Notify::new(),
        });
        let blocking = nonblocking_disabled();
        let mut tasks = Vec::with_capacity(servers.len());
        let mut waited = Vec::new();
        for (i, server) in servers.into_iter().enumerate() {
            let waits = blocking || server.config.always_load;
            let set = Arc::clone(&state);
            let (done, settled) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                let outcome = server.ensure_connected().await;
                set.settle(i, outcome.map(|()| server));
                let _ = done.send(());
            });
            tasks.push(task.abort_handle());
            if waits {
                waited.push(settled);
            }
        }
        if !waited.is_empty() {
            let _ = tokio::time::timeout(connect_wait(), futures_util::future::join_all(waited)).await;
        }
        McpToolset { state, tasks }
    }

    /// [`toolset`](Self::toolset), returned once every server has connected,
    /// failed or asked for a sign-in.
    pub async fn connected_toolset(self: &Arc<Self>, configs: &[McpServerConfig]) -> McpToolset {
        let set = self.toolset(configs).await;
        set.settled().await;
        set
    }

    /// Shared server instances alive right now.
    pub fn shared_servers(&self) -> usize {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).values().filter(|w| w.strong_count() > 0).count()
    }

    fn server_for(self: &Arc<Self>, config: &McpServerConfig) -> Arc<Server> {
        if !config.shared {
            return Server::new(config.clone(), self, false);
        }
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(server) = shared.get(config).and_then(Weak::upgrade) {
            return server;
        }
        let server = Server::new(config.clone(), self, true);
        shared.insert(config.clone(), Arc::downgrade(&server));
        server
    }

    /// Called by a shared server as it ends; a newer instance under the same
    /// config stays listed.
    pub(crate) fn forget(&self, config: &McpServerConfig, server: &Weak<Server>) {
        let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if shared.get(config).is_some_and(|w| w.ptr_eq(server)) {
            shared.remove(config);
        }
    }
}

/// Claude Code's `MCP_CONNECTION_NONBLOCKING`: off (`0`, `false`), every
/// server is waited for as `alwaysLoad` ones are.
fn nonblocking_disabled() -> bool {
    std::env::var("MCP_CONNECTION_NONBLOCKING")
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"))
}

/// Claude Code's `MCP_CONNECT_TIMEOUT_MS` (default 5 s): how long the
/// servers that are waited for are waited for.
fn connect_wait() -> Duration {
    std::env::var("MCP_CONNECT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(Duration::from_secs(5), |ms| Duration::from_millis(ms.min(2_147_483_647)))
}

/// One server of a set and where it stands.
struct Entry {
    name: String,
    transport: &'static str,
    slot: Slot,
}

enum Slot {
    /// Still connecting, in the background (its task holds the server).
    Pending,
    /// Connected, or waiting for a sign-in (its auth tools stand in).
    Live(Arc<Server>),
    /// Could not be reached; the server is let go.
    Failed(String),
}

struct SetState {
    entries: Mutex<Vec<Entry>>,
    /// Servers that stopped being pending (part of the set's version).
    settled: AtomicU64,
    changed: tokio::sync::Notify,
}

impl SetState {
    fn settle(&self, i: usize, outcome: Result<Arc<Server>, String>) {
        if let Some(entry) = self.lock().get_mut(i) {
            entry.slot = match outcome {
                Ok(server) => Slot::Live(server),
                Err(error) => Slot::Failed(error),
            };
        }
        self.settled.fetch_add(1, Ordering::AcqRel);
        self.changed.notify_waiters();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One agent's MCP tools. Holding it keeps its servers running; dropping it
/// releases them (a server shared with other agents keeps running for them)
/// and stops connecting the ones still pending.
pub struct McpToolset {
    state: Arc<SetState>,
    tasks: Vec<tokio::task::AbortHandle>,
}

impl Drop for McpToolset {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl std::fmt::Debug for McpToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolset")
            .field("servers", &self.servers())
            .field("pending", &self.pending())
            .field("failures", &self.failures())
            .finish()
    }
}

impl McpToolset {
    /// The servers connected (or waiting for a sign-in), in config order.
    fn live(&self) -> Vec<Arc<Server>> {
        self.state
            .lock()
            .iter()
            .filter_map(|e| match &e.slot {
                Slot::Live(server) => Some(Arc::clone(server)),
                _ => None,
            })
            .collect()
    }

    /// Every tool as of now: each connected server's tools, then
    /// `ListMcpResourcesTool` and `ReadMcpResourceTool` when one offers
    /// resources. Tool objects are shared by every agent using the same server.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let live = self.live();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for server in &live {
            tools.extend(server.catalog().tools.iter().cloned());
        }
        if live.iter().any(|s| s.catalog().resources) {
            let weak: Arc<[Weak<Server>]> = live.iter().map(Arc::downgrade).collect();
            tools.push(Arc::new(ListResources::new(Arc::clone(&weak))));
            tools.push(Arc::new(ReadResource::new(weak)));
        }
        tools
    }

    /// Changes when a server's tool list changes (`notifications/tools/list_changed`,
    /// a reconnect listing different tools, a server done connecting): rebuild
    /// the agent's toolbox from [`tools`](Self::tools) when it differs from the
    /// last value seen.
    pub fn version(&self) -> u64 {
        self.live().iter().map(|s| s.version()).sum::<u64>() + self.state.settled.load(Ordering::Acquire)
    }

    /// The `instructions` of the connected servers, in config order.
    pub fn instructions(&self) -> Vec<McpInstructions> {
        self.live()
            .iter()
            .filter_map(|s| {
                s.catalog().instructions.clone().map(|text| McpInstructions { server: s.config.name.clone(), text })
            })
            .collect()
    }

    /// Servers that could not be reached; their tools are missing from the set.
    pub fn failures(&self) -> Vec<McpFailure> {
        self.state
            .lock()
            .iter()
            .filter_map(|e| match &e.slot {
                Slot::Failed(error) => Some(McpFailure { server: e.name.clone(), error: error.clone() }),
                _ => None,
            })
            .collect()
    }

    /// Servers still connecting (Claude Code's `pending` clients).
    pub fn pending(&self) -> Vec<String> {
        self.state.lock().iter().filter(|e| matches!(e.slot, Slot::Pending)).map(|e| e.name.clone()).collect()
    }

    /// Returns once no server is still connecting.
    pub async fn settled(&self) {
        self.wait_for_pending(&[], Duration::MAX).await;
    }

    /// Returns once none of `servers` (any of them when empty) is still
    /// connecting, or after `limit`; how long it waited.
    pub async fn wait_for_pending(&self, servers: &[String], limit: Duration) -> Duration {
        let start = std::time::Instant::now();
        let waiting = || {
            self.state
                .lock()
                .iter()
                .any(|e| matches!(e.slot, Slot::Pending) && (servers.is_empty() || servers.contains(&e.name)))
        };
        let wait = async {
            loop {
                let changed = self.state.changed.notified();
                if !waiting() {
                    return;
                }
                changed.await;
            }
        };
        if limit == Duration::MAX {
            wait.await;
        } else {
            let _ = tokio::time::timeout(limit, wait).await;
        }
        start.elapsed()
    }

    /// Servers waiting for a sign-in (their `authenticate` tools stand in
    /// for their own).
    pub fn needs_auth(&self) -> Vec<String> {
        self.live().iter().filter(|s| s.needs_auth()).map(|s| s.config.name.clone()).collect()
    }

    /// Names of the servers this set reaches (connected or waiting for a sign-in).
    pub fn servers(&self) -> Vec<String> {
        self.live().iter().map(|s| s.config.name.clone()).collect()
    }

    /// Every configured server and where it stands, as Claude Code's `/mcp`
    /// lists them: connected, still connecting, waiting for a sign-in, not
    /// connected now (idle or not yet used), or failed to connect.
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.state
            .lock()
            .iter()
            .map(|e| {
                let (state, tools) = match &e.slot {
                    Slot::Pending => (McpServerState::Pending, 0),
                    Slot::Live(s) => (s.state(), if s.needs_auth() { 0 } else { s.catalog().tools.len() }),
                    Slot::Failed(error) => (McpServerState::Failed(error.clone()), 0),
                };
                McpServerStatus { name: e.name.clone(), transport: e.transport, state, tools }
            })
            .collect()
    }

    /// `/mcp` "Reconnect" for `server`.
    pub async fn reconnect(&self, server: &str) -> Result<(), String> {
        self.server(server)?.reconnect().await
    }

    /// `/mcp` "Clear authentication" for `server`: revokes its stored tokens
    /// at the authorization server (when it offers revocation), forgets
    /// them, and connects again. `Ok(false)` for a server without OAuth.
    pub async fn sign_out(&self, server: &str) -> Result<bool, String> {
        self.server(server)?.sign_out().await
    }

    fn server(&self, name: &str) -> Result<Arc<Server>, String> {
        self.live()
            .into_iter()
            .find(|s| s.config.name == name)
            .ok_or_else(|| format!("No MCP server named \"{name}\"."))
    }
}

/// One server as `/mcp` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    /// `stdio`, `http` or `sse`.
    pub transport: &'static str,
    pub state: McpServerState,
    /// Tools it offers now.
    pub tools: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerState {
    Connected,
    /// Still connecting (in the background).
    Pending,
    /// Waiting for a sign-in (its tools are the two auth tools).
    NeedsAuth,
    /// Not connected right now (closed while idle, or not used yet).
    Idle,
    Failed(String),
}

fn transport_name(transport: &McpTransport) -> &'static str {
    match transport {
        McpTransport::Stdio { .. } => "stdio",
        McpTransport::Http { .. } => "http",
        McpTransport::Sse { .. } => "sse",
    }
}
