//! Claude Code's `sandbox` settings, in the shape of its settings files, so a
//! `settings.json` fragment can be passed unchanged.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use super::credentials::{
    AwsPair, CredentialEnv, CredentialFile, CredentialMode, CredentialsConfig, Decode, OnExtractNoMatch, Sigv4Policies,
    Sigv4Policy, TlsTerminateConfig,
};

/// The `sandbox` object of Claude Code's settings. Unset fields take Claude
/// Code's defaults (see the accessors); keys this engine does not implement
/// are kept in `other` and ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub fail_if_unavailable: Option<bool>,
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    pub allow_unsandboxed_commands: Option<bool>,
    pub excluded_commands: Option<Vec<String>>,
    pub ignore_violations: Option<BTreeMap<String, Vec<String>>>,
    pub filesystem: FilesystemConfig,
    pub network: NetworkConfig,
    pub allow_apple_events: Option<bool>,
    pub enable_weaker_network_isolation: Option<bool>,
    /// Credential files and variables kept from sandboxed commands.
    pub credentials: Option<CredentialsConfig>,
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FilesystemConfig {
    pub allow_write: Option<Vec<String>>,
    pub deny_write: Option<Vec<String>>,
    pub deny_read: Option<Vec<String>>,
    pub allow_read: Option<Vec<String>>,
    /// Managed settings only: `allowRead` from the managed settings alone.
    pub allow_managed_read_paths_only: Option<bool>,
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct NetworkConfig {
    pub allowed_domains: Option<Vec<String>>,
    pub denied_domains: Option<Vec<String>>,
    pub strict_allowlist: Option<bool>,
    pub allow_unix_sockets: Option<Vec<String>>,
    pub allow_all_unix_sockets: Option<bool>,
    pub allow_local_binding: Option<bool>,
    pub allow_mach_lookup: Option<Vec<String>>,
    /// Managed settings only: `allowedDomains` from the managed settings alone.
    pub allow_managed_domains_only: Option<bool>,
    /// The proxy terminates TLS with its own CA (trusted tiers only).
    pub tls_terminate: Option<TlsTerminateConfig>,
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub other: BTreeMap<String, Value>,
}

impl SandboxConfig {
    /// Claude Code's default: off.
    pub fn enabled(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    pub fn fail_if_unavailable(&self) -> bool {
        self.fail_if_unavailable.unwrap_or(false)
    }

    pub fn auto_allow_bash_if_sandboxed(&self) -> bool {
        self.auto_allow_bash_if_sandboxed.unwrap_or(true)
    }

    pub fn allow_unsandboxed_commands(&self) -> bool {
        self.allow_unsandboxed_commands.unwrap_or(true)
    }

    pub fn excluded_commands(&self) -> &[String] {
        self.excluded_commands.as_deref().unwrap_or_default()
    }

    pub fn allow_local_binding(&self) -> bool {
        self.network.allow_local_binding.unwrap_or(false)
    }

    pub fn strict_allowlist(&self) -> bool {
        self.network.strict_allowlist.unwrap_or(false)
    }

    pub fn allowed_domains(&self) -> &[String] {
        self.network.allowed_domains.as_deref().unwrap_or_default()
    }

    pub fn denied_domains(&self) -> &[String] {
        self.network.denied_domains.as_deref().unwrap_or_default()
    }

    /// `layer` on top of `self`, as Claude Code merges settings sources: a
    /// value set in `layer` wins, lists are joined (duplicates dropped).
    pub fn merge(&mut self, layer: &SandboxConfig) {
        over(&mut self.enabled, layer.enabled);
        over(&mut self.fail_if_unavailable, layer.fail_if_unavailable);
        over(&mut self.auto_allow_bash_if_sandboxed, layer.auto_allow_bash_if_sandboxed);
        over(&mut self.allow_unsandboxed_commands, layer.allow_unsandboxed_commands);
        join(&mut self.excluded_commands, &layer.excluded_commands);
        if let Some(map) = &layer.ignore_violations {
            let mine = self.ignore_violations.get_or_insert_default();
            for (k, v) in map {
                let mut list = Some(mine.remove(k).unwrap_or_default());
                join(&mut list, &Some(v.clone()));
                mine.insert(k.clone(), list.unwrap_or_default());
            }
        }
        over(&mut self.allow_apple_events, layer.allow_apple_events);
        over(&mut self.enable_weaker_network_isolation, layer.enable_weaker_network_isolation);
        let (fs, lfs) = (&mut self.filesystem, &layer.filesystem);
        join(&mut fs.allow_write, &lfs.allow_write);
        join(&mut fs.deny_write, &lfs.deny_write);
        join(&mut fs.deny_read, &lfs.deny_read);
        join(&mut fs.allow_read, &lfs.allow_read);
        over(&mut fs.allow_managed_read_paths_only, lfs.allow_managed_read_paths_only);
        let (net, lnet) = (&mut self.network, &layer.network);
        join(&mut net.allowed_domains, &lnet.allowed_domains);
        join(&mut net.denied_domains, &lnet.denied_domains);
        over(&mut net.strict_allowlist, lnet.strict_allowlist);
        join(&mut net.allow_unix_sockets, &lnet.allow_unix_sockets);
        over(&mut net.allow_all_unix_sockets, lnet.allow_all_unix_sockets);
        over(&mut net.allow_local_binding, lnet.allow_local_binding);
        join(&mut net.allow_mach_lookup, &lnet.allow_mach_lookup);
        over(&mut net.allow_managed_domains_only, lnet.allow_managed_domains_only);
        if lnet.tls_terminate.is_some() {
            net.tls_terminate.clone_from(&lnet.tls_terminate);
        }
        if let Some(theirs) = &layer.credentials {
            let mine = self.credentials.get_or_insert_default();
            if let Some(b) = &theirs.files {
                mine.files.get_or_insert_default().extend(b.iter().cloned());
            }
            if let Some(b) = &theirs.env_vars {
                mine.env_vars.get_or_insert_default().extend(b.iter().cloned());
            }
            if let Some(b) = &theirs.aws_pairs {
                mine.aws_pairs.get_or_insert_default().extend(b.iter().cloned());
            }
            over(&mut mine.allow_plaintext_inject, theirs.allow_plaintext_inject);
            over(&mut mine.sigv4, theirs.sigv4);
        }
        for (k, v) in &layer.other {
            self.other.insert(k.clone(), v.clone());
        }
    }

    /// The `sandbox` value of a settings file, read leniently: a value that
    /// does not fit leaves the whole object out, as a broken settings file
    /// would in Claude Code.
    pub fn from_settings(settings: &Value) -> Option<Self> {
        let cfg: Self = serde_json::from_value(settings.get("sandbox")?.clone()).ok()?;
        if let Some(problem) = cfg.credentials.as_ref().and_then(CredentialsConfig::problem) {
            tracing::warn!("sandbox.credentials: {problem}; the settings file's sandbox settings are ignored");
            return None;
        }
        Some(cfg)
    }

    /// Keys accepted but not implemented here (own proxy ports, Linux-only
    /// settings), for a warning.
    pub fn unsupported_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.other.keys().map(|k| format!("sandbox.{k}")).collect();
        keys.extend(self.filesystem.other.keys().map(|k| format!("sandbox.filesystem.{k}")));
        keys.extend(self.network.other.keys().map(|k| format!("sandbox.network.{k}")));
        keys
    }
}

/// Settings paths of a tier whose relative paths start at `base` (Claude
/// Code's: the config directory for the user's settings, the working
/// directory for the others): `//x` is `/x`, `~` stays for later, relative
/// paths are joined to `base`; globs and a trailing `/**` are kept.
pub(crate) fn rebase(list: &mut Option<Vec<String>>, base: &std::path::Path) {
    for path in list.iter_mut().flatten() {
        if let Some(abs) = path.strip_prefix("//") {
            *path = format!("/{abs}");
        } else if !path.starts_with('/') && !path.starts_with('~') {
            *path = base.join(path.as_str()).to_string_lossy().into_owned();
        }
    }
}

pub(crate) fn over<T: Copy>(mine: &mut Option<T>, theirs: Option<T>) {
    if theirs.is_some() {
        *mine = theirs;
    }
}

pub(crate) fn join(mine: &mut Option<Vec<String>>, theirs: &Option<Vec<String>>) {
    let Some(theirs) = theirs else { return };
    let list = mine.get_or_insert_default();
    for item in theirs {
        if !list.contains(item) {
            list.push(item.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claude_codes_shape_and_defaults() {
        let settings = serde_json::json!({"sandbox": {
            "enabled": true,
            "excludedCommands": ["docker *"],
            "network": {"allowedDomains": ["github.com"], "httpProxyPort": 8080},
            "credentials": {"files": []},
            "unknownKey": 1,
        }});
        let cfg = SandboxConfig::from_settings(&settings).expect("parses");
        assert!(cfg.enabled() && cfg.auto_allow_bash_if_sandboxed() && cfg.allow_unsandboxed_commands());
        assert!(!cfg.allow_local_binding() && !cfg.strict_allowlist() && !cfg.fail_if_unavailable());
        assert_eq!(cfg.allowed_domains(), ["github.com"]);
        assert_eq!(cfg.unsupported_keys(), ["sandbox.unknownKey", "sandbox.network.httpProxyPort"]);
        assert!(!SandboxConfig::default().enabled(), "off unless turned on");
    }

    #[test]
    fn later_layers_win_and_lists_join() {
        let mut base: SandboxConfig = serde_json::from_value(serde_json::json!({
            "enabled": true, "network": {"allowedDomains": ["a.com"], "allowLocalBinding": true}
        }))
        .expect("base");
        let host: SandboxConfig = serde_json::from_value(serde_json::json!({
            "enabled": false, "network": {"allowedDomains": ["b.com", "a.com"]}
        }))
        .expect("host");
        base.merge(&host);
        assert!(!base.enabled());
        assert!(base.allow_local_binding());
        assert_eq!(base.allowed_domains(), ["a.com", "b.com"]);
    }
}
