//! The proxy's plumbing: relaying bytes, reading a request head, and the
//! machine's own addresses.

use std::io;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::{HEAD_LIMIT, IDLE_TIMEOUT};

/// Copies both ways until both sides finish or nothing moves for
/// `IDLE_TIMEOUT`; `early` (bytes the client sent after its request) goes
/// upstream first.
pub(crate) async fn relay<C, U>(client: C, upstream: U, early: &[u8]) -> io::Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut ur, mut uw) = tokio::io::split(upstream);
    if !early.is_empty() {
        uw.write_all(early).await?;
    }
    let started = Instant::now();
    let last = AtomicU64::new(0);
    let up = pump(&mut cr, &mut uw, &last, started);
    let down = pump(&mut ur, &mut cw, &last, started);
    let _ = tokio::join!(up, down);
    Ok(())
}

async fn pump<R, W>(from: &mut R, to: &mut W, last: &AtomicU64, started: Instant) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; 8192];
    loop {
        match tokio::time::timeout(IDLE_TIMEOUT, from.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(n)) => {
                to.write_all(&buf[..n]).await?;
                last.store(started.elapsed().as_secs(), Ordering::Relaxed);
            }
            // Quiet this way; the other way may still be busy.
            Err(_)
                if started.elapsed().as_secs().saturating_sub(last.load(Ordering::Relaxed))
                    < IDLE_TIMEOUT.as_secs() => {}
            Err(_) => break,
        }
    }
    to.shutdown().await
}

/// The machine's own interface addresses (a name resolving to one of them
/// would reach local services).
pub(super) fn local_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    getifaddrs(&mut out);
    out
}

#[allow(unsafe_code)]
fn getifaddrs(out: &mut Vec<IpAddr>) {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we only read and then
    // free with freeifaddrs; each ifa_addr is checked for null and its
    // family before it is read as the matching sockaddr type.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return;
        }
        let mut cur = head;
        while !cur.is_null() {
            let addr = (*cur).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(addr as *const libc::sockaddr_in);
                        out.push(IpAddr::from(u32::from_be(sin.sin_addr.s_addr).to_be_bytes()));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::from(sin6.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(head);
    }
}

/// Reads until the end of the head (`\r\n\r\n`); returns the head and any
/// bytes after it.
pub(crate) async fn read_head<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > HEAD_LIMIT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request head too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_addresses_include_loopback() {
        assert!(local_addresses().iter().any(IpAddr::is_loopback));
    }

    #[tokio::test]
    async fn head_reader_splits_and_caps() {
        let mut input: &[u8] = b"CONNECT a:1 HTTP/1.1\r\nHost: a\r\n\r\nextra";
        let (head, rest) = read_head(&mut input).await.expect("head");
        assert!(head.ends_with(b"\r\n\r\n"));
        assert_eq!(rest, b"extra");
        let big = vec![b'a'; 20_000];
        assert!(read_head(&mut big.as_slice()).await.is_err());
        assert!(read_head(&mut &b"partial"[..]).await.is_err());
    }
}
