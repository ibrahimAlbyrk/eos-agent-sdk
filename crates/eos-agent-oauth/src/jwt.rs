//! JSON Web Tokens: reading the claims, checking an RS256 signature against
//! the issuer's published keys (JWKS).

use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use base64::Engine as _;
use serde_json::Value;

fn b64(part: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(part.trim_end_matches('=')).ok()
}

/// The claims of a JWT, unverified.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice::<Value>(&b64(payload)?).ok().filter(Value::is_object)
}

/// The claims of `token` once its RS256 signature checks out against a key
/// of `jwks` (`{"keys":[{"kty":"RSA","kid","n","e"},…]}`): the key its
/// header names, or any RSA key when it names none.
pub fn verify_rs256(token: &str, jwks: &Value) -> Result<Value, String> {
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("the ID token is not a JWT".into());
    };
    let header: Value = b64(head).and_then(|h| serde_json::from_slice(&h).ok()).ok_or("unreadable token header")?;
    if header["alg"] != "RS256" {
        return Err(format!("unexpected token algorithm {}", header["alg"]));
    }
    let signature = b64(sig).ok_or("unreadable token signature")?;
    let message = &token.as_bytes()[..head.len() + 1 + body.len()];
    let kid = header["kid"].as_str();
    let keys = jwks["keys"].as_array().ok_or("the issuer published no keys")?;
    let verified = keys
        .iter()
        .filter(|k| k["kty"] == "RSA" && kid.is_none_or(|kid| k["kid"].as_str() == Some(kid)))
        .filter_map(|k| Some(RsaPublicKeyComponents { n: b64(k["n"].as_str()?)?, e: b64(k["e"].as_str()?)? }))
        .any(|key| key.verify(&RSA_PKCS1_2048_8192_SHA256, message, &signature).is_ok());
    if !verified {
        return Err("the ID token's signature does not match the issuer's keys".into());
    }
    jwt_claims(token).ok_or_else(|| "unreadable token claims".into())
}

/// Signing test tokens, for tests of sign-in flows (not for production use).
#[doc(hidden)]
pub mod testing {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{KeyPair, RSA_PKCS1_SHA256, RsaKeyPair};
    use base64::Engine as _;
    use serde_json::{Value, json};

    fn enc(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// A DER INTEGER's content at `at`, and where the next element starts.
    fn der_int(der: &[u8], at: usize) -> (&[u8], usize) {
        assert_eq!(der[at], 0x02, "INTEGER");
        let (len, start) = der_len(der, at + 1);
        let value = &der[start..start + len];
        let value = value.strip_prefix(&[0]).unwrap_or(value);
        (value, start + len)
    }

    fn der_len(der: &[u8], at: usize) -> (usize, usize) {
        if der[at] < 0x80 {
            return (usize::from(der[at]), at + 1);
        }
        let n = usize::from(der[at] & 0x7f);
        (der[at + 1..at + 1 + n].iter().fold(0, |acc, b| (acc << 8) | usize::from(*b)), at + 1 + n)
    }

    /// A signing key and its JWKS entry (`kid` = "k1").
    pub fn test_key() -> (RsaKeyPair, Value) {
        let key = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).expect("key");
        let der = key.public_key().as_ref().to_vec();
        let (_, body) = der_len(&der, 1);
        let (n, next) = der_int(&der, body);
        let (e, _) = der_int(&der, next);
        let jwk = json!({"kty": "RSA", "kid": "k1", "alg": "RS256", "n": enc(n), "e": enc(e)});
        (key, jwk)
    }

    pub fn sign(key: &RsaKeyPair, claims: &Value) -> String {
        let head = enc(br#"{"alg":"RS256","kid":"k1","typ":"JWT"}"#);
        let body = enc(claims.to_string().as_bytes());
        let message = format!("{head}.{body}");
        let mut sig = vec![0; key.public_modulus_len()];
        key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), message.as_bytes(), &mut sig).expect("sign");
        format!("{message}.{}", enc(&sig))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::{sign, test_key};
    use super::*;

    fn enc(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    #[test]
    fn signatures_are_checked_against_the_published_key() {
        let (key, jwk) = test_key();
        let jwks = json!({"keys": [jwk]});
        let token = sign(&key, &json!({"sub": "u1"}));
        assert_eq!(verify_rs256(&token, &jwks).expect("valid")["sub"], "u1");
        let (other, _) = test_key();
        assert!(verify_rs256(&sign(&other, &json!({"sub": "u1"})), &jwks).is_err(), "another key");
        let mut parts: Vec<&str> = token.split('.').collect();
        let forged = enc(br#"{"sub":"u2"}"#);
        parts[1] = &forged;
        assert!(verify_rs256(&parts.join("."), &jwks).is_err(), "changed claims");
        assert!(jwt_claims("not-a-jwt").is_none());
    }
}
