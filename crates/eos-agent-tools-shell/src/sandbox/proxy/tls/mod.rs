//! `sandbox.network.tlsTerminate` (sandbox-runtime's TLS termination): an
//! allowed CONNECT is answered at once; if the client then speaks TLS the
//! proxy finishes the handshake itself with a leaf from its CA (named after
//! the SNI, else the CONNECT host), reads each HTTP/1.1 request, swaps
//! credential placeholders and sends it over its own verified TLS
//! connection to the target. Bytes that are not TLS are tunnelled as they
//! are.

mod body;
pub(crate) mod ca;
#[cfg(test)]
mod end_to_end;
mod forward;
pub(crate) mod watchdog;

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Duration;

use http_body_util::BodyExt as _;
use hyper::body::Body as _;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;

use self::ca::Ca;
use self::forward::{ProxyBody, Tunnel, Upstream};
use self::watchdog::{Activity, Metered, Tracked, Verdict, Watchdog};
use super::{Registration, Shared, filter, relay};

/// TLS-terminated tunnels open at once (`maxTunnels`).
pub(crate) const MAX_TUNNELS: usize = 256;
/// From CONNECT to a finished handshake (`handshakeTimeoutMs`).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Reading the ClientHello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// A stream with bytes already read put back in front.
struct Prefixed<S> {
    head: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Prefixed<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.at < self.head.len() {
            let n = (self.head.len() - self.at).min(buf.remaining());
            let at = self.at;
            buf.put_slice(&self.head[at..at + n]);
            self.at += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Prefixed<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A TLS record header: handshake, version 3.x (`ZC`).
fn looks_like_tls(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 22 && bytes[1] == 3 && bytes[2] <= 3
}

/// Holds one of the proxy's tunnel slots.
struct Slot<'a>(&'a Shared);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.tunnels.fetch_sub(1, Ordering::SeqCst);
    }
}

/// An allowed CONNECT for an agent with TLS termination on.
pub(super) async fn connect(
    shared: &Shared,
    who: &Registration,
    mut client: TcpStream,
    host: String,
    port: u16,
    early: Vec<u8>,
    ca: Arc<Ca>,
) -> io::Result<()> {
    let open = shared.tunnels.fetch_add(1, Ordering::SeqCst);
    let slot = Slot(shared);
    if open >= MAX_TUNNELS {
        if !who.tunnel_refused.swap(true, Ordering::SeqCst) {
            shared.record(
                who,
                &host,
                port,
                &format!(
                    "the sandbox's limit of {MAX_TUNNELS} TLS-terminated tunnels at once is reached; the limit is \
                     network.tlsTerminate.maxTunnels in the sandbox's configuration"
                ),
            );
        }
        let text = "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nX-Proxy-Error: too-many-tunnels\r\n\
                    Connection: close\r\n\r\ntoo many TLS-terminated tunnels at once";
        return client.write_all(text.as_bytes()).await;
    }
    if open < MAX_TUNNELS / 2 {
        who.tunnel_refused.store(false, Ordering::SeqCst);
    }
    client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;

    let mut head = early;
    while head.len() < 3 {
        let mut buf = [0u8; 2048];
        let n = match tokio::time::timeout_at(deadline, client.read(&mut buf)).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return Ok(()),
            Ok(Ok(n)) => n,
        };
        head.extend_from_slice(&buf[..n]);
    }
    if !looks_like_tls(&head) {
        tracing::debug!("[tls-terminate] non-TLS bytes on CONNECT {host}:{port}; opaque-tunnelling");
        drop(slot);
        let Ok(upstream) = shared.dial(who, &host, port).await else { return Ok(()) };
        return relay(client, upstream, &head).await;
    }

    let stream = Prefixed { head, at: 0, inner: client };
    let accept = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
    let hello_by = deadline.min(tokio::time::Instant::now() + HELLO_TIMEOUT);
    let Ok(Ok(start)) = tokio::time::timeout_at(hello_by, accept).await else { return Ok(()) };
    // sandbox-runtime starts the tunnel's timers once the ClientHello is read.
    let timeouts = shared.tls_timeouts();
    let first_by = tokio::time::Instant::now() + timeouts.first_request;
    let target = filter::canonical(&host).trim_end_matches('.').to_ascii_lowercase();
    let name = start.client_hello().server_name().map_or_else(|| target.clone(), str::to_ascii_lowercase);
    let config = ca.server_config(&name)?;
    let Ok(Ok(tls)) = tokio::time::timeout_at(deadline, start.into_stream(config)).await else {
        tracing::debug!("[tls-terminate] TLS handshake not done in time with {host}:{port}");
        return Ok(());
    };

    // A first-request timer that ran out during the handshake started over
    // (sandbox-runtime re-arms it until the handshake is done).
    let mut first_by = first_by;
    while first_by <= tokio::time::Instant::now() {
        first_by += timeouts.first_request;
    }
    let tunnel = Tunnel::new(shared, who, target, port, Upstream::Tls, true);
    serve(tls, &tunnel, true, Watchdog::new(timeouts, first_by)).await;
    drop(slot);
    Ok(())
}

/// A proxied `http://` request whose placeholders are swapped in
/// (`allowPlaintextInject`): the head already read goes back in front.
pub(super) async fn plain(
    shared: &Shared,
    who: &Registration,
    client: TcpStream,
    mut head: Vec<u8>,
    host: String,
    port: u16,
) -> io::Result<()> {
    let stream = Prefixed { head: std::mem::take(&mut head), at: 0, inner: client };
    let tunnel = Tunnel::new(shared, who, filter::canonical(&host).to_ascii_lowercase(), port, Upstream::Plain, true);
    let timeouts = shared.tls_timeouts();
    let first_by = tokio::time::Instant::now() + timeouts.first_request;
    serve(stream, &tunnel, false, Watchdog::new(timeouts, first_by)).await;
    Ok(())
}

/// Serves the tunnel's requests under sandbox-runtime's timeouts.
async fn serve<S>(stream: S, tunnel: &Tunnel<'_>, keep_alive: bool, mut dog: Watchdog)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let activity = Arc::new(Activity::default());
    let io = TokioIo::new(Metered { inner: stream, activity: Arc::clone(&activity) });
    let service = service_fn(|req: http::Request<hyper::body::Incoming>| {
        let activity = Arc::clone(&activity);
        async move {
            let (parts, body) = req.into_parts();
            let (responding, receiving) = activity.started(body.is_end_stream());
            let body: ProxyBody = Tracked::new(body, receiving).boxed();
            let res = tunnel.handle(http::Request::from_parts(parts, body)).await?;
            Ok::<_, io::Error>(res.map(|b| -> ProxyBody { Tracked::new(b, Some(responding)).boxed() }))
        }
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .keep_alive(keep_alive)
        // sandbox-runtime's own timers replace the server's.
        .header_read_timeout(None)
        .serve_connection(io, service);
    let mut conn = std::pin::pin!(conn);
    let verdict = tokio::select! {
        res = conn.as_mut() => return log_end(tunnel, res),
        v = dog.next(&activity) => v,
    };
    if verdict == Verdict::Close {
        conn.as_mut().graceful_shutdown();
        tokio::select! {
            res = conn.as_mut() => log_end(tunnel, res),
            _ = dog.next(&activity) => {}
        }
    }
}

fn log_end(tunnel: &Tunnel<'_>, res: Result<(), hyper::Error>) {
    if let Err(e) = res {
        tracing::debug!("[tls-terminate] {}:{}: {e}", tunnel.host, tunnel.port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_is_recognised_by_its_first_bytes() {
        assert!(looks_like_tls(&[22, 3, 1, 0, 5]));
        assert!(!looks_like_tls(b"GET / HTTP/1.1"));
        assert!(!looks_like_tls(&[22, 3]));
        assert!(!looks_like_tls(&[22, 3, 4]));
    }
}
