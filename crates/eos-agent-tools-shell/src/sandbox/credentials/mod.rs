//! Claude Code's `sandbox.credentials` (2.1.319): credential files a
//! sandboxed command may not read, and environment variables it gets unset
//! (`deny`) or replaced by a placeholder the proxy swaps back for the real
//! value on the way out (`mask`), only towards the hosts the entry names.
//!
//! The settings shape and its checks are Claude Code's (`hh`, `Vn`, `Yn`,
//! `Jn`, `fh`); how the tiers combine is its sandbox glue (`J0`); what the
//! runtime does with the result is sandbox-runtime's on macOS, where a file
//! `mask` becomes a `deny`.

mod jwt;
pub(crate) mod sigv4;
pub(crate) mod vault;

use serde::{Deserialize, Serialize};

pub(crate) use self::vault::{EnvPlan, Vault};

/// `sandbox.credentials`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CredentialsConfig {
    pub files: Option<Vec<CredentialFile>>,
    pub env_vars: Option<Vec<CredentialEnv>>,
    /// Swap placeholders on plain-HTTP requests too (trusted tiers only).
    pub allow_plaintext_inject: Option<bool>,
    /// AWS key pairs to re-sign SigV4 requests with (trusted tiers only).
    pub aws_pairs: Option<Vec<AwsPair>>,
    /// What to do with SigV4 requests the proxy cannot re-sign.
    pub sigv4: Option<Sigv4Policies>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CredentialMode {
    Deny,
    Mask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnExtractNoMatch {
    Warn,
    Deny,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decode {
    Jwt,
}

/// One entry of `credentials.files`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialFile {
    pub path: String,
    pub mode: CredentialMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_extract_no_match: Option<OnExtractNoMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<Decode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_claims: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_duplicates: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_hosts: Option<Vec<String>>,
}

/// One entry of `credentials.envVars`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEnv {
    pub name: String,
    pub mode: CredentialMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_extract_no_match: Option<OnExtractNoMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<Decode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_claims: Option<Vec<String>>,
    /// Where the proxy swaps the placeholder; unset: `network.allowedDomains`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_hosts: Option<Vec<String>>,
}

impl CredentialEnv {
    /// A whole-value mask (no `extract`, no `decode`): usable in an AWS pair.
    pub(crate) fn whole_value_mask(&self) -> bool {
        self.mode == CredentialMode::Mask && self.extract.is_none() && self.decode.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwsPair {
    pub access_key_id_var: String,
    pub secret_access_key_var: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token_var: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sigv4Policy {
    Deny,
    Passthrough,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Sigv4Policies {
    pub streaming: Option<Sigv4Policy>,
    pub presigned: Option<Sigv4Policy>,
    pub sigv4a: Option<Sigv4Policy>,
}

/// `sandbox.network.tlsTerminate`: a CA (both paths) or none (an ephemeral
/// one is made).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TlsTerminateConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca_key_path: Option<String>,
}

/// The conventional AWS variables, paired without an `awsPairs` entry.
pub(crate) const AWS_KEY_ID: &str = "AWS_ACCESS_KEY_ID";
pub(crate) const AWS_SECRET: &str = "AWS_SECRET_ACCESS_KEY";
pub(crate) const AWS_SESSION: &str = "AWS_SESSION_TOKEN";

impl CredentialsConfig {
    /// Claude Code's settings checks (`Vn`, `Yn`, `hh`): the first problem.
    pub(crate) fn problem(&self) -> Option<String> {
        for f in self.files.iter().flatten() {
            if f.path.is_empty() {
                return Some("credentials.files: path must not be empty".into());
            }
            if f.mode == CredentialMode::Mask {
                if let Some(p) = f.extract.as_deref().and_then(extract_problem) {
                    return Some(p);
                }
                if let Some(p) = f.mask_claims.as_ref().and_then(|c| claims_problem(c, f.decode)) {
                    return Some(p);
                }
            }
        }
        for e in self.env_vars.iter().flatten() {
            if e.name.is_empty() {
                return Some("credentials.envVars: name must not be empty".into());
            }
            if e.mode != CredentialMode::Mask {
                continue;
            }
            if let Some(p) = e.extract.as_deref().and_then(extract_problem) {
                return Some(p);
            }
            if let Some(p) = e.mask_claims.as_ref().and_then(|c| claims_problem(c, e.decode)) {
                return Some(p);
            }
            if e.decode.is_some() && e.extract.is_some() {
                return Some(
                    "extract cannot be combined with decode on an env entry \u{2014} the runtime takes the decode \
                     path (whole-value JWT verification) and never consults extract, silently disabling the \
                     structured masking. Remove one of the two."
                        .into(),
                );
            }
            if e.decode.is_some()
                && matches!(e.on_extract_no_match, Some(OnExtractNoMatch::Deny | OnExtractNoMatch::Error))
            {
                return Some("onExtractNoMatch cannot be honored on an env entry with decode".into());
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for pair in self.aws_pairs.iter().flatten() {
            let members =
                [Some(&pair.access_key_id_var), Some(&pair.secret_access_key_var), pair.session_token_var.as_ref()];
            let mut mine: Vec<&str> = Vec::new();
            for name in members.into_iter().flatten() {
                if mine.contains(&name.as_str()) {
                    return Some(format!(
                        "awsPairs: a pair names '{name}' twice \u{2014} each pair member must be a distinct variable."
                    ));
                }
                if seen.contains(&name.as_str()) {
                    return Some(format!(
                        "\"{name}\" appears in more than one awsPairs slot (within or across pairs) \u{2014} each variable can fill exactly one slot."
                    ));
                }
                mine.push(name);
            }
            seen.extend(mine);
        }
        None
    }

    /// The deny paths of `files`, a `mask` among them as a `deny` (macOS):
    /// the `deny` entries first, then the masks (sandbox-runtime's `_g`).
    pub(crate) fn denied_files(&self) -> Vec<String> {
        let mut out = self.deny_mode_files();
        for f in self.files.iter().flatten() {
            if !out.contains(&f.path) {
                out.push(f.path.clone());
            }
        }
        out
    }

    /// The paths of the `files` entries whose mode is `deny` (`Ou`).
    pub(crate) fn deny_mode_files(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for f in self.files.iter().flatten().filter(|f| f.mode == CredentialMode::Deny) {
            if !out.contains(&f.path) {
                out.push(f.path.clone());
            }
        }
        out
    }

    /// The `allowPlaintextInject` setting.
    pub(crate) fn plaintext_inject(&self) -> bool {
        self.allow_plaintext_inject.unwrap_or(false)
    }
}

fn extract_problem(pattern: &str) -> Option<String> {
    match regex::Regex::new(pattern) {
        Err(e) => Some(format!("extract is not a valid regular expression: {e}")),
        Ok(re) if re.captures_len() < 2 => Some(
            "extract must contain at least one capturing group \u{2014} group 1 is the credential value to mask \
             (e.g. \"token:\\s*(\\S+)\")."
                .into(),
        ),
        Ok(_) => None,
    }
}

fn claims_problem(claims: &[String], decode: Option<Decode>) -> Option<String> {
    if claims.is_empty() || claims.iter().any(String::is_empty) {
        return Some(
            "maskClaims must name at least one non-empty claim \u{2014} omit maskClaims for whole-token masking."
                .into(),
        );
    }
    decode.is_none().then(|| {
        "maskClaims requires decode \u{2014} without a decode format there is no token to read claims from. Set \
         decode, or omit maskClaims."
            .into()
    })
}

/// One settings tier's `credentials`, and whether a repository can write it
/// (`.claude/settings.json`, `.claude/settings.local.json`).
pub(crate) struct Tier<'a> {
    pub credentials: &'a CredentialsConfig,
    pub repository: bool,
}

/// Claude Code's combination of the tiers (user, project, local, host,
/// managed; `J0`): every file entry counts (a `mask` as a `deny` on macOS);
/// an env `deny` from any tier wins over a `mask`, a later `mask` replaces an
/// earlier one, and a repository's `mask` is left out (it could send a real
/// credential where the repository says); `allowPlaintextInject`,
/// `awsPairs` and `sigv4` count only from the tiers a repository cannot write.
pub(crate) fn effective(tiers: &[Tier<'_>]) -> Option<CredentialsConfig> {
    if tiers.is_empty() {
        return None;
    }
    let mut files: Vec<CredentialFile> = Vec::new();
    let mut env: Vec<CredentialEnv> = Vec::new();
    let mut out = CredentialsConfig::default();
    let mut pairs: Vec<AwsPair> = Vec::new();
    for tier in tiers {
        let c = tier.credentials;
        for f in c.files.iter().flatten() {
            if f.mode == CredentialMode::Mask {
                tracing::warn!(
                    "[sandbox] credential file mask for '{}' degrades to deny on macos (file masking is not enforced \
                     by this platform's sandbox backend yet)",
                    f.path
                );
            }
            if !files.iter().any(|g| g.path == f.path) {
                // A mask keeps its mode (only `deny` entries spare the writable defaults, `RI`).
                files.push(CredentialFile {
                    path: f.path.clone(),
                    mode: f.mode,
                    extract: None,
                    on_extract_no_match: None,
                    decode: None,
                    mask_claims: None,
                    mask_duplicates: None,
                    inject_hosts: None,
                });
            }
        }
        for e in c.env_vars.iter().flatten() {
            if e.mode == CredentialMode::Mask && tier.repository {
                continue;
            }
            let at = env.iter().position(|x| x.name == e.name);
            if at.is_some_and(|i| env[i].mode == CredentialMode::Deny) {
                continue;
            }
            let entry = if e.mode == CredentialMode::Deny {
                CredentialEnv {
                    name: e.name.clone(),
                    mode: CredentialMode::Deny,
                    extract: None,
                    on_extract_no_match: None,
                    decode: None,
                    mask_claims: None,
                    inject_hosts: None,
                }
            } else {
                e.clone()
            };
            match at {
                Some(i) => env[i] = entry,
                None => env.push(entry),
            }
        }
        if tier.repository {
            continue;
        }
        if c.allow_plaintext_inject.is_some() {
            out.allow_plaintext_inject = c.allow_plaintext_inject;
        }
        for pair in c.aws_pairs.iter().flatten() {
            if let Some(i) = pairs.iter().position(|p| p.access_key_id_var == pair.access_key_id_var) {
                tracing::warn!(
                    "[sandbox] credentials.awsPairs entry for '{}' replaces an earlier pair for the same access key \
                     id (later-processed entry wins)",
                    pair.access_key_id_var
                );
                pairs.remove(i);
            }
            pairs.push(pair.clone());
        }
        if c.sigv4.is_some() {
            out.sigv4 = c.sigv4;
        }
    }
    let whole = |name: &str| env.iter().any(|e| e.name == name && e.whole_value_mask());
    pairs.retain(|p| {
        let missing: Vec<&str> = [&p.access_key_id_var, &p.secret_access_key_var]
            .into_iter()
            .map(String::as_str)
            .filter(|n| !whole(n))
            .collect();
        if missing.is_empty() {
            return true;
        }
        tracing::warn!(
            "[sandbox] dropping credentials.awsPairs entry '{}': referenced env var(s) {} are not forwarded as \
             whole-value mask entries (dropped by trust filtering, deny-overridden, never declared, or using \
             extract/decode)",
            p.access_key_id_var,
            missing.join(", ")
        );
        false
    });
    out.files = (!files.is_empty()).then_some(files);
    out.env_vars = (!env.is_empty()).then_some(env);
    out.aws_pairs = (!pairs.is_empty()).then_some(pairs);
    Some(out)
}

/// Claude Code's warning about masks that cannot work (`zue`), if any.
pub(crate) fn mask_warning(c: &CredentialsConfig, tls: bool) -> Option<String> {
    let masks = || c.env_vars.iter().flatten().filter(|e| e.mode == CredentialMode::Mask);
    let empty: Vec<&str> =
        masks().filter(|e| e.inject_hosts.as_ref().is_some_and(Vec::is_empty)).map(|e| e.name.as_str()).collect();
    let injecting: Vec<&str> =
        masks().filter(|e| !e.inject_hosts.as_ref().is_some_and(Vec::is_empty)).map(|e| e.name.as_str()).collect();
    let mut parts = Vec::new();
    if !empty.is_empty() {
        parts.push(format!(
            "sandbox.credentials mask entries ({}) have an empty injectHosts \u{2014} sandboxed commands see only a \
             sentinel and the proxy never substitutes the real credential, so tools needing these will fail to \
             authenticate. If the adapter forced this (a filesystem.allowRead entry re-opened a denied credential \
             path), remove the conflicting allowRead entry or the deny; if a parent/managed settings tier supplied \
             this mask, sentinel-only is its intended posture (that channel cannot grant injection, so an \
             injectHosts set there is stripped on load) and the entry can only be removed in the parent settings; \
             otherwise set injectHosts or remove the entry",
            empty.join(", ")
        ));
    }
    if !injecting.is_empty() && !tls && !c.plaintext_inject() {
        parts.push(format!(
            "sandbox.credentials mask entries ({}) are configured but TLS termination is unavailable \u{2014} \
             sandboxed commands see only a sentinel value and the proxy cannot substitute the real credential on \
             egress, so tools needing these will fail to authenticate. Enable sandbox.network.tlsTerminate, or \
             remove the mask entries",
            injecting.join(", ")
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn creds(v: serde_json::Value) -> CredentialsConfig {
        serde_json::from_value(v).expect("credentials")
    }

    #[test]
    fn claude_codes_checks() {
        let ok = creds(json!({"envVars": [{"name": "T", "mode": "mask", "extract": "token=(\\w+)"}]}));
        assert_eq!(ok.problem(), None);
        let no_group = creds(json!({"envVars": [{"name": "T", "mode": "mask", "extract": "token=\\w+"}]}));
        assert!(no_group.problem().expect("problem").starts_with("extract must contain at least one capturing group"));
        let both = creds(json!({"envVars": [{"name": "T", "mode": "mask", "extract": "(x)", "decode": "jwt"}]}));
        assert!(both.problem().expect("problem").starts_with("extract cannot be combined with decode"));
        let claims = creds(json!({"envVars": [{"name": "T", "mode": "mask", "maskClaims": ["sub"]}]}));
        assert!(claims.problem().expect("problem").starts_with("maskClaims requires decode"));
        let twice = creds(json!({"awsPairs": [
            {"accessKeyIdVar": "A", "secretAccessKeyVar": "S"}, {"accessKeyIdVar": "B", "secretAccessKeyVar": "S"}]}));
        assert!(twice.problem().expect("problem").contains("more than one awsPairs slot"));
        let deny = creds(json!({"envVars": [{"name": "T", "mode": "deny", "extract": "no group"}]}));
        assert_eq!(deny.problem(), None, "a deny's mask options are ignored");
    }

    #[test]
    fn tiers_combine_as_claude_code_combines_them() {
        let user = creds(json!({
            "envVars": [{"name": "GH", "mode": "mask", "injectHosts": ["api.github.com"]}, {"name": "X", "mode": "mask"}],
            "allowPlaintextInject": true,
            "files": [{"path": "/u/.netrc", "mode": "mask"}],
        }));
        let project = creds(json!({
            "envVars": [{"name": "GH", "mode": "mask", "injectHosts": ["evil.com"]}, {"name": "X", "mode": "deny"}],
            "allowPlaintextInject": false,
            "files": [{"path": "/p/key", "mode": "deny"}],
        }));
        let out = effective(&[
            Tier { credentials: &user, repository: false },
            Tier { credentials: &project, repository: true },
        ])
        .expect("credentials");
        let env = out.env_vars.clone().expect("env");
        assert_eq!(
            env[0].inject_hosts.as_deref(),
            Some(&["api.github.com".to_owned()][..]),
            "a repository's mask is left out"
        );
        assert_eq!(env[1].mode, CredentialMode::Deny, "a deny from any tier wins");
        assert!(out.allow_plaintext_inject == Some(true), "only from trusted tiers");
        assert_eq!(out.denied_files(), ["/p/key", "/u/.netrc"], "a file mask is a deny on macOS, after the denies");
        assert_eq!(out.deny_mode_files(), ["/p/key"], "a mask is not a deny entry");
    }

    #[test]
    fn pairs_need_whole_value_masks() {
        let user = creds(json!({
            "envVars": [{"name": "K", "mode": "mask"}, {"name": "S", "mode": "mask", "extract": "(.*)"}],
            "awsPairs": [{"accessKeyIdVar": "K", "secretAccessKeyVar": "S"}],
        }));
        let out = effective(&[Tier { credentials: &user, repository: false }]).expect("credentials");
        assert!(out.aws_pairs.is_none());
    }

    #[test]
    fn the_mask_warning() {
        let c = creds(
            json!({"envVars": [{"name": "A", "mode": "mask"}, {"name": "B", "mode": "mask", "injectHosts": []}]}),
        );
        let text = mask_warning(&c, false).expect("warning");
        assert!(text.starts_with("sandbox.credentials mask entries (B) have an empty injectHosts"));
        assert!(text.contains(
            " \u{b7} sandbox.credentials mask entries (A) are configured but TLS termination is unavailable"
        ));
        assert!(mask_warning(&c, true).expect("warning").contains("(B)"));
    }
}
