//! An agent's masked credentials: each real value's placeholder
//! (sandbox-runtime's sentinel registry), the environment a sandboxed
//! command gets, and the swap back on the way out.

use std::sync::{Mutex, PoisonError};

use super::{AWS_KEY_ID, AWS_SECRET, AWS_SESSION, CredentialMode, CredentialsConfig, Decode, OnExtractNoMatch, jwt};
use crate::sandbox::proxy::filter;

/// Every placeholder starts so (`Vf`).
pub(crate) const SENTINEL_PREFIX: &str = "fake_value_";
/// Padding characters (`SR`).
const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-";

#[derive(Debug)]
struct Entry {
    key: String,
    sentinel: String,
    real: String,
    hosts: Vec<String>,
}

/// An AWS key pair the proxy re-signs SigV4 requests with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AwsKeys {
    pub key_sentinel: String,
    pub key_id: String,
    pub secret: String,
    pub session_token: Option<String>,
    pub hosts: Vec<String>,
}

/// The environment changes for one command.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EnvPlan {
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

/// One agent's placeholders. A variable keeps its placeholder for the
/// agent's life (sandbox-runtime keeps it for its session); each command
/// refreshes the real value behind it.
#[derive(Debug, Default)]
pub(crate) struct Vault {
    entries: Mutex<Vec<Entry>>,
    pairs: Mutex<Vec<AwsKeys>>,
}

/// `fake_value_<uuid>`, padded to the real value's byte length (`jH`).
fn sentinel_for(real: &str) -> String {
    let mut s = format!("{SENTINEL_PREFIX}{}", uuid::Uuid::new_v4());
    while s.len() < real.len() {
        for b in uuid::Uuid::new_v4().into_bytes() {
            if s.len() < real.len() {
                s.push(char::from(ALPHABET[usize::from(b) % ALPHABET.len()]));
            }
        }
    }
    s
}

impl Vault {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The placeholder for `key`, made on first use from `make`.
    fn register_with(&self, key: &str, real: &str, hosts: &[String], make: impl FnOnce() -> String) -> String {
        let mut entries = self.lock();
        if let Some(e) = entries.iter_mut().find(|e| e.key == key) {
            e.real = real.to_owned();
            e.hosts = hosts.to_vec();
            return e.sentinel.clone();
        }
        let sentinel = make();
        entries.push(Entry {
            key: key.to_owned(),
            sentinel: sentinel.clone(),
            real: real.to_owned(),
            hosts: hosts.to_vec(),
        });
        sentinel
    }

    fn register(&self, key: &str, real: &str, hosts: &[String]) -> String {
        self.register_with(key, real, hosts, || sentinel_for(real))
    }

    fn sentinel(&self, key: &str) -> Option<String> {
        self.lock().iter().find(|e| e.key == key).map(|e| e.sentinel.clone())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// `text` with every placeholder meant for `host` replaced by its real
    /// value (`substituteInString`); `None` when nothing changed.
    pub(crate) fn substitute(&self, text: &str, host: &str) -> Option<String> {
        let entries = self.lock();
        let mut out: Option<String> = None;
        for e in entries.iter() {
            let current = out.as_deref().unwrap_or(text);
            if !current.contains(&e.sentinel) || !e.hosts.iter().any(|p| filter::matches(host, 0, p)) {
                continue;
            }
            out = Some(current.replace(&e.sentinel, &e.real));
        }
        out
    }

    /// The (placeholder, real value) pairs meant for `host`, for bodies.
    pub(crate) fn for_host(&self, host: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let entries = self.lock();
        entries
            .iter()
            .filter(|e| e.hosts.iter().any(|p| filter::matches(host, 0, p)))
            .map(|e| (e.sentinel.clone().into_bytes(), e.real.clone().into_bytes()))
            .collect()
    }

    /// The AWS pair whose access key placeholder is `key_id`, if it may be
    /// used for `host`.
    pub(crate) fn aws_pair(&self, key_id: &str, host: &str) -> Option<AwsKeys> {
        let pairs = self.pairs.lock().unwrap_or_else(PoisonError::into_inner);
        pairs.iter().find(|p| p.key_sentinel == key_id && p.hosts.iter().any(|h| filter::matches(host, 0, h))).cloned()
    }

    /// The environment of one command (`_g`): `deny` entries unset, `mask`
    /// entries replaced by placeholders, read from `env` (the command's
    /// inherited environment). `default_hosts` stand in for an entry
    /// without `injectHosts`. An `Err` stops the command (an extract
    /// pattern with `onExtractNoMatch: "error"`).
    pub(crate) fn plan(
        &self,
        cfg: &CredentialsConfig,
        default_hosts: &[String],
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<EnvPlan, String> {
        let mut plan = EnvPlan::default();
        for e in cfg.env_vars.iter().flatten() {
            if e.mode == CredentialMode::Deny {
                plan.unset.push(e.name.clone());
                continue;
            }
            let Some(value) = env(&e.name) else { continue };
            let hosts = e.inject_hosts.clone().unwrap_or_else(|| default_hosts.to_vec());
            let name = &e.name;
            if e.decode == Some(Decode::Jwt) {
                let fake = if !jwt::is_jwt(&value) {
                    None
                } else if let Some(claims) = &e.mask_claims {
                    let fake = jwt::mask_claims(&value, claims, |claim, real| {
                        self.register(&format!("env:{name}#{claim}"), real, &hosts)
                    });
                    if fake.is_none() {
                        tracing::warn!(
                            "[sandbox-runtime] WARNING: credentials.envVars entry \"{name}\" has maskClaims [{}] but \
                             none is present as a string claim in its JWT value. The variable is left UNPROTECTED \
                             (real value visible as-is inside the sandbox). Fix the config or remove the entry.",
                            claims.join(", ")
                        );
                        continue;
                    }
                    fake.map(|(token, _)| token)
                } else {
                    Some(
                        self.sentinel(&format!("env:{name}"))
                            .unwrap_or_else(|| jwt::fake_token(&uuid::Uuid::new_v4().to_string())),
                    )
                };
                let Some(fake) = fake else {
                    tracing::warn!(
                        "[sandbox-runtime] WARNING: credentials.envVars entry \"{name}\" has decode \"jwt\" but its \
                         value did not verify as a JWT. The variable is left UNPROTECTED (real value visible as-is \
                         inside the sandbox). Fix the config or remove the entry."
                    );
                    continue;
                };
                let fake = self.register_with(&format!("env:{name}"), &value, &hosts, || fake);
                plan.set.push((name.clone(), fake));
                continue;
            }
            if let Some(pattern) = &e.extract {
                match self.extract(name, &value, pattern, &hosts)? {
                    Some(masked) => plan.set.push((name.clone(), masked)),
                    None => match e.on_extract_no_match.unwrap_or(OnExtractNoMatch::Warn) {
                        OnExtractNoMatch::Error => {
                            return Err(format!(
                                "credentials.envVars entry \"{name}\": extract pattern \"{pattern}\" matched nothing \
                                 (onExtractNoMatch: \"error\"). Fix the regex, change to \"warn\"/\"deny\", or remove \
                                 the entry."
                            ));
                        }
                        OnExtractNoMatch::Deny => plan.unset.push(name.clone()),
                        OnExtractNoMatch::Warn => tracing::warn!(
                            "[sandbox-runtime] WARNING: credentials.envVars entry \"{name}\" has extract pattern \
                             \"{pattern}\" that matched nothing in the variable's value. The variable is left \
                             UNPROTECTED (visible as-is inside the sandbox). Fix the regex, set onExtractNoMatch to \
                             \"deny\" or \"error\", or remove the entry."
                        ),
                    },
                }
                continue;
            }
            plan.set.push((name.clone(), self.register(name, &value, &hosts)));
        }
        self.pair_up(cfg, default_hosts, &env);
        Ok(plan)
    }

    /// `extract` (`Km`): group 1 of every match is a credential; equal
    /// values share one placeholder. `None` when nothing matched.
    fn extract(&self, name: &str, value: &str, pattern: &str, hosts: &[String]) -> Result<Option<String>, String> {
        let re = regex::Regex::new(pattern).map_err(|e| format!("extract is not a valid regular expression: {e}"))?;
        let mut spans: Vec<(usize, usize, usize)> = Vec::new();
        let mut values: Vec<&str> = Vec::new();
        for caps in re.captures_iter(value) {
            let whole = caps.get(0).map_or(0, |m| m.start());
            let Some(g) = caps.get(1) else {
                return Err(format!(
                    "extract pattern /{pattern}/ matched at offset {whole} but capture group 1 is undefined \u{2014} \
                     group 1 must capture the credential value on every match."
                ));
            };
            if g.as_str().is_empty() {
                continue;
            }
            let idx = values.iter().position(|v| *v == g.as_str()).unwrap_or_else(|| {
                values.push(g.as_str());
                values.len() - 1
            });
            spans.push((g.start(), g.end(), idx));
        }
        if spans.is_empty() {
            return Ok(None);
        }
        let sentinels: Vec<String> =
            values.iter().enumerate().map(|(i, v)| self.register(&format!("env:{name}#{i}"), v, hosts)).collect();
        let mut out = String::with_capacity(value.len() + 64);
        let mut at = 0;
        for (start, end, idx) in spans {
            out.push_str(&value[at..start]);
            out.push_str(&sentinels[idx]);
            at = end;
        }
        out.push_str(&value[at..]);
        Ok(Some(out))
    }

    /// The AWS pairs (`RR`): the explicit ones, then the conventional trio
    /// unless a pair names one of its variables. Both members must be
    /// whole-value masks present in the environment.
    fn pair_up(&self, cfg: &CredentialsConfig, default_hosts: &[String], env: &impl Fn(&str) -> Option<String>) {
        let whole = |n: &str| cfg.env_vars.iter().flatten().find(|e| e.name == n && e.whole_value_mask());
        let mut pairs = Vec::new();
        let mut explicit: Vec<(String, String, Option<String>)> = cfg
            .aws_pairs
            .iter()
            .flatten()
            .map(|p| (p.access_key_id_var.clone(), p.secret_access_key_var.clone(), p.session_token_var.clone()))
            .collect();
        let named = |n: &str, list: &[(String, String, Option<String>)]| {
            list.iter().any(|(k, s, t)| k == n || s == n || t.as_deref() == Some(n))
        };
        if ![AWS_KEY_ID, AWS_SECRET, AWS_SESSION].iter().any(|n| named(n, &explicit)) {
            match (whole(AWS_KEY_ID).is_some(), whole(AWS_SECRET).is_some()) {
                (true, true) => explicit.push((AWS_KEY_ID.into(), AWS_SECRET.into(), Some(AWS_SESSION.into()))),
                (false, true) if env(AWS_SECRET).is_some() => tracing::warn!(
                    "\"{AWS_SECRET}\" is masked but \"{AWS_KEY_ID}\" is not, so no AWS credential pair was registered. \
                     SigV4 requests will be signed with the masked placeholder secret"
                ),
                _ => {}
            }
        }
        for (key, secret, session) in explicit {
            let (Some(k), Some(_)) = (whole(&key), whole(&secret)) else { continue };
            let (Some(key_id), Some(secret_value)) = (env(&key), env(&secret)) else { continue };
            let Some(key_sentinel) = self.sentinel(&key) else { continue };
            let session_token = session.filter(|s| whole(s).is_some()).and_then(|s| env(&s));
            let hosts = k.inject_hosts.clone().unwrap_or_else(|| default_hosts.to_vec());
            pairs.push(AwsKeys { key_sentinel, key_id, secret: secret_value, session_token, hosts });
        }
        *self.pairs.lock().unwrap_or_else(PoisonError::into_inner) = pairs;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cfg(v: serde_json::Value) -> CredentialsConfig {
        serde_json::from_value(v).expect("credentials")
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn sentinels_look_like_sandbox_runtimes() {
        let short = sentinel_for("abc");
        assert_eq!(short.len(), 47);
        assert!(short.starts_with("fake_value_"));
        let long = sentinel_for(&"x".repeat(80));
        assert_eq!(long.len(), 80);
        assert!(long[47..].bytes().all(|b| ALPHABET.contains(&b)));
    }

    #[test]
    fn deny_unsets_and_mask_swaps_back_only_for_its_hosts() {
        let vault = Vault::default();
        let c = cfg(json!({"envVars": [
            {"name": "GONE", "mode": "deny"},
            {"name": "TOKEN", "mode": "mask", "injectHosts": ["api.github.com"]},
            {"name": "ABSENT", "mode": "mask"},
        ]}));
        let plan = vault.plan(&c, &[], env(&[("TOKEN", "ghp_real"), ("GONE", "x")])).expect("plan");
        assert_eq!(plan.unset, ["GONE"]);
        let [(name, fake)] = plan.set.as_slice() else { panic!("{plan:?}") };
        assert_eq!(name, "TOKEN");
        let header = format!("token {fake}");
        assert_eq!(vault.substitute(&header, "api.github.com").as_deref(), Some("token ghp_real"));
        assert_eq!(vault.substitute(&header, "evil.com"), None, "left as is elsewhere");
        let again = vault.plan(&c, &[], env(&[("TOKEN", "ghp_new")])).expect("plan");
        assert_eq!(&again.set[0].1, fake, "a variable keeps its placeholder");
        assert_eq!(vault.substitute(&header, "api.github.com").as_deref(), Some("token ghp_new"));
    }

    #[test]
    fn extract_masks_only_the_captures() {
        let vault = Vault::default();
        let c = cfg(json!({"envVars": [{"name": "DB", "mode": "mask", "extract": ":([^:@]+)@"}]}));
        let plan = vault.plan(&c, &["db.example.com".into()], env(&[("DB", "postgres://u:pw@db/x")])).expect("plan");
        let masked = &plan.set[0].1;
        assert!(masked.starts_with("postgres://u:fake_value_") && masked.ends_with("@db/x"), "{masked}");
        assert_eq!(vault.substitute(masked, "db.example.com").as_deref(), Some("postgres://u:pw@db/x"));

        let strict = cfg(
            json!({"envVars": [{"name": "DB", "mode": "mask", "extract": "secret=(\\w+)", "onExtractNoMatch": "error"}]}),
        );
        let err = vault.plan(&strict, &[], env(&[("DB", "nothing here")])).expect_err("no match");
        assert!(err.contains("matched nothing (onExtractNoMatch: \"error\")"));
        let deny = cfg(
            json!({"envVars": [{"name": "DB", "mode": "mask", "extract": "secret=(\\w+)", "onExtractNoMatch": "deny"}]}),
        );
        assert_eq!(vault.plan(&deny, &[], env(&[("DB", "x")])).expect("plan").unset, ["DB"]);
        let warn = cfg(json!({"envVars": [{"name": "DB", "mode": "mask", "extract": "secret=(\\w+)"}]}));
        assert_eq!(vault.plan(&warn, &[], env(&[("DB", "x")])).expect("plan"), EnvPlan::default(), "left as is");
    }

    #[test]
    fn jwts_become_fake_tokens() {
        use base64::Engine as _;
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let real = format!("{}.{}.sig", b64(r#"{"alg":"RS256"}"#), b64(r#"{"sub":"me"}"#));
        let vault = Vault::default();
        let c = cfg(
            json!({"envVars": [{"name": "J", "mode": "mask", "decode": "jwt"}, {"name": "K", "mode": "mask", "decode": "jwt"}]}),
        );
        let plan = vault.plan(&c, &["a.com".into()], env(&[("J", &real), ("K", "not-a-jwt")])).expect("plan");
        assert_eq!(plan.set.len(), 1, "a value that is no JWT stays as is");
        let fake = &plan.set[0].1;
        assert!(fake.ends_with(".c3J0LWZha2U"));
        assert_eq!(vault.substitute(&format!("Bearer {fake}"), "a.com"), Some(format!("Bearer {real}")));
    }

    #[test]
    fn aws_pairs() {
        let vault = Vault::default();
        let c = cfg(json!({"envVars": [
            {"name": "AWS_ACCESS_KEY_ID", "mode": "mask", "injectHosts": ["*.amazonaws.com"]},
            {"name": "AWS_SECRET_ACCESS_KEY", "mode": "mask"},
        ]}));
        let plan = vault
            .plan(&c, &[], env(&[("AWS_ACCESS_KEY_ID", "AKIAREAL"), ("AWS_SECRET_ACCESS_KEY", "s3cr3t")]))
            .expect("plan");
        let key = &plan.set[0].1;
        let pair = vault.aws_pair(key, "s3.amazonaws.com").expect("pair");
        assert_eq!((pair.key_id.as_str(), pair.secret.as_str(), pair.session_token), ("AKIAREAL", "s3cr3t", None));
        assert!(vault.aws_pair(key, "evil.com").is_none());
    }
}
