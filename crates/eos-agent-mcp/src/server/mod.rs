//! One server instance: its connection (made lazily, remade after it dies or
//! idles out), the tools it lists, and its teardown.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

use eos_agent_core::{CancellationToken, Tool};
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientConfig, ErrorCode, Implementation, ReadResourceRequestParams,
};
use rmcp::service::{MaybeSendFuture, NotificationContext, RunningService};
use rmcp::{ClientHandler, Peer, RoleClient, ServiceError};
use serde_json::{Map, Value};

use crate::config::McpServerConfig;
use crate::hub::McpHub;
use crate::oauth::{Rejection, ServerAuth};
use crate::process::ChildGuard;
use crate::tool::McpTool;

mod auth;
mod connect;
mod step_up;

/// A server that needs a sign-in is looked at again (for a sign-in made
/// elsewhere) at most this often.
const NEEDS_AUTH_RECHECK: Duration = Duration::from_secs(10);

/// What the server offers, as of its last listing.
#[derive(Default)]
pub(crate) struct Catalog {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) instructions: Option<Arc<str>>,
    pub(crate) resources: bool,
}

/// Why a tool call attempt failed.
enum Failed {
    Plain(String),
    /// The server turned the sign-in away.
    Rejected(Rejection),
}

/// A failed request; `code` is the JSON-RPC error code when the server sent one.
#[derive(Debug)]
pub(crate) struct CallError {
    pub(crate) code: Option<i32>,
    pub(crate) message: String,
}

impl From<String> for CallError {
    fn from(message: String) -> Self {
        Self { code: None, message }
    }
}

impl From<ServiceError> for CallError {
    fn from(e: ServiceError) -> Self {
        match e {
            ServiceError::McpError(err) => {
                Self { code: Some(err.code.0), message: format!("MCP error {}: {}", err.code.0, err.message) }
            }
            other => other.to_string().into(),
        }
    }
}

pub(crate) struct Server {
    pub(crate) config: McpServerConfig,
    hub: Weak<McpHub>,
    /// Listed in the hub for sharing (and must be unlisted on drop).
    registered: bool,
    me: Weak<Server>,
    conn: tokio::sync::Mutex<Option<Connection>>,
    catalog: RwLock<Arc<Catalog>>,
    version: AtomicU64,
    in_flight: AtomicUsize,
    last_used_ms: AtomicU64,
    epoch: Instant,
    idle_timeout: Duration,
    max_output_tokens: usize,
    /// OAuth, for a remote server without its own `Authorization` header.
    auth: Option<Arc<ServerAuth>>,
    http: reqwest::Client,
    /// Set while the server waits for a sign-in; its tools are then the auth tools.
    needs_auth: Mutex<Option<NeedsAuth>>,
    auth_tools: OnceLock<Vec<Arc<dyn Tool>>>,
    /// Claude Code's needs-auth cache is consulted before the first connection only.
    cache_consulted: std::sync::atomic::AtomicBool,
    /// The step-up question in progress, shared by every call that needs it.
    step_up_flight: Mutex<Option<step_up::Flight>>,
}

struct NeedsAuth {
    rejection: Rejection,
    checked: Instant,
}

struct Connection {
    service: RunningService<RoleClient, Handler>,
    child: Option<ChildGuard>,
}

impl Connection {
    fn live_peer(&self) -> Option<Peer<RoleClient>> {
        let peer = self.service.peer();
        (!peer.is_transport_closed()).then(|| peer.clone())
    }

    async fn close(mut self) {
        let _ = self.service.close_with_timeout(Duration::from_secs(2)).await;
        if let Some(child) = self.child.take() {
            child.shutdown().await;
        }
    }
}

struct Handler {
    server: Weak<Server>,
}

impl ClientHandler for Handler {
    fn on_tool_list_changed(
        &self,
        context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        // The refresh sends a request on this same connection, so it must not
        // run inside the notification handler.
        if let Some(server) = self.server.upgrade() {
            tokio::spawn(async move {
                let _ = server.refresh(&context.peer).await;
            });
        }
        std::future::ready(())
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("eos-agent-sdk", env!("CARGO_PKG_VERSION")),
        )
    }
}

impl Server {
    pub(crate) fn new(config: McpServerConfig, hub: &Arc<McpHub>, registered: bool) -> Arc<Self> {
        let limits = &hub.limits;
        let config_for_auth = config.clone();
        let idle_timeout =
            if config.transport.is_stdio() { limits.stdio_idle_timeout } else { limits.remote_idle_timeout };
        let server = Arc::new_cyclic(|me| Self {
            config,
            hub: Arc::downgrade(hub),
            registered,
            me: me.clone(),
            conn: tokio::sync::Mutex::new(None),
            catalog: RwLock::default(),
            version: AtomicU64::new(0),
            in_flight: AtomicUsize::new(0),
            last_used_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            idle_timeout,
            max_output_tokens: limits.max_output_tokens,
            auth: ServerAuth::for_server(&config_for_auth, &hub.auth),
            http: hub.auth.http.clone(),
            needs_auth: Mutex::default(),
            auth_tools: OnceLock::new(),
            cache_consulted: std::sync::atomic::AtomicBool::new(false),
            step_up_flight: Mutex::default(),
        });
        spawn_idle_reaper(Arc::downgrade(&server), idle_timeout);
        server
    }

    pub(crate) fn catalog(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Changes whenever the tool list does.
    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Connects, unless the server waits for a sign-in (its auth tools then
    /// stand in for its own, which is not a failure).
    pub(crate) async fn ensure_connected(&self) -> Result<(), String> {
        let _busy = self.busy();
        match self.peer().await {
            Ok(_) => Ok(()),
            Err(_) if self.needs_auth() => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Calls a tool. A call refused for want of a wider scope may ask the
    /// user to sign in again and is then retried once (Claude Code's step-up).
    pub(crate) async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        cancel: &CancellationToken,
        agent_id: &str,
    ) -> Result<Value, String> {
        let _busy = self.busy();
        let work = async {
            let mut reauthed: Option<String> = None;
            loop {
                let rejection = match self.call_once(name, arguments.clone()).await {
                    Ok(mut value) => {
                        if let Some(scope) = &reauthed {
                            step_up::lead_with_reauth(&mut value, &self.config.name, scope);
                        }
                        return Ok(value);
                    }
                    Err(Failed::Plain(message)) => return Err(message),
                    Err(Failed::Rejected(rejection)) => rejection,
                };
                if let (Some(done), Rejection::StepUp { scope, .. }) = (&reauthed, &rejection) {
                    self.note_rechallenged(done, scope);
                }
                if reauthed.is_none()
                    && let Rejection::StepUp { scope, .. } = &rejection
                    && self.may_ask_step_up(scope)
                    && self.step_up(scope, agent_id).await == step_up::Outcome::Reauthenticated
                {
                    reauthed = Some(scope.clone());
                    continue;
                }
                self.reject(rejection).await;
                return Err(self.needs_auth_message());
            }
        };
        // Dropping the request sends `notifications/cancelled` to the server.
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err("Interrupted by user".into()),
            result = work => result,
        }
    }

    /// One attempt at a tool call.
    async fn call_once(&self, name: &str, arguments: Map<String, Value>) -> Result<Value, Failed> {
        let peer = self.peer().await.map_err(Failed::Plain)?;
        let call = peer.call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments));
        let result = match self.config.tool_timeout {
            Some(limit) => tokio::time::timeout(limit, call).await.map_err(|_| {
                Failed::Plain(format!(
                    "MCP server \"{}\" tool \"{name}\" timed out after {}ms",
                    self.config.name,
                    limit.as_millis()
                ))
            })?,
            None => call.await,
        };
        match result {
            Ok(r) => serde_json::to_value(r).map_err(|e| Failed::Plain(e.to_string())),
            Err(e) => {
                let message = CallError::from(e).message;
                match self.auth.as_ref().and_then(|a| a.take_rejection()) {
                    Some(rejection) => Err(Failed::Rejected(rejection)),
                    None => Err(Failed::Plain(message)),
                }
            }
        }
    }

    pub(crate) async fn list_resources(&self) -> Result<Vec<Value>, CallError> {
        let _busy = self.busy();
        let peer = self.peer().await?;
        let resources = peer.list_all_resources().await?;
        Ok(resources.into_iter().filter_map(|r| serde_json::to_value(r).ok()).collect())
    }

    pub(crate) async fn read_resource(&self, uri: &str) -> Result<Value, CallError> {
        let _busy = self.busy();
        let peer = self.peer().await?;
        let result = peer.read_resource(ReadResourceRequestParams::new(uri)).await?;
        serde_json::to_value(result).map_err(|e| CallError::from(e.to_string()))
    }

    /// The server refused the sign-in: its tools become the auth tools and
    /// the connection is dropped until a new sign-in.
    async fn reject(&self, rejection: Rejection) {
        if let Some(auth) = &self.auth {
            auth.note_needs_auth();
        }
        self.needs_sign_in(rejection);
        if let Some(conn) = self.conn.lock().await.take() {
            tokio::spawn(conn.close());
        }
    }

    /// The live connection's peer, connecting first when there is none or it died.
    async fn peer(&self) -> Result<Peer<RoleClient>, String> {
        let mut slot = self.conn.lock().await;
        if let Some(peer) = slot.as_ref().and_then(Connection::live_peer) {
            return Ok(peer);
        }
        if let Some(dead) = slot.take() {
            tokio::spawn(dead.close());
        }
        if let Some(message) = self.still_needs_auth().await {
            return Err(message);
        }
        // Claude Code skips connecting a server that asked for a sign-in in
        // the last 15 minutes ("Skipping connection (cached needs-auth)").
        if let Some(auth) = &self.auth
            && !self.cache_consulted.swap(true, Ordering::AcqRel)
            && (auth.known_to_need_auth().await || auth.skips_connect().await)
        {
            self.needs_sign_in(Rejection::SignIn { token: None });
            return Err(self.needs_auth_message());
        }
        match self.connect().await {
            Ok(conn) => {
                *lock(&self.needs_auth) = None;
                let peer = conn.service.peer().clone();
                *slot = Some(conn);
                Ok(peer)
            }
            Err(_) if self.check_rejection() => Err(self.needs_auth_message()),
            Err(e) => Err(e),
        }
    }

    async fn connect(&self) -> Result<Connection, String> {
        let hub = self.hub.upgrade().ok_or("The MCP hub is gone.")?;
        let slots = if self.config.transport.is_stdio() { &hub.stdio_slots } else { &hub.remote_slots };
        let _permit = slots.acquire().await.map_err(|e| e.to_string())?;
        let limit = hub.limits.connect_timeout;
        let attempt = async {
            let conn = self.open().await?;
            self.refresh(conn.service.peer()).await?;
            Ok::<_, String>(conn)
        };
        let result = match tokio::time::timeout(limit, attempt).await {
            Ok(result) => result,
            Err(_) => Err(format!("Connection timed out after {}ms", limit.as_millis())),
        };
        if result.is_ok()
            && let Some(auth) = &self.auth
        {
            auth.note_connected();
        }
        result
    }

    /// Lists the tools again and swaps in the new catalog. Unchanged tool
    /// definitions keep their objects, so agents' tool boxes stay valid.
    async fn refresh(&self, peer: &Peer<RoleClient>) -> Result<(), String> {
        let info = peer.peer_info();
        let capabilities = info.as_ref().map(|i| &i.capabilities);
        let listed = if capabilities.is_some_and(|c| c.tools.is_some()) {
            peer.list_all_tools().await.map_err(|e| CallError::from(e).message)?
        } else {
            Vec::new()
        };
        let tools: Vec<Arc<dyn Tool>> =
            listed.iter().map(|t| McpTool::build(self, self.me.clone(), t, self.max_output_tokens)).collect();
        let mut slot = self.catalog.write().unwrap_or_else(PoisonError::into_inner);
        let same = slot.tools.len() == tools.len()
            && slot.tools.iter().zip(&tools).all(|(a, b)| a.spec().json() == b.spec().json());
        let catalog = Catalog {
            tools: if same { slot.tools.clone() } else { tools },
            instructions: info.as_ref().and_then(|i| i.instructions.as_deref()).map(Arc::from),
            resources: capabilities.is_some_and(|c| c.resources.is_some()),
        };
        *slot = Arc::new(catalog);
        if !same {
            self.version.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }

    fn busy(&self) -> Busy<'_> {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        Busy(self)
    }

    fn touch(&self) {
        let now = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_used_ms.store(now, Ordering::Release);
    }

    fn idle_for(&self) -> Duration {
        let now = self.epoch.elapsed();
        now.saturating_sub(Duration::from_millis(self.last_used_ms.load(Ordering::Acquire)))
    }

    /// Closes the connection when nothing used it for the idle timeout.
    /// Returns how long to wait before looking again.
    async fn close_if_idle(&self) -> Duration {
        let idle_for = self.idle_for();
        let mut slot = self.conn.lock().await;
        // Checked under the lock: a call marks itself busy before it takes the lock.
        if slot.is_none() || self.in_flight.load(Ordering::Acquire) > 0 {
            return self.idle_timeout;
        }
        if idle_for < self.idle_timeout {
            return self.idle_timeout - idle_for;
        }
        let conn = slot.take();
        drop(slot);
        if let Some(conn) = conn {
            conn.close().await;
        }
        self.idle_timeout
    }
}

struct Busy<'a>(&'a Server);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.touch();
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

fn spawn_idle_reaper(server: Weak<Server>, idle_timeout: Duration) {
    tokio::spawn(async move {
        let mut wait = idle_timeout;
        loop {
            tokio::time::sleep(wait).await;
            let Some(server) = server.upgrade() else { return };
            wait = server.close_if_idle().await;
        }
    });
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.registered
            && let Some(hub) = self.hub.upgrade()
        {
            hub.forget(&self.config, &self.me);
        }
        if let Some(conn) = self.conn.get_mut().take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(conn.close());
        }
        // Without a runtime the connection is just dropped, which kills the process group.
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn is_not_found(e: &CallError) -> bool {
    e.code == Some(ErrorCode::RESOURCE_NOT_FOUND.0)
}

pub(crate) fn is_method_not_found(e: &CallError) -> bool {
    e.code == Some(ErrorCode::METHOD_NOT_FOUND.0)
}
