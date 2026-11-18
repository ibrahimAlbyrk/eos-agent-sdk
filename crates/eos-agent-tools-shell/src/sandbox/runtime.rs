//! What every sandboxed command of the process shares (the OS sandbox, the
//! proxy, the violation monitor) and what one agent's settings make of it.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use eos_agent_core::PermissionGate;
use tokio::sync::OnceCell;

use super::confine::{self, ConfineError, ConfineGuard, ConfineSpec, Confined};
use super::credentials::{self, Vault};
use super::env::command_env;
use super::proxy::filter::HostPolicy;
use super::proxy::tls::ca::Ca;
use super::proxy::{Lease, Owner, Proxy};
use super::seatbelt::SANDBOX_EXEC;
use super::violations::{Violations, Watch};
use super::{
    Sandbox, SandboxConfig, SandboxDirs, SandboxRequest, SandboxUnavailable, Seatbelt, TlsTerminateConfig, Wrapped,
    prompt,
};

/// The process's sandbox machinery: one OS sandbox, one proxy and one
/// violation monitor (both started with the first sandboxed command) for
/// every agent, whatever its settings.
pub struct SandboxRuntime {
    sandbox: Arc<dyn Sandbox>,
    /// Can the OS sandbox run here (asked once, when a policy first needs it)?
    probe: fn() -> bool,
    available: OnceLock<bool>,
    gate: Arc<dyn PermissionGate>,
    violations: Arc<Violations>,
    proxy: OnceCell<Option<Proxy>>,
    dirs: SandboxDirs,
    /// The TLS-terminating proxy's CAs, one per configured pair (and one
    /// ephemeral), shared by the agents that name them.
    cas: Mutex<HashMap<TlsTerminateConfig, Arc<Ca>>>,
}

impl fmt::Debug for SandboxRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxRuntime").field("dirs", &self.dirs).finish_non_exhaustive()
    }
}

impl SandboxRuntime {
    /// Seatbelt, protecting Claude Code's files under `dirs`. `gate` answers
    /// `SandboxNetworkAccess` for hosts no list decides.
    pub fn new(dirs: SandboxDirs, gate: Arc<dyn PermissionGate>) -> Arc<Self> {
        let seatbelt = Arc::new(Seatbelt::new(dirs.clone()));
        Self::build(seatbelt, Seatbelt::available, dirs, gate)
    }

    /// Over another sandbox mechanism (or a test double), always available.
    pub fn with_sandbox(sandbox: Arc<dyn Sandbox>, dirs: SandboxDirs, gate: Arc<dyn PermissionGate>) -> Arc<Self> {
        Self::build(sandbox, || true, dirs, gate)
    }

    fn build(
        sandbox: Arc<dyn Sandbox>,
        probe: fn() -> bool,
        dirs: SandboxDirs,
        gate: Arc<dyn PermissionGate>,
    ) -> Arc<Self> {
        Arc::new(Self {
            sandbox,
            probe,
            available: OnceLock::new(),
            gate,
            violations: Violations::new(),
            proxy: OnceCell::new(),
            dirs,
            cas: Mutex::default(),
        })
    }

    pub fn dirs(&self) -> &SandboxDirs {
        &self.dirs
    }

    /// Can the OS sandbox run here? Asked once per process (blocking, the
    /// first time).
    pub fn available(&self) -> bool {
        *self.available.get_or_init(self.probe)
    }

    /// Confines one host command to `spec`, on the shared proxy and
    /// violation monitor. Fails closed: `Unavailable` when the OS sandbox
    /// cannot run here. Blocking the first time (see [`Self::available`]).
    pub async fn confine(&self, spec: ConfineSpec) -> Result<Confined, ConfineError> {
        if !self.available() {
            return Err(ConfineError::Unavailable);
        }
        let cmd_id = new_cmd_id();
        let proxy = self.proxy().await;
        let profile = confine::profile(&spec, &self.violations.tag(&cmd_id), proxy.map(Proxy::port))?;
        tokio::fs::create_dir_all(&spec.tmpdir).await?;
        let watched = self.violations.ensure_monitor().await;
        let watch = self.violations.watch_confined(&cmd_id, spec.audit);
        let lease = proxy.map(|proxy| proxy.register(confine::owner(&cmd_id, &spec.hosts)));
        let sandbox_env = command_env(&spec.tmpdir, proxy.map(Proxy::port).zip(lease.as_ref()), true);
        let mut args = vec!["-p".to_owned(), profile, spec.program.to_string_lossy().into_owned()];
        args.extend(spec.args);
        Ok(Confined {
            program: PathBuf::from(SANDBOX_EXEC),
            args,
            env: confine::environment(spec.env, sandbox_env),
            guard: ConfineGuard { watch, lease, watched },
        })
    }

    /// The sandbox an agent with these (effective) settings runs under:
    /// `None` when they leave it off, or on but unavailable here (a warning
    /// is logged and commands run unsandboxed, as in Claude Code);
    /// `SandboxUnavailable` when unavailable and `failIfUnavailable` is set.
    /// Blocking: the first call may probe the OS sandbox.
    pub fn policy(self: &Arc<Self>, config: SandboxConfig) -> Result<Option<Arc<SandboxPolicy>>, SandboxUnavailable> {
        if !config.enabled() {
            return Ok(None);
        }
        for key in config.unsupported_keys() {
            tracing::warn!("{key} is not supported by this engine's sandbox and is ignored");
        }
        if !self.available() {
            if config.fail_if_unavailable() {
                return Err(SandboxUnavailable);
            }
            tracing::warn!("sandbox.enabled is set but the sandbox cannot start here; commands run unsandboxed");
            return Ok(None);
        }
        // sandbox-runtime makes or loads its CA when it starts; a bad pair keeps it from starting.
        let tls = match &config.network.tls_terminate {
            None => None,
            Some(tls) => match self.ca(tls) {
                Ok(ca) => Some(ca),
                Err(e) => {
                    if config.fail_if_unavailable() {
                        return Err(SandboxUnavailable);
                    }
                    tracing::warn!("the sandbox cannot start ({e}); commands run unsandboxed");
                    return Ok(None);
                }
            },
        };
        let hosts = Arc::new(HostPolicy {
            allowed: config.allowed_domains().to_vec(),
            denied: config.denied_domains().to_vec(),
            strict: config.strict_allowlist(),
            public_only: false,
        });
        let ignore = Arc::new(config.ignore_violations.clone().unwrap_or_default());
        if let Some(text) = config
            .credentials
            .as_ref()
            .and_then(|c| credentials::mask_warning(c, config.network.tls_terminate.is_some()))
        {
            tracing::warn!("{text}");
        }
        // A mask without `injectHosts` is swapped back for every allowed host (`EDn`: ports dropped).
        let mut inject_default: Vec<String> = Vec::new();
        for pattern in config.allowed_domains() {
            let host = super::proxy::filter::host_of(pattern);
            if !inject_default.contains(&host) {
                inject_default.push(host);
            }
        }
        let vault = Arc::new(Vault::default());
        Ok(Some(Arc::new(SandboxPolicy {
            runtime: Arc::clone(self),
            config,
            hosts,
            ignore,
            vault,
            inject_default,
            tls,
        })))
    }

    /// The CA for `tls` (both paths, or none for an ephemeral one).
    fn ca(&self, tls: &TlsTerminateConfig) -> std::io::Result<Arc<Ca>> {
        let mut cas = self.cas.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ca) = cas.get(tls) {
            return Ok(Arc::clone(ca));
        }
        let home = self.dirs.home.as_deref();
        let expand = |p: &str| super::paths::expand(p, Path::new("/"), home).unwrap_or_else(|| PathBuf::from(p));
        let paths = match (&tls.ca_cert_path, &tls.ca_key_path) {
            (Some(cert), Some(key)) => Some((expand(cert), expand(key))),
            (None, None) => None,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "tlsTerminate: caCertPath and caKeyPath must be provided together",
                ));
            }
        };
        let ca = Arc::new(Ca::new(paths.as_ref().map(|(c, k)| (c.as_path(), k.as_path())), &std::env::temp_dir())?);
        cas.insert(tls.clone(), Arc::clone(&ca));
        Ok(ca)
    }

    /// The proxy's port, starting it with the first command.
    async fn proxy(&self) -> Option<&Proxy> {
        self.proxy
            .get_or_init(|| async {
                match Proxy::start(Arc::clone(&self.gate), Arc::clone(&self.violations)).await {
                    Ok(proxy) => Some(proxy),
                    Err(e) => {
                        tracing::warn!(
                            "the sandbox network proxy could not start ({e}); sandboxed commands get no network"
                        );
                        None
                    }
                }
            })
            .await
            .as_ref()
    }
}

/// One agent's sandbox: its settings (from its working directory's
/// settings tiers) over the process's shared machinery. The proxy judges
/// each of its commands' connections by these settings' host lists.
pub struct SandboxPolicy {
    runtime: Arc<SandboxRuntime>,
    config: SandboxConfig,
    hosts: Arc<HostPolicy>,
    ignore: Arc<BTreeMap<String, Vec<String>>>,
    /// The agent's masked credentials.
    vault: Arc<Vault>,
    inject_default: Vec<String>,
    /// The CA its tunnels are terminated with (`tlsTerminate`).
    tls: Option<Arc<Ca>>,
}

impl fmt::Debug for SandboxPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SandboxPolicy").field("config", &self.config).finish_non_exhaustive()
    }
}

impl SandboxPolicy {
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    pub fn should_sandbox(&self, command: &str, disable_sandbox: bool) -> bool {
        super::should_sandbox(&self.config, command, disable_sandbox)
    }

    /// Claude Code's `## Bash command sandbox` section for an agent working
    /// in `cwd` (and `roots`, `cwd` first), to lead its system message.
    pub fn prompt_section(&self, cwd: &Path, roots: &[PathBuf]) -> String {
        let mut all = vec![cwd.to_path_buf()];
        all.extend(roots.iter().filter(|r| r.as_path() != cwd).cloned());
        prompt::section(&self.config, cwd, &all, &self.runtime.dirs)
    }

    /// Everything one sandboxed command needs: the wrapped launch, its
    /// environment, and the guard that holds its proxy credentials and
    /// collects its violations until it ends.
    pub(crate) async fn prepare(&self, p: Prepare<'_>) -> io::Result<Prepared> {
        let runtime = &self.runtime;
        // `credentials.envVars`, from the environment the command inherits.
        let plan = match &self.config.credentials {
            Some(c) => self
                .vault
                .plan(c, &self.inject_default, |k| std::env::var(k).ok())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?,
            None => credentials::EnvPlan::default(),
        };
        tokio::fs::create_dir_all(p.tmpdir).await?;
        let _ = runtime.violations.ensure_monitor().await;
        let cmd_id = new_cmd_id();
        let watch = runtime.violations.watch(&cmd_id, p.command, Arc::clone(&self.ignore));
        let proxy = runtime.proxy().await;
        let lease = proxy.map(|proxy| {
            proxy.register(Owner {
                agent_id: p.agent_id.to_owned(),
                session: p.session.to_owned(),
                cmd_id: cmd_id.clone(),
                hosts: Arc::clone(&self.hosts),
                tls: self.tls.clone(),
                vault: Arc::clone(&self.vault),
                plaintext_inject: self
                    .config
                    .credentials
                    .as_ref()
                    .is_some_and(credentials::CredentialsConfig::plaintext_inject),
                sigv4: self.config.credentials.as_ref().and_then(|c| c.sigv4).unwrap_or_default(),
            })
        });
        let tag = runtime.violations.tag(watch.cmd_id());
        let own_read: Vec<PathBuf> =
            self.tls.iter().flat_map(|ca| [ca.cert_path.clone(), ca.bundle_path.clone()]).collect();
        let req = SandboxRequest {
            config: &self.config,
            log_tag: &tag,
            cwd: p.cwd,
            roots: p.roots,
            tmpdir: p.tmpdir,
            write_files: p.write_files,
            proxy_port: proxy.map(Proxy::port),
            own_read: &own_read,
        };
        let wrapped = runtime.sandbox.wrap(&req)?;
        let java_unset = plan.unset.iter().any(|n| n == "JAVA_TOOL_OPTIONS");
        let mut set = plan.set;
        set.extend(command_env(p.tmpdir, proxy.map(Proxy::port).zip(lease.as_ref()), java_unset));
        if let Some(ca) = &self.tls {
            set.extend(super::env::trust_env(&ca.bundle_path));
        }
        let env = CommandEnv { set, unset: plan.unset };
        Ok(Prepared { wrapped, env, guard: SandboxGuard { watch, lease } })
    }
}

/// One command's facts for [`SandboxPolicy::prepare`].
pub(crate) struct Prepare<'a> {
    pub command: &'a str,
    pub agent_id: &'a str,
    pub session: &'a str,
    pub cwd: &'a Path,
    pub roots: &'a [PathBuf],
    pub tmpdir: &'a Path,
    pub write_files: &'a [PathBuf],
}

pub(crate) struct Prepared {
    pub wrapped: Wrapped,
    pub env: CommandEnv,
    pub guard: SandboxGuard,
}

/// A sandboxed command's environment: variables removed (`deny`
/// credentials), then variables set (placeholders, then the proxy's).
#[derive(Debug, Default)]
pub(crate) struct CommandEnv {
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

/// Held while a sandboxed command runs (in the foreground or as a task):
/// its proxy credentials stay valid and its violations are collected.
#[derive(Debug)]
pub(crate) struct SandboxGuard {
    watch: Watch,
    lease: Option<Lease>,
}

impl SandboxGuard {
    /// The command's violation lines (`failed`: it exited non-zero); its
    /// credentials end here.
    pub(crate) async fn finish(self, failed: bool) -> Vec<String> {
        let Self { watch, lease } = self;
        drop(lease);
        watch.finish(failed).await
    }
}

fn new_cmd_id() -> String {
    std::iter::repeat_with(fastrand::alphanumeric).take(12).collect()
}
