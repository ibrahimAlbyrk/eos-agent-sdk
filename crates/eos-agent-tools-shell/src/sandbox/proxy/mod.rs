//! The one network proxy every sandboxed command of the process goes
//! through: HTTP (CONNECT and absolute-form) and SOCKS5 on one loopback port.
//! Each command gets its own random credentials, so a connection is judged
//! for the agent that made it; hosts no list decides go to the permission
//! gate as `SandboxNetworkAccess` (Claude Code's design).

#[cfg(test)]
mod bench;
pub(crate) mod filter;
mod http;
mod net;
mod socks;
pub(crate) mod tls;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use eos_agent_core::{PermissionGate, ToolCall};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinHandle;

use self::filter::{HostPolicy, Verdict};
use self::net::local_addresses;
pub(crate) use self::net::{read_head, relay};
use self::tls::ca::Ca;
use super::credentials::Vault;
use super::violations::Violations;

/// The name the permission gate sees for a host no list decides.
pub const NETWORK_TOOL: &str = "SandboxNetworkAccess";

const MAX_CONNECTIONS: usize = 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const HEAD_LIMIT: usize = 16 * 1024;

/// Who a command's connections belong to.
#[derive(Debug)]
pub(crate) struct Owner {
    pub agent_id: String,
    /// Grants last as long as this session (the top-level agent and its
    /// subagents), as a "Yes" lasts for a Claude Code session.
    pub session: String,
    pub cmd_id: String,
    /// The agent's host lists (its own working directory's settings).
    pub hosts: Arc<HostPolicy>,
    /// The CA its CONNECT tunnels are terminated with (`tlsTerminate`).
    pub tls: Option<Arc<Ca>>,
    /// Its masked credentials, swapped back on the way out.
    pub vault: Arc<Vault>,
    /// Swap them on plain-HTTP requests too (`allowPlaintextInject`).
    pub plaintext_inject: bool,
    /// What to do with SigV4 requests that cannot be signed again.
    pub sigv4: super::Sigv4Policies,
}

#[derive(Debug)]
struct Registration {
    password: String,
    owner: Owner,
    /// The tunnel-limit refusal was recorded for this command.
    tunnel_refused: AtomicBool,
}

type AskKey = (String, String);

pub(crate) struct Shared {
    gate: Arc<dyn PermissionGate>,
    violations: Arc<Violations>,
    commands: Mutex<HashMap<String, Arc<Registration>>>,
    grants: Mutex<HashMap<String, HashSet<String>>>,
    asking: Mutex<HashMap<AskKey, Arc<OnceCell<bool>>>>,
    asks: AtomicU64,
    connections: Arc<Semaphore>,
    /// TLS-terminated tunnels open now.
    tunnels: AtomicUsize,
    upstream_tls: OnceLock<Arc<rustls::ClientConfig>>,
    /// A terminated tunnel's timeouts (sandbox-runtime's defaults; tests shorten them).
    tls_timeouts: OnceLock<tls::watchdog::Timeouts>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

/// The running proxy; stops when dropped.
#[derive(Debug)]
pub(crate) struct Proxy {
    port: u16,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A command's credentials; dropping it unregisters them.
#[derive(Debug)]
pub(crate) struct Lease {
    shared: Arc<Shared>,
    pub user: String,
    pub password: String,
}

impl Drop for Lease {
    fn drop(&mut self) {
        lock(&self.shared.commands).remove(&self.user);
    }
}

impl Proxy {
    pub(crate) async fn start(gate: Arc<dyn PermissionGate>, violations: Arc<Violations>) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            gate,
            violations,
            commands: Mutex::default(),
            grants: Mutex::default(),
            asking: Mutex::default(),
            asks: AtomicU64::new(0),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            tunnels: AtomicUsize::new(0),
            upstream_tls: OnceLock::new(),
            tls_timeouts: OnceLock::new(),
        });
        let task = tokio::spawn(accept(listener, Arc::clone(&shared)));
        Ok(Self { port, shared, task })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Fresh credentials for one command.
    pub(crate) fn register(&self, owner: Owner) -> Lease {
        let user = format!("eos.{}", owner.cmd_id);
        let password: String = std::iter::repeat_with(fastrand::alphanumeric).take(32).collect();
        let registration =
            Arc::new(Registration { password: password.clone(), owner, tunnel_refused: AtomicBool::new(false) });
        lock(&self.shared.commands).insert(user.clone(), registration);
        Lease { shared: Arc::clone(&self.shared), user, password }
    }
}

async fn accept(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Ok(permit) = Arc::clone(&shared.connections).try_acquire_owned() else {
            drop(stream); // over the limit: refuse rather than queue
            continue;
        };
        let shared = Arc::clone(&shared);
        tokio::spawn(async move {
            let _permit = permit;
            let _ = serve(&shared, stream).await;
        });
    }
}

async fn serve(shared: &Shared, stream: TcpStream) -> io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut first = [0u8; 1];
    if stream.peek(&mut first).await? == 0 {
        return Ok(());
    }
    if first[0] == 0x05 { socks::serve(shared, stream).await } else { http::serve(shared, stream).await }
}

/// Why a connection was not made.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// Bad credentials: the client must not be served at all.
    Unauthenticated,
    /// A policy said no; the reason is shown and recorded.
    Denied(String),
    /// The name resolved only to addresses the guard refuses.
    Address(String),
    /// Resolving or connecting failed.
    Unreachable,
}

impl Shared {
    /// The client side of a terminated tunnel's upstream connection: the
    /// default roots, no ALPN (Node's `https.Agent`).
    fn tls_timeouts(&self) -> tls::watchdog::Timeouts {
        self.tls_timeouts.get().copied().unwrap_or_default()
    }

    fn upstream_tls(&self) -> &Arc<rustls::ClientConfig> {
        self.upstream_tls.get_or_init(|| {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map(|b| b.with_root_certificates(tls::ca::upstream_roots()).with_no_client_auth());
            match config {
                Ok(config) => Arc::new(config),
                Err(e) => panic!("the default TLS versions are always supported: {e}"),
            }
        })
    }

    /// The command these credentials were made for. A connection without
    /// them belongs to no agent, so no agent's lists can judge it.
    fn authenticate(&self, creds: Option<(&str, &str)>) -> Result<Arc<Registration>, Refusal> {
        let Some((user, password)) = creds else { return Err(Refusal::Unauthenticated) };
        let found = lock(&self.commands).get(user).cloned();
        match found {
            Some(r) if constant_time_eq(r.password.as_bytes(), password.as_bytes()) => Ok(r),
            _ => Err(Refusal::Unauthenticated),
        }
    }

    /// Claude Code's filter with the command's agent's lists, then the dial
    /// with the address guard.
    async fn open(&self, who: &Registration, host: &str, port: u16) -> Result<TcpStream, Refusal> {
        if let Err(reason) = self.decide(who, host, port).await {
            self.record(who, host, port, &reason);
            return Err(Refusal::Denied(reason));
        }
        self.dial(who, host, port).await
    }

    async fn decide(&self, who: &Registration, host: &str, port: u16) -> Result<(), String> {
        match who.owner.hosts.judge(host, port) {
            Verdict::Allow => return Ok(()),
            Verdict::Deny(why) => return Err(why.to_owned()),
            Verdict::Unlisted => {}
        }
        let host = filter::canonical(host).to_ascii_lowercase();
        let session = &who.owner.session;
        if lock(&self.grants).get(session).is_some_and(|g| g.contains(&host)) {
            return Ok(());
        }
        // One question per session and host, however many connections wait on it.
        let key = (session.clone(), host.clone());
        let cell = Arc::clone(lock(&self.asking).entry(key.clone()).or_default());
        let allowed = *cell.get_or_init(|| self.ask(who, &host, port)).await;
        lock(&self.asking).remove(&key);
        if allowed {
            lock(&self.grants).entry(session.clone()).or_default().insert(host);
            Ok(())
        } else {
            Err("user denied".into())
        }
    }

    async fn ask(&self, who: &Registration, host: &str, port: u16) -> bool {
        let n = self.asks.fetch_add(1, Ordering::Relaxed);
        let id = format!("net_{n}");
        let input = serde_json::json!({"host": host, "port": port}).to_string();
        let Ok(input) = serde_json::value::RawValue::from_string(input) else { return false };
        let call = ToolCall {
            agent_id: &who.owner.agent_id,
            id: &id,
            name: NETWORK_TOOL,
            input: &input,
            sandboxed: false,
            review: None,
            workspace: None,
            origin: None,
        };
        self.gate.decide(call).await.is_allowed()
    }

    fn record(&self, who: &Registration, host: &str, port: u16, reason: &str) {
        self.violations.record(&who.owner.cmd_id, &format!("deny network-outbound {host}:{port} ({reason})"));
    }

    async fn dial(&self, who: &Registration, host: &str, port: u16) -> Result<TcpStream, Refusal> {
        let addrs: Vec<SocketAddr> = match filter::canonical(host).parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, port)],
            Err(_) => tokio::net::lookup_host((host, port)).await.map_err(|_| Refusal::Unreachable)?.collect(),
        };
        let local = local_addresses();
        let mut refused = Vec::new();
        let mut dialed = false;
        for addr in addrs {
            if let Some(why) = who.owner.hosts.guard(host, addr.ip(), port, &local) {
                if !refused.contains(&why) {
                    refused.push(why);
                }
                continue;
            }
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    return Ok(stream);
                }
                Ok(Err(_)) | Err(_) => dialed = true,
            }
        }
        if !dialed && !refused.is_empty() {
            let reason = format!("resolved to {}", refused.join(" / "));
            self.record(who, host, port, &reason);
            return Err(Refusal::Address(format!("Connection to {host} blocked: {reason}")));
        }
        Err(Refusal::Unreachable)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
