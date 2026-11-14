//! AWS SigV4 re-signing of requests that carry a masked access key
//! (sandbox-runtime's `jC`, `K5`, `YC`): the client signed with the
//! placeholders, so the proxy signs again with the real pair.

use aws_lc_rs::{digest, hmac};

/// Bodies are buffered up to this to hash them (`QC`).
pub(crate) const BUFFER_LIMIT: usize = 64 * 1024 * 1024;
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// What kind of signed request this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Shape {
    /// An `Authorization` header the proxy can sign again.
    Header,
    /// `x-amz-content-sha256: STREAMING-*`: chunk signatures chain off it.
    Streaming,
    /// The signature is in the URL.
    Presigned,
    /// `AWS4-ECDSA-P256-SHA256`: no shared key to sign with.
    Sigv4a,
}

impl Shape {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Header => "header-sigv4",
            Self::Streaming => "streaming",
            Self::Presigned => "presigned",
            Self::Sigv4a => "sigv4a",
        }
    }
}

/// The request's shape and the access key id it names (`jC`).
pub(crate) fn classify(authorization: Option<&str>, content_sha: Option<&str>, query: &str) -> Option<(Shape, String)> {
    if let Some(auth) = authorization {
        let key = credential_key(auth)?;
        if auth.starts_with("AWS4-ECDSA-P256-SHA256") {
            return Some((Shape::Sigv4a, key));
        }
        if auth.starts_with(ALGORITHM) {
            let streaming = content_sha.is_some_and(|c| c.starts_with("STREAMING-"));
            return Some((if streaming { Shape::Streaming } else { Shape::Header }, key));
        }
        return None;
    }
    let params: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let get = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if get("X-Amz-Algorithm") == Some(ALGORITHM) && get("X-Amz-Signature").is_some() {
        let key = get("X-Amz-Credential")?.split('/').next()?.to_owned();
        return Some((Shape::Presigned, key));
    }
    None
}

/// `Credential=<key>/`.
fn credential_key(auth: &str) -> Option<String> {
    let at = auth.find("Credential=")? + "Credential=".len();
    let rest = &auth[at..];
    let end = rest.find(|c: char| c == '/' || c == ',' || c.is_whitespace())?;
    (rest.as_bytes().get(end) == Some(&b'/') && end > 0).then(|| rest[..end].to_owned())
}

/// A parsed `Authorization: AWS4-HMAC-SHA256 …` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signed {
    pub date: String,
    pub region: String,
    pub service: String,
    pub signed_headers: Vec<String>,
}

/// `Credential` with five parts ending in `aws4_request`, `SignedHeaders`
/// and a hex `Signature`; `None` when malformed.
pub(crate) fn parse(auth: &str) -> Option<Signed> {
    let rest = auth.strip_prefix(ALGORITHM)?.trim();
    let mut credential = None;
    let mut headers = None;
    let mut signature = None;
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        match k {
            "Credential" => credential = Some(v),
            "SignedHeaders" => headers = Some(v),
            "Signature" => signature = Some(v),
            _ => {}
        }
    }
    let parts: Vec<&str> = credential?.split('/').collect();
    let [_, date, region, service, "aws4_request"] = parts.as_slice() else { return None };
    let signature = signature?;
    if signature.is_empty() || !signature.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let signed_headers: Vec<String> =
        headers?.split(';').filter(|h| !h.is_empty()).map(str::to_ascii_lowercase).collect();
    if signed_headers.is_empty() {
        return None;
    }
    Some(Signed { date: (*date).into(), region: (*region).into(), service: (*service).into(), signed_headers })
}

/// `encodeURIComponent`, also escaping `!'()*` (`hE`).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `decodeURIComponent`, the text as it was when it does not decode.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok());
            let Some(b) = hex else { return s.to_owned() };
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// The canonical path (`G5`): S3's as sent; others normalized, then encoded.
fn canonical_path(path: &str, service: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    if service == "s3" {
        return path.into();
    }
    let mut segments: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let trailing = if !segments.is_empty() && path.ends_with('/') { "/" } else { "" };
    encode(&format!("/{}{trailing}", segments.join("/"))).replace("%2F", "/")
}

/// The canonical query (`H5`): decoded, encoded again, sorted.
fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (encode(&decode(k)), encode(&decode(v)))
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// A header value trimmed, runs of spaces and tabs as one space (`HC`).
fn tidy(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    for c in v.trim().chars() {
        if c == ' ' || c == '\t' {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of `data`, in hex.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, data).as_ref())
}

fn mac(key: &[u8], data: &str) -> Vec<u8> {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data.as_bytes()).as_ref().to_vec()
}

/// Everything one signature covers.
pub(crate) struct Request<'a> {
    pub method: &'a str,
    /// Path and query as sent.
    pub target: &'a str,
    /// The `Host` value signed (`host[:port]`, no port for 443).
    pub host: &'a str,
    /// A request header's values (lower-case name).
    pub header: &'a dyn Fn(&str) -> Option<Vec<String>>,
    pub signed_headers: &'a [String],
    pub amz_date: &'a str,
    pub scope: &'a Signed,
    pub payload_hash: &'a str,
}

/// The `Authorization` value for `key_id`/`secret` (`YC`).
pub(crate) fn authorization(req: &Request<'_>, key_id: &str, secret: &str) -> Result<String, String> {
    let (path, query) = req.target.split_once('?').unwrap_or((req.target, ""));
    let mut signed: Vec<String> = req.signed_headers.iter().map(|h| h.to_ascii_lowercase()).collect();
    signed.sort();
    let mut lines = Vec::with_capacity(signed.len());
    for name in &signed {
        let value = if name == "host" {
            req.host.to_owned()
        } else {
            let values =
                (req.header)(name).ok_or_else(|| format!("signed header \"{name}\" is missing from the request"))?;
            values.iter().map(|v| tidy(v)).collect::<Vec<_>>().join(",")
        };
        lines.push(format!("{name}:{value}"));
    }
    let s = req.scope;
    let canonical = [
        req.method.to_ascii_uppercase(),
        canonical_path(path, &s.service),
        canonical_query(query),
        lines.join("\n"),
        String::new(),
        signed.join(";"),
        req.payload_hash.to_owned(),
    ]
    .join("\n");
    let scope = format!("{}/{}/{}/aws4_request", s.date, s.region, s.service);
    let to_sign = [ALGORITHM, req.amz_date, &scope, &sha256_hex(canonical.as_bytes())].join("\n");
    let key = mac(&mac(&mac(&mac(format!("AWS4{secret}").as_bytes(), &s.date), &s.region), &s.service), "aws4_request");
    let signature = hex(&mac(&key, &to_sign));
    Ok(format!("{ALGORITHM} Credential={key_id}/{scope}, SignedHeaders={}, Signature={signature}", signed.join(";")))
}

/// The refusal for a shape the policy denies.
pub(crate) fn denied(shape: &Shape) -> String {
    let kind = shape.name();
    format!(
        "AWS SigV4 {kind} request uses a masked credential but cannot be re-signed by the sandbox proxy; denied by \
         policy. Set credentials.sigv4.{kind} to \"passthrough\" to forward it unmodified (it will fail upstream: the \
         signature covers the masked placeholder)."
    )
}

pub(crate) const MALFORMED: &str = "AWS SigV4 request uses a masked credential but its Authorization header is malformed; the proxy cannot re-sign it.";
pub(crate) const NO_DATE: &str =
    "AWS SigV4 request uses a masked credential but carries no x-amz-date header; the proxy cannot re-sign it.";

pub(crate) fn missing(header: &str) -> String {
    format!(
        "AWS SigV4 request uses a masked credential but signed header \"{header}\" is missing; the proxy cannot re-sign it."
    )
}

pub(crate) fn too_large() -> String {
    format!(
        "AWS SigV4 request uses a masked credential and signs a literal body hash, so the proxy must buffer the body \
         to re-sign it, but the body exceeds the {BUFFER_LIMIT}-byte buffering limit; denied. Sign the payload as \
         UNSIGNED-PAYLOAD to stream it without buffering, or use an unmasked credential to have the request \
         forwarded untouched."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes() {
        let auth = "AWS4-HMAC-SHA256 Credential=fake_value_x/20150830/us-east-1/iam/aws4_request, SignedHeaders=host, Signature=ab";
        assert_eq!(classify(Some(auth), None, ""), Some((Shape::Header, "fake_value_x".into())));
        assert_eq!(
            classify(Some(auth), Some("STREAMING-AWS4-HMAC-SHA256-PAYLOAD"), "").map(|s| s.0),
            Some(Shape::Streaming)
        );
        let a = auth.replace("AWS4-HMAC-SHA256", "AWS4-ECDSA-P256-SHA256");
        assert_eq!(classify(Some(&a), None, "").map(|s| s.0), Some(Shape::Sigv4a));
        let q = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=KEY%2F20150830%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Signature=ab";
        assert_eq!(classify(None, None, q), Some((Shape::Presigned, "KEY".into())));
        assert_eq!(classify(None, None, "a=b"), None);
        assert!(parse(auth).is_some());
        assert!(parse("AWS4-HMAC-SHA256 Credential=k/d/r/s/x, SignedHeaders=host, Signature=ab").is_none());
    }

    /// AWS's published example (IAM `ListUsers`, Signature Version 4 test suite).
    #[test]
    fn signs_as_aws_documents() {
        let scope = Signed {
            date: "20150830".into(),
            region: "us-east-1".into(),
            service: "iam".into(),
            signed_headers: vec!["content-type".into(), "host".into(), "x-amz-date".into()],
        };
        let header = |name: &str| match name {
            "content-type" => Some(vec!["application/x-www-form-urlencoded; charset=utf-8".to_owned()]),
            "x-amz-date" => Some(vec!["20150830T123600Z".to_owned()]),
            _ => None,
        };
        let req = Request {
            method: "GET",
            target: "/?Action=ListUsers&Version=2010-05-08",
            host: "iam.amazonaws.com",
            header: &header,
            signed_headers: &scope.signed_headers,
            amz_date: "20150830T123600Z",
            scope: &scope,
            payload_hash: &sha256_hex(b""),
        };
        let auth = authorization(&req, "AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY").expect("signed");
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn paths_and_queries() {
        assert_eq!(canonical_path("/a/./b/../c/", "iam"), "/a/c/");
        assert_eq!(canonical_path("/a b", "iam"), "/a%20b");
        assert_eq!(canonical_path("/a%20b", "iam"), "/a%2520b", "encoded again");
        assert_eq!(canonical_path("/a/../b//", "s3"), "/a/../b//");
        assert_eq!(canonical_query("b=2&a=1&a=0"), "a=0&a=1&b=2");
        assert_eq!(canonical_query("x=a%20b!"), "x=a%20b%21");
    }
}
