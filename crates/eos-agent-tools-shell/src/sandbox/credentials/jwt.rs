//! `decode: "jwt"`: sandbox-runtime's JWT check (`jm`) and the fake tokens a
//! sandboxed command sees instead (`qm`, `Ym`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde_json::Value;

/// The signature segment of every fake token: `srt-fake`.
const FAKE_SIGNATURE: &str = "c3J0LWZha2U";
/// The fake payload's `exp`.
const FAKE_EXP: u64 = 9_999_999_999;

fn decode_segment(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Three dot-separated parts; the header a JSON object with `alg`, the
/// payload any JSON.
pub(super) fn is_jwt(token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, _] = parts.as_slice() else { return false };
    let Some(header) = decode_segment(header).and_then(|h| serde_json::from_slice::<Value>(&h).ok()) else {
        return false;
    };
    if !header.as_object().is_some_and(|h| h.contains_key("alg")) {
        return false;
    }
    decode_segment(payload).is_some_and(|p| serde_json::from_slice::<Value>(&p).is_ok())
}

/// A whole fake token: `{"alg":"HS256","typ":"JWT"}`, `{"sub":"<sentinel>","exp":9999999999}`.
pub(super) fn fake_token(sentinel_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let sub = Value::String(format!("{}{sentinel_id}", super::vault::SENTINEL_PREFIX));
    let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":{sub},"exp":{FAKE_EXP}}}"#));
    format!("{header}.{payload}.{FAKE_SIGNATURE}")
}

/// The token with the named string claims replaced (`sentinel` makes each
/// claim's placeholder from its real value): the fake token and the
/// replaced claims, `None` when no named claim is a string.
pub(super) fn mask_claims(
    token: &str,
    claims: &[String],
    mut sentinel: impl FnMut(&str, &str) -> String,
) -> Option<(String, Vec<String>)> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = decode_segment(parts.next()?)?;
    let mut fields = ordered_object(&payload)?;
    let mut masked = Vec::new();
    for (key, value) in &mut fields {
        if !claims.contains(key) {
            continue;
        }
        if let Value::String(real) = value {
            *value = Value::String(sentinel(key, real));
            masked.push(key.clone());
        }
    }
    if masked.is_empty() {
        return None;
    }
    // `JSON.stringify` of the object: its own key order.
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{}:{}", Value::String(k.clone()), serde_json::to_string(v).unwrap_or_default()))
        .collect();
    let payload = URL_SAFE_NO_PAD.encode(format!("{{{}}}", body.join(",")));
    Some((format!("{header}.{payload}.{FAKE_SIGNATURE}"), masked))
}

/// A JSON object's members in their order (`serde_json::Map` sorts them).
fn ordered_object(bytes: &[u8]) -> Option<Vec<(String, Value)>> {
    struct Ordered;
    impl<'de> Visitor<'de> for Ordered {
        type Value = Vec<(String, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out: Vec<(String, Value)> = Vec::new();
            while let Some((k, v)) = map.next_entry::<String, Value>()? {
                match out.iter_mut().find(|(o, _)| *o == k) {
                    Some(slot) => slot.1 = v,
                    None => out.push((k, v)),
                }
            }
            Ok(out)
        }
    }
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let out = de.deserialize_map(Ordered).ok()?;
    de.end().ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(header: &str, payload: &str) -> String {
        format!("{}.{}.sig", URL_SAFE_NO_PAD.encode(header), URL_SAFE_NO_PAD.encode(payload))
    }

    #[test]
    fn checks_and_fakes() {
        assert!(is_jwt(&token(r#"{"alg":"RS256"}"#, r#"{"sub":"u"}"#)));
        assert!(!is_jwt(&token(r#"{"typ":"JWT"}"#, r#"{"sub":"u"}"#)), "no alg");
        assert!(!is_jwt("a.b"));
        let fake = fake_token("id");
        assert!(fake.ends_with(".c3J0LWZha2U") && is_jwt(&fake));
        let payload = decode_segment(fake.split('.').nth(1).expect("payload")).expect("b64");
        assert_eq!(String::from_utf8(payload).expect("utf8"), r#"{"sub":"fake_value_id","exp":9999999999}"#);
    }

    #[test]
    fn masks_claims_in_their_order() {
        let real = token(r#"{"alg":"RS256"}"#, r#"{"z":1,"email":"a@b","sub":"u1","n":2}"#);
        let (fake, masked) = mask_claims(&real, &["sub".into(), "n".into()], |k, _| format!("S_{k}")).expect("masked");
        assert_eq!(masked, ["sub"], "only string claims");
        let payload = decode_segment(fake.split('.').nth(1).expect("payload")).expect("b64");
        assert_eq!(String::from_utf8(payload).expect("utf8"), r#"{"z":1,"email":"a@b","sub":"S_sub","n":2}"#);
        assert!(fake.starts_with(real.split('.').next().expect("header")));
        assert!(mask_claims(&real, &["nope".into()], |_, _| String::new()).is_none());
    }
}
