//! sandbox-runtime's timeouts on a terminated tunnel (`rH`, defaults `rR`):
//! the first request must come within 10 s; once the last response is done
//! the tunnel closes after 6 s without a new request (keep-alive 5 s plus a
//! 1 s margin) — gently first, then for good after as long again; and while
//! something is pending (a request still arriving, or the client not taking
//! what is written to it) a tunnel on which no byte moved for 300 s is
//! dropped. Waiting for the upstream's answer is never timed out.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

/// The tunnel's timeouts (sandbox-runtime's `rR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timeouts {
    /// `firstRequestMs`.
    pub first_request: Duration,
    /// `keepAliveMs` + `keepAliveMarginMs`.
    pub keep_alive: Duration,
    /// `stalledMs`: checked in five steps (`gE`).
    pub stalled: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            first_request: Duration::from_secs(10),
            keep_alive: Duration::from_secs(6),
            stalled: Duration::from_secs(300),
        }
    }
}

/// sandbox-runtime's `gE`: the stall check runs this many times per period,
/// and an idle close with bytes still queued is tried again this much sooner.
pub(crate) const STEPS: u32 = 5;

/// What the tunnel's client connection has been doing.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    read: AtomicU64,
    written: AtomicU64,
    /// A write to the client is waiting for it to take bytes.
    write_blocked: AtomicBool,
    /// Requests whose responses are not done.
    in_flight: AtomicUsize,
    /// Requests whose bodies are still arriving.
    receiving: AtomicUsize,
    changed: Notify,
}

impl Activity {
    /// Bytes moved either way.
    pub(crate) fn progress(&self) -> u64 {
        self.read.load(Ordering::Relaxed) + self.written.load(Ordering::Relaxed)
    }

    /// Bytes are waiting to reach the client.
    pub(crate) fn queued(&self) -> bool {
        self.write_blocked.load(Ordering::Relaxed)
    }

    /// Something is pending that the client has to move.
    pub(crate) fn pending(&self) -> bool {
        self.queued() || self.receiving.load(Ordering::SeqCst) > 0
    }

    pub(crate) fn busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) > 0
    }

    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    /// A request arrived (its body still to come unless it has none).
    pub(crate) fn started(self: &Arc<Self>, body_done: bool) -> (Responding, Option<Receiving>) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_one();
        let receiving = (!body_done).then(|| {
            self.receiving.fetch_add(1, Ordering::SeqCst);
            Receiving(Some(Arc::clone(self)))
        });
        (Responding(Some(Arc::clone(self))), receiving)
    }
}

/// Held while a response is not done; dropping it ends the request.
#[derive(Debug)]
pub(crate) struct Responding(Option<Arc<Activity>>);

impl Drop for Responding {
    fn drop(&mut self) {
        if let Some(a) = self.0.take() {
            a.in_flight.fetch_sub(1, Ordering::SeqCst);
            a.changed.notify_one();
        }
    }
}

/// Held while a request's body is still arriving.
#[derive(Debug)]
pub(crate) struct Receiving(Option<Arc<Activity>>);

impl Receiving {
    fn done(&mut self) {
        if let Some(a) = self.0.take() {
            a.receiving.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        self.done();
    }
}

/// A body that holds a guard until it ends (or is dropped).
pub(crate) struct Tracked<B, G> {
    inner: B,
    guard: Option<G>,
}

impl<B, G> Tracked<B, G> {
    pub(crate) fn new(inner: B, guard: Option<G>) -> Self {
        Self { inner, guard }
    }
}

impl<B: Body<Data = Bytes> + Unpin, G: Unpin> Body for Tracked<B, G> {
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let frame = ready!(Pin::new(&mut self.inner).poll_frame(cx));
        if frame.is_none() || self.inner.is_end_stream() {
            self.guard = None;
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The client connection, counted.
pub(crate) struct Metered<S> {
    pub(crate) inner: S,
    pub(crate) activity: Arc<Activity>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Metered<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.activity.read.fetch_add(n as u64, Ordering::Relaxed);
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Metered<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.note(&res);
        res
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        self.note(&res);
        res
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let res = Pin::new(&mut self.inner).poll_flush(cx);
        self.activity.write_blocked.store(res.is_pending(), Ordering::Relaxed);
        res
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl<S> Metered<S> {
    fn note(&self, res: &Poll<io::Result<usize>>) {
        let a = &self.activity;
        a.write_blocked.store(res.is_pending(), Ordering::Relaxed);
        if let Poll::Ready(Ok(n)) = res {
            a.written.fetch_add(*n as u64, Ordering::Relaxed);
        }
    }
}

/// What the watchdog decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Idle: end the connection gently (`n.end()`).
    Close,
    /// Idle again after that, or stalled: drop it.
    Drop,
}

/// sandbox-runtime's timers over one tunnel's [`Activity`].
pub(crate) struct Watchdog {
    t: Timeouts,
    period: Duration,
    idle_at: Option<tokio::time::Instant>,
    ended: bool,
    stall_tick: tokio::time::Interval,
    last: u64,
    still: u32,
}

impl Watchdog {
    /// The first request is due by `first_by`.
    pub(crate) fn new(t: Timeouts, first_by: tokio::time::Instant) -> Self {
        let step = t.stalled / STEPS;
        Self {
            t,
            period: t.first_request,
            idle_at: Some(first_by),
            ended: false,
            stall_tick: tokio::time::interval_at(tokio::time::Instant::now() + step, step),
            last: 0,
            still: 0,
        }
    }

    /// Waits until the connection must be closed (the first idle timeout)
    /// or dropped (the next one, or a stall).
    pub(crate) async fn next(&mut self, activity: &Activity) -> Verdict {
        loop {
            let idle = async {
                match self.idle_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = idle => {
                    if activity.busy() {
                        self.idle_at = None;
                        continue;
                    }
                    if activity.queued() {
                        self.idle_at = Some(arm(activity, self.period));
                        continue;
                    }
                    self.idle_at = Some(arm(activity, self.period));
                    if self.ended {
                        return Verdict::Drop;
                    }
                    tracing::debug!("[tls-terminate] tunnel idle for {} ms; closed", self.period.as_millis());
                    self.ended = true;
                    return Verdict::Close;
                }
                _ = self.stall_tick.tick() => {
                    let now = activity.progress();
                    self.still = if activity.pending() && now == self.last { self.still + 1 } else { 0 };
                    self.last = now;
                    if self.still >= STEPS {
                        tracing::debug!(
                            "[tls-terminate] nothing has moved for {} ms; tunnel closed",
                            self.t.stalled.as_millis()
                        );
                        return Verdict::Drop;
                    }
                    if !activity.busy() && self.idle_at.is_none() {
                        self.rearm(activity);
                    }
                }
                () = activity.changed() => {
                    if activity.busy() {
                        self.idle_at = None;
                    } else if self.idle_at.is_none() {
                        self.rearm(activity);
                    }
                }
            }
        }
    }

    /// The last response is done: the keep-alive wait starts.
    fn rearm(&mut self, activity: &Activity) {
        self.period = self.t.keep_alive;
        self.idle_at = Some(arm(activity, self.period));
    }
}

/// The idle timer's next deadline: `period`, or a fifth of it while bytes
/// are still queued for the client.
fn arm(activity: &Activity, period: Duration) -> tokio::time::Instant {
    let wait = if activity.queued() { period / STEPS } else { period };
    tokio::time::Instant::now() + wait
}
