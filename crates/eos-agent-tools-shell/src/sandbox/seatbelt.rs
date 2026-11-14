//! macOS: every sandboxed command runs as `/usr/bin/sandbox-exec -p
//! <profile> <shell> -c …`. sandbox-exec execs in place, so the shell keeps
//! the pid we started, and process groups, kills and timeouts are unchanged.

use std::io;
use std::path::{Path, PathBuf};

use super::paths::{self, expand, glob_regex, is_glob, real, spellings};
use super::profile::{ProfileSpec, Rule, usable};
use super::protect::fence;
use super::{Sandbox, SandboxDirs, SandboxRequest, Wrapped};

pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[derive(Debug)]
pub struct Seatbelt {
    dirs: SandboxDirs,
}

impl Seatbelt {
    /// Protecting Claude Code's files under `dirs`.
    pub fn new(dirs: SandboxDirs) -> Self {
        Self { dirs }
    }

    /// Can this machine run it? (`sandbox-exec` present and working.)
    pub fn available() -> bool {
        std::process::Command::new(SANDBOX_EXEC)
            .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// The profile for one command.
    pub(crate) fn profile(&self, req: &SandboxRequest<'_>) -> io::Result<String> {
        let cfg = req.config;
        let home = self.dirs.home.as_deref();
        let settings_paths = |list: &Option<Vec<String>>| -> Vec<PathBuf> {
            list.iter().flatten().filter(|p| !is_glob(p)).filter_map(|p| expand(p, req.cwd, home)).collect()
        };
        // Globs become regexes over the real path, as sandbox-runtime writes them.
        let settings_globs = |list: &Option<Vec<String>>| -> Vec<String> {
            let globs = list.iter().flatten().filter(|p| is_glob(p));
            globs.filter_map(|p| expand(p, req.cwd, home)).map(|g| glob_regex(&real(&g))).collect()
        };
        // Built from resolved paths, so every entry is already spelled as Seatbelt sees it.
        let real_roots: Vec<PathBuf> = req.roots.iter().map(|r| real(r)).collect();
        let real_dirs = SandboxDirs {
            home: home.map(real),
            config_dir: self.dirs.config_dir.as_deref().map(real),
            managed_dir: self.dirs.managed_dir.clone(),
        };
        let fence = fence(&real(req.cwd), &real_roots, &real_dirs);

        let mut roots: Vec<PathBuf> = paths::default_writable(cfg, req.cwd, home);
        roots.extend(req.roots.iter().cloned());
        roots.push(req.tmpdir.to_path_buf());
        roots.extend(settings_paths(&cfg.filesystem.allow_write));
        roots.extend(fence.writable.iter().cloned());
        let write_roots = resolved(&roots);

        let mut deny_write: Vec<Rule> = paths::rules(&fence.write);
        for path in cfg.filesystem.deny_write.iter().flatten() {
            if is_glob(path) {
                if let Some(glob) = expand(path, req.cwd, home) {
                    deny_write.push(Rule::Regex(glob_regex(&real(&glob))));
                }
            } else if let Some(p) = expand(path, req.cwd, home) {
                deny_write.extend(spellings(&p).into_iter().map(Rule::Subpath));
            }
        }

        let mut credential_deny = Vec::new();
        for path in cfg.credentials.iter().flat_map(|c| c.denied_files()) {
            let Some(p) = expand(&path, req.cwd, home) else { continue };
            if is_glob(&path) {
                credential_deny.push(Rule::Regex(glob_regex(&real(&p))));
            } else {
                credential_deny.extend(spellings(&p).into_iter().map(Rule::Subpath));
            }
        }

        let spec = ProfileSpec {
            log_tag: req.log_tag.to_owned(),
            proxy_port: req.proxy_port,
            write_roots,
            write_files: resolved(req.write_files),
            deny_write,
            deny_read: resolved(&[fence.read, settings_paths(&cfg.filesystem.deny_read)].concat()),
            allow_read: resolved(&settings_paths(&cfg.filesystem.allow_read)),
            write_globs: settings_globs(&cfg.filesystem.allow_write),
            deny_read_globs: settings_globs(&cfg.filesystem.deny_read),
            allow_read_globs: settings_globs(&cfg.filesystem.allow_read),
            credential_deny,
            own_allow_read: resolved(req.own_read),
            allow_local_binding: cfg.allow_local_binding(),
            allow_unix_sockets: resolved(&settings_paths(&cfg.network.allow_unix_sockets)),
            allow_all_unix_sockets: cfg.network.allow_all_unix_sockets.unwrap_or(false),
            allow_mach_lookup: cfg.network.allow_mach_lookup.clone().unwrap_or_default(),
            allow_pty: false,
            enable_weaker_network_isolation: cfg.enable_weaker_network_isolation.unwrap_or(false),
            allow_apple_events: cfg.allow_apple_events.unwrap_or(false),
        };
        let all = spec.write_roots.iter().chain(&spec.write_files).chain(&spec.deny_read).chain(&spec.allow_read);
        if let Some(bad) = all.into_iter().find(|p| !usable(p)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a sandbox path holds a NUL or line break: {:?}", bad.display().to_string()),
            ));
        }
        Ok(spec.render())
    }
}

/// Each path in both spellings, without duplicates.
fn resolved(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(paths.len() * 2);
    for p in paths {
        for s in spellings(Path::new(p)) {
            if !out.contains(&s) {
                out.push(s);
            }
        }
    }
    out
}

impl Sandbox for Seatbelt {
    fn wrap(&self, req: &SandboxRequest<'_>) -> io::Result<Wrapped> {
        let profile = self.profile(req)?;
        Ok(Wrapped { program: PathBuf::from(SANDBOX_EXEC), args: vec!["-p".into(), profile] })
    }
}
