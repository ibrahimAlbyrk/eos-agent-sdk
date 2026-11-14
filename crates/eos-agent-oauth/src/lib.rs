//! OAuth plumbing the provider adapters share: where secrets are kept (a
//! Keychain item, a 0600 file, memory), the cross-process lock that keeps
//! two processes from spending one single-use refresh token, PKCE and random
//! tokens, the loopback listener a browser sign-in comes back to, form
//! encoding and checking a JWT.

mod callback;
mod jwt;
mod lock;
mod secrets;

use base64::Engine as _;
use sha2::{Digest, Sha256};

pub use callback::{Callback, CallbackListener, CallbackPages, page};
pub use eos_agent_core::SecretStore;
pub use jwt::{jwt_claims, testing, verify_rs256};
pub use lock::{DirLock, lockfile_path};
pub use secrets::{FallbackSecrets, FileSecrets, KeychainSecrets, MemorySecrets};

/// Unix milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// 32 random bytes, base64url without padding (state, nonce, PKCE verifier).
pub fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE `S256` challenge for `verifier`.
pub fn pkce_challenge(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `application/x-www-form-urlencoded`, as `URLSearchParams` writes it.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => (b as char).to_string(),
                b' ' => "+".to_owned(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

/// A token endpoint's answer: the HTTP status and the JSON body (if it was JSON).
#[derive(Debug, Clone)]
pub struct TokenAnswer {
    pub status: u16,
    pub body: Option<serde_json::Value>,
}

impl TokenAnswer {
    /// The OAuth `error` code of a refusal.
    pub fn error_code(&self) -> Option<&str> {
        let body = self.body.as_ref()?;
        body["error"].as_str().or_else(|| body["error"]["code"].as_str()).or_else(|| body["code"].as_str())
    }
}

/// Posts a form to a token endpoint. `Err` is a network failure (worth a
/// retry); any HTTP answer is `Ok`.
pub async fn post_form(http: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> Result<TokenAnswer, String> {
    let res = http
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .body(form_encode(pairs))
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let status = res.status().as_u16();
    let text = res.text().await.map_err(|e| format!("{url}: {e}"))?;
    Ok(TokenAnswer { status, body: serde_json::from_str(&text).ok() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_tokens_are_43_url_safe_chars() {
        let t = random_token().expect("random");
        assert_eq!(t.len(), 43);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(t, random_token().expect("random"));
    }

    #[test]
    fn pkce_matches_rfc_7636_appendix_b() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn forms_encode_like_url_search_params() {
        assert_eq!(form_encode(&[("a", "x y&z"), ("b", "ok-1.~*")]), "a=x+y%26z&b=ok-1.%7E*");
    }
}
