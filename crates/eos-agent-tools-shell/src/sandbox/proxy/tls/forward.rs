//! One request through a terminated tunnel (sandbox-runtime's `uH`), or a
//! plain-HTTP request whose credentials may be swapped in
//! (`allowPlaintextInject`): hop-by-hop headers dropped, `Host` set to the
//! target, placeholders swapped for the hosts they are meant for, then sent
//! over the tunnel's one upstream connection.

use std::convert::Infallible;
use std::io;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{
    CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, HOST, HeaderMap, HeaderValue, TRANSFER_ENCODING, UPGRADE,
};
use http::{Method, Request, Response, StatusCode, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use tokio::sync::Mutex;

use super::super::{Refusal, Registration, Shared, filter};
use super::body::Substituted;
use crate::sandbox::Sigv4Policy;
use crate::sandbox::credentials::sigv4::{self, Shape};
use crate::sandbox::credentials::vault::AwsKeys;

pub(crate) type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// The headers that describe one hop only (`$pt`).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Methods whose bodies are left alone unless they declare one (`dE`).
const BODILESS: &[Method] = &[Method::GET, Method::HEAD, Method::OPTIONS];

/// What a SigV4 request is signed again with.
struct Resign {
    keys: AwsKeys,
    signed: sigv4::Signed,
    amz_date: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Upstream {
    /// TLS to the target, verified against the default roots.
    Tls,
    /// Plain TCP (a proxied `http://` request).
    Plain,
}

/// One client connection's target and upstream connection.
pub(crate) struct Tunnel<'a> {
    pub shared: &'a Shared,
    pub who: &'a Registration,
    pub host: String,
    pub port: u16,
    pub upstream: Upstream,
    /// Placeholders are swapped on this connection.
    pub inject: bool,
    pub sender: Mutex<Option<SendRequest<ProxyBody>>>,
}

fn full(text: impl Into<Bytes>) -> ProxyBody {
    Full::new(text.into()).map_err(|never: Infallible| match never {}).boxed()
}

/// sandbox-runtime's refusal inside a tunnel (`zs`).
pub(crate) fn refusal(status: u16, reason: &str) -> Response<ProxyBody> {
    let mut res = Response::new(full(format!("{reason}\n")));
    *res.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN);
    res.headers_mut().insert("content-type", HeaderValue::from_static("text/plain"));
    res.headers_mut().insert("x-proxy-error", HeaderValue::from_static("blocked-by-sandbox-runtime"));
    res
}

fn bad_gateway() -> Response<ProxyBody> {
    let mut res = Response::new(full("Bad Gateway"));
    *res.status_mut() = StatusCode::BAD_GATEWAY;
    res.headers_mut().insert("content-type", HeaderValue::from_static("text/plain"));
    res
}

/// Drops the hop-by-hop headers and those `Connection` names.
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<String> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    for name in HOP_BY_HOP.iter().copied().chain(listed.iter().map(String::as_str)) {
        headers.remove(name);
    }
}

/// `host[:port]`, the port left out when it is the scheme's default.
fn authority(host: &str, port: u16, default: u16) -> String {
    let host = filter::canonical(host);
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    if port == default { host } else { format!("{host}:{port}") }
}

impl Tunnel<'_> {
    pub(crate) fn new<'a>(
        shared: &'a Shared,
        who: &'a Registration,
        host: String,
        port: u16,
        upstream: Upstream,
        inject: bool,
    ) -> Tunnel<'a> {
        Tunnel { shared, who, host, port, upstream, inject, sender: Mutex::new(None) }
    }

    /// The answer to one request; `Err` closes the client connection.
    pub(crate) async fn handle(&self, req: Request<ProxyBody>) -> io::Result<Response<ProxyBody>> {
        if req.version() == Version::HTTP_11 && !req.headers().contains_key(HOST) {
            return Ok(refusal(400, "HTTP/1.1 request without a Host header"));
        }
        let wants_upgrade = req.headers().contains_key(UPGRADE)
            && req
                .headers()
                .get_all(CONNECTION)
                .iter()
                .any(|v| v.to_str().is_ok_and(|v| v.to_ascii_lowercase().contains("upgrade")));
        if wants_upgrade {
            tracing::debug!("[tls-terminate] upgrade request refused");
            return Err(io::Error::other("upgrade request refused"));
        }
        let (mut parts, body) = req.into_parts();
        let resign = match self.sigv4_plan(&parts.headers, parts.uri.query().unwrap_or("")) {
            Ok(plan) => plan,
            Err(reason) => return Ok(refusal(403, &reason)),
        };
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        parts.uri = target.parse().map_err(io::Error::other)?;
        parts.version = Version::HTTP_11;
        let declared = parts.headers.contains_key(CONTENT_LENGTH) || parts.headers.contains_key(TRANSFER_ENCODING);
        strip_hop_by_hop(&mut parts.headers);
        let default_port = if self.upstream == Upstream::Tls { 443 } else { 80 };
        let host_value = authority(&self.host, self.port, default_port);
        parts.headers.insert(HOST, HeaderValue::from_str(&host_value).map_err(io::Error::other)?);

        let vault = &self.who.owner.vault;
        let mut body = body;
        if self.inject && !vault.is_empty() {
            let mut swapped = Vec::new();
            for (name, value) in &parts.headers {
                if let Some(text) = value.to_str().ok().and_then(|v| vault.substitute(v, &self.host))
                    && let Ok(v) = HeaderValue::from_str(&text)
                {
                    swapped.push((name.clone(), v));
                }
            }
            for (name, value) in swapped {
                parts.headers.insert(name, value);
            }
            let subs = vault.for_host(&self.host);
            if !subs.is_empty() && !(BODILESS.contains(&parts.method) && !declared) {
                if parts.headers.contains_key(CONTENT_ENCODING) {
                    tracing::warn!(
                        "[body-substitution] {}: request body has Content-Encoding; substitution skipped",
                        self.host
                    );
                } else {
                    if subs.iter().any(|(s, r)| s.len() != r.len()) {
                        parts.headers.remove(CONTENT_LENGTH);
                    }
                    body = Substituted::new(body, subs).boxed();
                }
            }
        }
        if let Some(plan) = resign {
            match self.resign(&mut parts, body, plan).await {
                Ok(signed) => body = signed,
                Err(reason) => return Ok(refusal(403, &reason)),
            }
        }
        if declared && !parts.headers.contains_key(CONTENT_LENGTH) {
            parts.headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        }
        let req = Request::from_parts(parts, body);
        self.send(req).await
    }

    /// A SigV4 request signed with a masked key (TLS only, as in
    /// sandbox-runtime): `Ok(Some)` to sign again, `Err` to refuse it.
    fn sigv4_plan(&self, headers: &HeaderMap, query: &str) -> Result<Option<Resign>, String> {
        let vault = &self.who.owner.vault;
        if self.upstream != Upstream::Tls || vault.is_empty() {
            return Ok(None);
        }
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let auth = text("authorization");
        let Some((shape, key)) = sigv4::classify(auth, text("x-amz-content-sha256"), query) else { return Ok(None) };
        let Some(keys) = vault.aws_pair(&key, &self.host) else { return Ok(None) };
        let policies = &self.who.owner.sigv4;
        let policy = match shape {
            Shape::Header => None,
            Shape::Streaming => Some(policies.streaming),
            Shape::Presigned => Some(policies.presigned),
            Shape::Sigv4a => Some(policies.sigv4a),
        };
        if let Some(policy) = policy {
            if policy == Some(Sigv4Policy::Passthrough) {
                tracing::debug!("[sigv4] {} request to {} forwarded unmodified (passthrough)", shape.name(), self.host);
                return Ok(None);
            }
            return Err(sigv4::denied(&shape));
        }
        let signed = auth.and_then(sigv4::parse).ok_or_else(|| sigv4::MALFORMED.to_owned())?;
        let amz_date = text("x-amz-date").ok_or_else(|| sigv4::NO_DATE.to_owned())?.to_owned();
        for name in &signed.signed_headers {
            let ours = name == "host" || (name == "x-amz-security-token" && keys.session_token.is_some());
            if !ours && !headers.contains_key(name.as_str()) {
                return Err(sigv4::missing(name));
            }
        }
        Ok(Some(Resign { keys, signed, amz_date }))
    }

    /// Signs the request again with the real pair, hashing the body unless
    /// it is sent as `UNSIGNED-PAYLOAD`.
    async fn resign(
        &self,
        parts: &mut http::request::Parts,
        body: ProxyBody,
        plan: Resign,
    ) -> Result<ProxyBody, String> {
        let Resign { keys, mut signed, amz_date } = plan;
        let headers = &mut parts.headers;
        if let Some(token) = &keys.session_token {
            let value = HeaderValue::from_str(token).map_err(|e| format!("AWS SigV4 re-signing failed: {e}"))?;
            headers.insert("x-amz-security-token", value);
            if !signed.signed_headers.iter().any(|h| h == "x-amz-security-token") {
                signed.signed_headers.push("x-amz-security-token".into());
            }
        }
        let unsigned = headers.get("x-amz-content-sha256").is_some_and(|v| v == "UNSIGNED-PAYLOAD");
        let (hash, body) = if unsigned {
            ("UNSIGNED-PAYLOAD".to_owned(), body)
        } else {
            let mut body = body;
            let mut buf: Vec<u8> = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| format!("AWS SigV4 re-signing failed: {e}"))?;
                if let Ok(data) = frame.into_data() {
                    buf.extend_from_slice(&data);
                    if buf.len() > sigv4::BUFFER_LIMIT {
                        return Err(sigv4::too_large());
                    }
                }
            }
            headers.insert(CONTENT_LENGTH, HeaderValue::from(buf.len()));
            headers.remove(TRANSFER_ENCODING);
            (sigv4::sha256_hex(&buf), full(buf))
        };
        if !unsigned && headers.contains_key("x-amz-content-sha256") {
            headers.insert("x-amz-content-sha256", HeaderValue::from_str(&hash).map_err(|e| e.to_string())?);
        }
        let host = authority(&self.host, self.port, 443);
        let snapshot = headers.clone();
        let lookup = move |name: &str| -> Option<Vec<String>> {
            let values: Vec<String> =
                snapshot.get_all(name).iter().filter_map(|v| v.to_str().ok()).map(str::to_owned).collect();
            (!values.is_empty()).then_some(values)
        };
        let target = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
        let req = sigv4::Request {
            method: parts.method.as_str(),
            target: &target,
            host: &host,
            header: &lookup,
            signed_headers: &signed.signed_headers,
            amz_date: &amz_date,
            scope: &signed,
            payload_hash: &hash,
        };
        let auth = sigv4::authorization(&req, &keys.key_id, &keys.secret)
            .map_err(|e| format!("AWS SigV4 re-signing failed: {e}"))?;
        parts.headers.insert("authorization", HeaderValue::from_str(&auth).map_err(|e| e.to_string())?);
        Ok(body)
    }

    async fn send(&self, req: Request<ProxyBody>) -> io::Result<Response<ProxyBody>> {
        let mut slot = self.sender.lock().await;
        let ready = match slot.as_mut() {
            Some(sender) => sender.ready().await.is_ok(),
            None => false,
        };
        let reused = ready;
        if !ready {
            *slot = None;
            match self.connect().await {
                Ok(sender) => *slot = Some(sender),
                Err(answer) => return Ok(*answer),
            }
        }
        let sender = slot.as_mut().expect("connected");
        match sender.send_request(req).await {
            Ok(res) => {
                let (mut parts, body) = res.into_parts();
                strip_hop_by_hop(&mut parts.headers);
                Ok(Response::from_parts(parts, body.boxed()))
            }
            Err(e) => {
                *slot = None;
                if reused && e.is_incomplete_message() {
                    // The upstream closed a kept-alive connection: let the client retry.
                    return Err(io::Error::other(e));
                }
                Ok(bad_gateway())
            }
        }
    }

    /// The tunnel's upstream connection, through the address guard.
    async fn connect(&self) -> Result<SendRequest<ProxyBody>, Box<Response<ProxyBody>>> {
        let tcp = match self.shared.dial(self.who, &self.host, self.port).await {
            Ok(tcp) => tcp,
            Err(Refusal::Address(reason) | Refusal::Denied(reason)) => return Err(Box::new(refusal(403, &reason))),
            Err(Refusal::Unauthenticated | Refusal::Unreachable) => return Err(Box::new(bad_gateway())),
        };
        let handshake = match self.upstream {
            Upstream::Plain => hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.map(|(s, c)| {
                tokio::spawn(c);
                s
            }),
            Upstream::Tls => {
                let name = ServerName::try_from(filter::canonical(&self.host)).map_err(|_| Box::new(bad_gateway()))?;
                let connector = tokio_rustls::TlsConnector::from(Arc::clone(self.shared.upstream_tls()));
                let tls = connector.connect(name, tcp).await.map_err(|e| {
                    tracing::debug!("[tls-terminate] upstream {}:{}: {e}", self.host, self.port);
                    Box::new(bad_gateway())
                })?;
                hyper::client::conn::http1::handshake(TokioIo::new(tls)).await.map(|(s, c)| {
                    tokio::spawn(c);
                    s
                })
            }
        };
        handshake.map_err(|_| Box::new(bad_gateway()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_and_authority() {
        let mut h = HeaderMap::new();
        h.insert("connection", HeaderValue::from_static("keep-alive, x-secret"));
        h.insert("x-secret", HeaderValue::from_static("1"));
        h.insert("proxy-authorization", HeaderValue::from_static("Basic x"));
        h.insert("accept", HeaderValue::from_static("*/*"));
        strip_hop_by_hop(&mut h);
        assert_eq!(h.len(), 1);
        assert_eq!(authority("api.x.com", 443, 443), "api.x.com");
        assert_eq!(authority("api.x.com", 8443, 443), "api.x.com:8443");
        assert_eq!(authority("::1", 443, 443), "[::1]");
    }
}
