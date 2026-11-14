//! Claude Code's settings tiers and how it combines their `sandbox` objects
//! (2.1.319, `J0`, `D$`, `La`, `rb`): user, project, local, `--settings`
//! (here the host's) and managed, later ones winning and lists joined, with
//! the grants a repository could use to widen the sandbox kept to the
//! trusted tiers (user, `--settings`, managed) when those tiers ask for it.

use std::collections::BTreeMap;
use std::path::Path;

use super::SandboxConfig;
use super::config::{join, rebase};
use super::credentials;

/// The `sandbox` object of each of Claude Code's settings tiers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SandboxLayers {
    /// `~/.claude/settings.json`.
    pub user: Option<SandboxConfig>,
    /// `<cwd>/.claude/settings.json`.
    pub project: Option<SandboxConfig>,
    /// `<cwd>/.claude/settings.local.json`.
    pub local: Option<SandboxConfig>,
    /// Claude Code's `--settings`: here the host's settings.
    pub flag: Option<SandboxConfig>,
    /// The organization's managed settings.
    pub managed: Option<SandboxConfig>,
}

impl SandboxConfig {
    /// This tier's paths with relative ones started at `base` (Claude Code
    /// resolves each tier's own: the user's from its config directory, the
    /// others from the working directory).
    #[must_use]
    pub fn rebased(mut self, base: &Path) -> Self {
        let fs = &mut self.filesystem;
        for list in [&mut fs.allow_write, &mut fs.deny_write, &mut fs.deny_read, &mut fs.allow_read] {
            rebase(list, base);
        }
        rebase(&mut self.network.allow_unix_sockets, base);
        for file in self.credentials.iter_mut().flat_map(|c| c.files.iter_mut().flatten()) {
            let mut one = Some(vec![std::mem::take(&mut file.path)]);
            rebase(&mut one, base);
            file.path = one.and_then(|v| v.into_iter().next()).unwrap_or_default();
        }
        self
    }
}

impl SandboxLayers {
    /// The settings one agent's commands run under.
    pub fn effective(&self) -> SandboxConfig {
        let mut out = SandboxConfig::default();
        for tier in [&self.user, &self.project, &self.local, &self.flag, &self.managed].into_iter().flatten() {
            out.merge(tier);
        }
        // Claude Code's `_0`: the tiers a repository cannot write, highest first.
        let trusted: Vec<&SandboxConfig> = [&self.managed, &self.flag, &self.user].into_iter().flatten().collect();
        let union = |pick: fn(&SandboxConfig) -> &Option<Vec<String>>| -> Option<Vec<String>> {
            let mut list = None;
            for tier in &trusted {
                join(&mut list, pick(tier));
            }
            list
        };
        let first = |pick: fn(&SandboxConfig) -> Option<bool>| trusted.iter().find_map(|t| pick(t));
        let managed = self.managed.as_ref();
        let managed_domains_only = managed.and_then(|m| m.network.allow_managed_domains_only) == Some(true);
        // `La`: an administrator's sandbox mandate.
        let unsandboxed = managed
            .and_then(|m| m.allow_unsandboxed_commands)
            .or_else(|| self.flag.as_ref().and_then(|f| f.allow_unsandboxed_commands));
        let mandate = managed_domains_only || unsandboxed == Some(false);
        // `rb`: a strict allowlist counts only from a trusted tier.
        let strict = trusted.iter().any(|t| t.network.strict_allowlist == Some(true));
        out.network.strict_allowlist = strict.then_some(true);

        out.network.allowed_domains = if managed_domains_only {
            managed.and_then(|m| m.network.allowed_domains.clone())
        } else if mandate || strict {
            union(|t| &t.network.allowed_domains)
        } else {
            out.network.allowed_domains
        };
        if managed.and_then(|m| m.filesystem.allow_managed_read_paths_only) == Some(true) {
            out.filesystem.allow_read = managed.and_then(|m| m.filesystem.allow_read.clone());
        } else if mandate {
            out.filesystem.allow_read = self.trusted_in_order(|t| &t.filesystem.allow_read);
        }
        // `mw`: TLS termination from the first trusted tier that sets it.
        out.network.tls_terminate = trusted.iter().find_map(|t| t.network.tls_terminate.clone());
        let tiers = [
            (&self.user, false),
            (&self.project, true),
            (&self.local, true),
            (&self.flag, false),
            (&self.managed, false),
        ];
        let tiers: Vec<credentials::Tier<'_>> = tiers
            .into_iter()
            .filter_map(|(t, repository)| {
                Some(credentials::Tier { credentials: t.as_ref()?.credentials.as_ref()?, repository })
            })
            .collect();
        out.credentials = credentials::effective(&tiers);
        // `fv`: Apple Events only from a trusted tier.
        out.allow_apple_events = first(|t| t.allow_apple_events);
        if mandate {
            out.excluded_commands = union(|t| &t.excluded_commands);
            out.filesystem.allow_write = self.trusted_in_order(|t| &t.filesystem.allow_write);
            out.network.allow_unix_sockets = union(|t| &t.network.allow_unix_sockets);
            out.network.allow_mach_lookup = union(|t| &t.network.allow_mach_lookup);
            // `Kp`: a `false` anywhere stays; otherwise the first trusted value.
            let kept =
                |merged: Option<bool>, trusted: Option<bool>| if merged == Some(false) { merged } else { trusted };
            out.network.allow_all_unix_sockets =
                kept(out.network.allow_all_unix_sockets, first(|t| t.network.allow_all_unix_sockets));
            out.network.allow_local_binding =
                kept(out.network.allow_local_binding, first(|t| t.network.allow_local_binding));
            out.enable_weaker_network_isolation =
                kept(out.enable_weaker_network_isolation, first(|t| t.enable_weaker_network_isolation));
            out.ignore_violations = trusted_violations(&trusted);
        }
        out
    }

    /// A list from the trusted tiers in Claude Code's tier order (user,
    /// `--settings`, managed).
    fn trusted_in_order(&self, pick: fn(&SandboxConfig) -> &Option<Vec<String>>) -> Option<Vec<String>> {
        let mut list = None;
        for tier in [&self.user, &self.flag, &self.managed].into_iter().flatten() {
            join(&mut list, pick(tier));
        }
        list
    }
}

/// `fM`: `ignoreViolations` of the trusted tiers, joined per key.
fn trusted_violations(trusted: &[&SandboxConfig]) -> Option<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for tier in trusted {
        for (key, paths) in tier.ignore_violations.iter().flatten() {
            let mut list = out.remove(key);
            join(&mut list, &Some(paths.clone()));
            out.insert(key.clone(), list.unwrap_or_default());
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sandbox::TlsTerminateConfig;

    fn cfg(v: serde_json::Value) -> Option<SandboxConfig> {
        Some(serde_json::from_value(v).expect("config"))
    }

    #[test]
    fn later_tiers_win_and_lists_join() {
        let layers = SandboxLayers {
            user: cfg(json!({"enabled": false, "network": {"allowedDomains": ["a.com"]}})),
            project: cfg(json!({"enabled": true, "excludedCommands": ["docker *"],
                "network": {"allowedDomains": ["b.com"], "strictAllowlist": true}})),
            local: cfg(json!({"allowAppleEvents": true, "filesystem": {"allowWrite": ["/w/out"]}})),
            ..SandboxLayers::default()
        };
        let eff = layers.effective();
        assert!(eff.enabled());
        assert_eq!(eff.allowed_domains(), ["a.com", "b.com"]);
        assert_eq!(eff.excluded_commands(), ["docker *"]);
        assert_eq!(eff.filesystem.allow_write.as_deref(), Some(&["/w/out".to_owned()][..]));
        assert!(!eff.strict_allowlist(), "a repository's strict allowlist does not count");
        assert_eq!(eff.allow_apple_events, None, "Apple Events only from a trusted tier");
    }

    #[test]
    fn a_trusted_strict_allowlist_drops_repository_domains() {
        let layers = SandboxLayers {
            user: cfg(json!({"network": {"allowedDomains": ["a.com"], "strictAllowlist": true}})),
            project: cfg(json!({"enabled": true, "network": {"allowedDomains": ["b.com"]}})),
            flag: cfg(json!({"network": {"allowedDomains": ["c.com"]}})),
            ..SandboxLayers::default()
        };
        let eff = layers.effective();
        assert!(eff.strict_allowlist());
        assert_eq!(eff.allowed_domains(), ["c.com", "a.com"]);
    }

    #[test]
    fn a_mandate_keeps_repository_grants_out() {
        let layers = SandboxLayers {
            user: cfg(json!({"excludedCommands": ["make"], "filesystem": {"allowWrite": ["/u"]}})),
            project: cfg(json!({"enabled": true, "excludedCommands": ["docker *"],
                "filesystem": {"allowWrite": ["/p"], "denyWrite": ["/d"]},
                "network": {"allowLocalBinding": true, "allowUnixSockets": ["/s"]},
                "ignoreViolations": {"*": ["/x"]}})),
            managed: cfg(json!({"allowUnsandboxedCommands": false})),
            ..SandboxLayers::default()
        };
        let eff = layers.effective();
        assert_eq!(eff.excluded_commands(), ["make"]);
        assert_eq!(eff.filesystem.allow_write.as_deref(), Some(&["/u".to_owned()][..]));
        assert_eq!(eff.filesystem.deny_write.as_deref(), Some(&["/d".to_owned()][..]), "denials always count");
        assert!(!eff.allow_local_binding() && eff.network.allow_unix_sockets.is_none());
        assert!(eff.ignore_violations.is_none());
        assert!(!eff.allow_unsandboxed_commands());
    }

    #[test]
    fn tls_termination_and_credentials_follow_the_trusted_tiers() {
        let layers = SandboxLayers {
            user: cfg(
                json!({"network": {"tlsTerminate": {}}, "credentials": {"envVars": [{"name": "T", "mode": "mask"}]}}),
            ),
            project: cfg(json!({"network": {"tlsTerminate": {"caCertPath": "/p/ca.pem", "caKeyPath": "/p/ca.key"}},
                "credentials": {"files": [{"path": "secrets/key", "mode": "deny"}]}})),
            ..SandboxLayers::default()
        };
        let mut layers = layers;
        layers.project = layers.project.map(|p| p.rebased(Path::new("/w")));
        let eff = layers.effective();
        assert_eq!(eff.network.tls_terminate, Some(TlsTerminateConfig::default()), "a repository's CA is ignored");
        let creds = eff.credentials.expect("credentials");
        assert_eq!(creds.denied_files(), ["/w/secrets/key"]);
        assert_eq!(creds.env_vars.expect("env")[0].name, "T");
        assert!(SandboxLayers::default().effective().credentials.is_none());
    }

    #[test]
    fn managed_only_lists() {
        let layers = SandboxLayers {
            user: cfg(json!({"network": {"allowedDomains": ["a.com"]}, "filesystem": {"allowRead": ["/u"]}})),
            managed: cfg(json!({"network": {"allowManagedDomainsOnly": true, "allowedDomains": ["m.com"]},
                "filesystem": {"allowManagedReadPathsOnly": true, "allowRead": ["/m"]}})),
            ..SandboxLayers::default()
        };
        let eff = layers.effective();
        assert_eq!(eff.allowed_domains(), ["m.com"]);
        assert_eq!(eff.filesystem.allow_read.as_deref(), Some(&["/m".to_owned()][..]));
    }

    #[test]
    fn tiers_resolve_their_own_relative_paths() {
        let tier = cfg(json!({"filesystem": {"allowWrite": ["out", "//abs", "/root", "~/x", "src/**"]}}))
            .expect("tier")
            .rebased(Path::new("/w"));
        assert_eq!(
            tier.filesystem.allow_write.expect("list"),
            ["/w/out", "/abs", "/root", "~/x", "/w/src/**"].map(str::to_owned)
        );
    }
}
