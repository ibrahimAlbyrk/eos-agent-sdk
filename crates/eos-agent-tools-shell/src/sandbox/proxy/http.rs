//! The HTTP side: `CONNECT host:port` tunnels and absolute-form requests
//! (`GET http://host/path`), each judged before anything is dialed.

use std::io;

use base64::Engine as _;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use super::{Refusal, Shared, read_head, relay, tls};

/// A parsed request head.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

pub(super) fn parse_head(bytes: &[u8]) -> Option<Head> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?, first.next()?, first.next()?);
    if first.next().is_some() || !version.starts_with("HTTP/1.") || method.is_empty() {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    Some(Head { method: method.into(), target: target.into(), version: version.into(), headers })
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// `Proxy-Authorization: Basic <user:pass>`.
    fn credentials(&self) -> Option<(String, String)> {
        let value = self.header("proxy-authorization")?;
        let (scheme, token) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("basic") {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD.decode(token.trim()).ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (user, pass) = decoded.split_once(':')?;
        Some((user.to_owned(), pass.to_owned()))
    }
}

/// `host:port` (`[v6]:port`) with `default` when no port is given.
pub(super) fn host_port(authority: &str, default: Option<u16>) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if tail.is_empty() => default?,
            None => return None,
        };
        return Some((host.to_owned(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
        None => Some((authority.to_owned(), default?)),
    }
}

/// `http://host[:port]/path?q` → (host, port, `/path?q`).
fn absolute(target: &str) -> Option<(String, u16, String)> {
    let rest = target
        .strip_prefix("http://")
        .or_else(|| target.get(..7).filter(|s| s.eq_ignore_ascii_case("http://")).and_then(|_| target.get(7..)))?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    let authority = authority.rsplit_once('@').map_or(authority, |(_, a)| a);
    let (host, port) = host_port(authority, Some(80))?;
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_owned() };
    Some((host, port, path))
}

pub(super) async fn serve(shared: &Shared, mut client: TcpStream) -> io::Result<()> {
    let (raw, early) = match read_head(&mut client).await {
        Ok(parts) => parts,
        Err(_) => return reply(&mut client, 400, "Bad Request", "malformed request").await,
    };
    let Some(head) = parse_head(&raw) else {
        return reply(&mut client, 400, "Bad Request", "malformed request").await;
    };
    let creds = head.credentials();
    let who = match shared.authenticate(creds.as_ref().map(|(u, p)| (u.as_str(), p.as_str()))) {
        Ok(who) => who,
        Err(_) => {
            let text = "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"sandbox\"\r\n\
                        Content-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            return client.write_all(text.as_bytes()).await;
        }
    };
    let connect = head.method.eq_ignore_ascii_case("CONNECT");
    let target = if connect {
        host_port(&head.target, None).map(|(h, p)| (h, p, String::new()))
    } else {
        absolute(&head.target)
    };
    let Some((host, port, path)) = target else {
        return reply(&mut client, 400, "Bad Request", "the proxy takes CONNECT or absolute http:// requests").await;
    };
    let terminate = connect && who.owner.tls.is_some();
    let inject = !connect && who.owner.plaintext_inject && !who.owner.vault.is_empty();
    if terminate || inject {
        if let Err(reason) = shared.decide(&who, &host, port).await {
            shared.record(&who, &host, port, &reason);
            return forbidden(&mut client, &reason).await;
        }
        if let Some(ca) = who.owner.tls.clone() {
            return tls::connect(shared, &who, client, host, port, early, ca).await;
        }
        let mut head = raw;
        head.extend_from_slice(&early);
        return tls::plain(shared, &who, client, head, host, port).await;
    }
    let upstream = match shared.open(&who, &host, port).await {
        Ok(stream) => stream,
        Err(Refusal::Denied(reason) | Refusal::Address(reason)) => return forbidden(&mut client, &reason).await,
        Err(Refusal::Unauthenticated) => return Ok(()),
        Err(Refusal::Unreachable) => return reply(&mut client, 502, "Bad Gateway", "Bad Gateway").await,
    };
    if connect {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        return relay(client, upstream, &early).await;
    }
    // One request per connection keeps the proxy from parsing bodies.
    let mut out = format!("{} {} {}\r\n", head.method, path, head.version);
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("proxy-") || lower == "connection" || lower == "keep-alive" {
            continue;
        }
        out += &format!("{name}: {value}\r\n");
    }
    out += "Connection: close\r\n\r\n";
    let mut early_out = out.into_bytes();
    early_out.extend_from_slice(&early);
    relay(client, upstream, &early_out).await
}

/// Claude Code's refusal (sandbox-runtime's `$m`).
async fn forbidden(client: &mut TcpStream, reason: &str) -> io::Result<()> {
    let text = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nX-Proxy-Error: blocked-by-sandbox-runtime\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{reason}\n",
        reason.len() + 1
    );
    client.write_all(text.as_bytes()).await
}

async fn reply(client: &mut TcpStream, code: u16, status: &str, body: &str) -> io::Result<()> {
    let text = format!(
        "HTTP/1.1 {code} {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(text.as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heads_and_targets() {
        let head = parse_head(
            b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic dTpw\r\n\r\n",
        )
        .expect("head");
        assert_eq!(head.method, "CONNECT");
        assert_eq!(head.credentials(), Some(("u".into(), "p".into())));
        assert_eq!(host_port("example.com:443", None), Some(("example.com".into(), 443)));
        assert_eq!(host_port("[::1]:8080", None), Some(("::1".into(), 8080)));
        assert_eq!(host_port("example.com", None), None);
        assert_eq!(absolute("http://a.com/x?y=1"), Some(("a.com".into(), 80, "/x?y=1".into())));
        assert_eq!(absolute("http://a.com:81"), Some(("a.com".into(), 81, "/".into())));
        assert_eq!(absolute("https://a.com/"), None);
        assert!(parse_head(b"GARBAGE\r\n\r\n").is_none());
        assert!(parse_head(b"GET / SMTP/1.0\r\n\r\n").is_none());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut rng = fastrand::Rng::with_seed(7);
        for _ in 0..5000 {
            let len = rng.usize(0..200);
            let bytes: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
            if let Some(head) = parse_head(&bytes) {
                let _ = head.credentials();
                let _ = host_port(&head.target, None);
                let _ = absolute(&head.target);
            }
        }
    }
}
