//! The SOCKS5 side (RFC 1928, user/password per RFC 1929): CONNECT only,
//! names resolved by the proxy (`socks5h`), no UDP.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{Refusal, Shared, relay};

const USER_PASS: u8 = 0x02;
const NO_METHOD: u8 = 0xff;

/// The reply codes we send.
const SUCCEEDED: u8 = 0x00;
const NOT_ALLOWED: u8 = 0x02;
const HOST_UNREACHABLE: u8 = 0x04;
const COMMAND_UNSUPPORTED: u8 = 0x07;
const ADDRESS_UNSUPPORTED: u8 = 0x08;

/// A CONNECT request's destination.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    Connect { host: String, port: u16 },
    Unsupported(u8),
}

pub(super) async fn serve(shared: &Shared, mut client: TcpStream) -> io::Result<()> {
    let methods = greeting(&mut client).await?;
    let creds = if methods.contains(&USER_PASS) {
        client.write_all(&[5, USER_PASS]).await?;
        let (user, pass) = user_pass(&mut client).await?;
        Some((user, pass))
    } else {
        return client.write_all(&[5, NO_METHOD]).await;
    };
    let who = match shared.authenticate(creds.as_ref().map(|(u, p)| (u.as_str(), p.as_str()))) {
        Ok(who) => {
            client.write_all(&[1, 0]).await?;
            who
        }
        Err(_) => return client.write_all(&[1, 1]).await,
    };
    let (host, port) = match request(&mut client).await? {
        Request::Connect { host, port } => (host, port),
        Request::Unsupported(code) => return answer(&mut client, code).await,
    };
    match shared.open(&who, &host, port).await {
        Ok(upstream) => {
            answer(&mut client, SUCCEEDED).await?;
            relay(client, upstream, &[]).await
        }
        Err(Refusal::Denied(_) | Refusal::Address(_) | Refusal::Unauthenticated) => {
            answer(&mut client, NOT_ALLOWED).await
        }
        Err(Refusal::Unreachable) => answer(&mut client, HOST_UNREACHABLE).await,
    }
}

async fn greeting<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    r.read_exact(&mut methods).await?;
    Ok(methods)
}

async fn user_pass<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<(String, String)> {
    let mut version = [0u8; 1];
    r.read_exact(&mut version).await?;
    if version[0] != 1 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad auth version"));
    }
    let user = read_string(r).await?;
    let pass = read_string(r).await?;
    Ok((user, pass))
}

async fn read_string<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<String> {
    let mut len = [0u8; 1];
    r.read_exact(&mut len).await?;
    let mut bytes = vec![0u8; usize::from(len[0])];
    r.read_exact(&mut bytes).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) async fn request<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Request> {
    let mut head = [0u8; 4];
    r.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
    }
    let host = match head[3] {
        1 => {
            let mut a = [0u8; 4];
            r.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        3 => read_string(r).await?,
        4 => {
            let mut a = [0u8; 16];
            r.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        _ => return Ok(Request::Unsupported(ADDRESS_UNSUPPORTED)),
    };
    let mut port = [0u8; 2];
    r.read_exact(&mut port).await?;
    if head[1] != 1 {
        return Ok(Request::Unsupported(COMMAND_UNSUPPORTED));
    }
    Ok(Request::Connect { host, port: u16::from_be_bytes(port) })
}

async fn answer(client: &mut TcpStream, code: u8) -> io::Result<()> {
    client.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_requests() {
        let mut domain: &[u8] =
            &[5, 1, 0, 3, 11, b'e', b'x', b'a', b'm', b'p', b'l', b'e', b'.', b'c', b'o', b'm', 1, 187];
        assert_eq!(
            request(&mut domain).await.expect("req"),
            Request::Connect { host: "example.com".into(), port: 443 }
        );
        let mut v4: &[u8] = &[5, 1, 0, 1, 127, 0, 0, 1, 0, 80];
        assert_eq!(request(&mut v4).await.expect("req"), Request::Connect { host: "127.0.0.1".into(), port: 80 });
        let mut bind: &[u8] = &[5, 2, 0, 1, 127, 0, 0, 1, 0, 80];
        assert_eq!(request(&mut bind).await.expect("req"), Request::Unsupported(COMMAND_UNSUPPORTED));
        let mut udp: &[u8] = &[5, 3, 0, 9];
        assert_eq!(request(&mut udp).await.expect("req"), Request::Unsupported(ADDRESS_UNSUPPORTED));
    }

    #[tokio::test]
    async fn random_bytes_never_panic() {
        let mut rng = fastrand::Rng::with_seed(11);
        for _ in 0..3000 {
            let len = rng.usize(0..40);
            let bytes: Vec<u8> = (0..len).map(|_| rng.u8(..)).collect();
            let _ = request(&mut bytes.as_slice()).await;
            let _ = greeting(&mut bytes.as_slice()).await;
            let _ = user_pass(&mut bytes.as_slice()).await;
        }
    }
}
