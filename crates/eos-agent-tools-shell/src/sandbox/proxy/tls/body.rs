//! A request body with credential placeholders swapped for their real values
//! as it streams (sandbox-runtime's `P5`): a tail shorter than the longest
//! placeholder is held back between chunks, so one split across two chunks
//! is still found.

use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

pub(crate) struct Substituted<B> {
    inner: B,
    subs: Vec<(Vec<u8>, Vec<u8>)>,
    longest: usize,
    held: Vec<u8>,
    /// Trailers that came while held bytes were still to go out.
    trailers: Option<Frame<Bytes>>,
    flushed: bool,
    /// The body keeps its length (every placeholder as long as its value).
    same_length: bool,
}

impl<B> Substituted<B> {
    pub(crate) fn new(inner: B, subs: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        let longest = subs.iter().map(|(s, _)| s.len()).max().unwrap_or(1);
        let same_length = subs.iter().all(|(s, r)| s.len() == r.len());
        Self { inner, subs, longest, held: Vec::new(), trailers: None, flushed: false, same_length }
    }

    /// Swaps every whole placeholder in `buf`; with `more` coming, keeps the
    /// last `longest - 1` bytes (and any placeholder that may start there).
    fn run(&mut self, buf: &[u8], more: bool) -> Vec<u8> {
        let emit_upto = if more { buf.len().saturating_sub(self.longest - 1) } else { buf.len() };
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        'scan: while i < buf.len() {
            for (sentinel, real) in &self.subs {
                if buf[i..].starts_with(sentinel) {
                    out.extend_from_slice(real);
                    i += sentinel.len();
                    continue 'scan;
                }
            }
            if i >= emit_upto {
                break;
            }
            out.push(buf[i]);
            i += 1;
        }
        self.held = buf[i..].to_vec();
        out
    }
}

impl<B> Body for Substituted<B>
where
    B: Body<Data = Bytes> + Unpin,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, B::Error>>> {
        let this = &mut *self;
        loop {
            if let Some(trailers) = this.trailers.take() {
                this.flushed = true;
                return Poll::Ready(Some(Ok(trailers)));
            }
            if this.flushed {
                return Poll::Ready(None);
            }
            match ready!(Pin::new(&mut this.inner).poll_frame(cx)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => {
                        let mut buf = std::mem::take(&mut this.held);
                        buf.extend_from_slice(&data);
                        let out = this.run(&buf, true);
                        if !out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                    }
                    Err(trailers) => {
                        let held = std::mem::take(&mut this.held);
                        let out = this.run(&held, false);
                        this.trailers = Some(trailers);
                        if !out.is_empty() {
                            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                        }
                    }
                },
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => {
                    this.flushed = true;
                    let held = std::mem::take(&mut this.held);
                    let out = this.run(&held, false);
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from(out)))));
                    }
                }
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.flushed || (self.held.is_empty() && self.trailers.is_none() && self.inner.is_end_stream())
    }

    fn size_hint(&self) -> SizeHint {
        if self.same_length { self.inner.size_hint() } else { SizeHint::default() }
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::{BodyExt as _, StreamBody};

    use super::*;

    async fn run(chunks: &[&str], subs: &[(&str, &str)]) -> String {
        let frames: Vec<Result<Frame<Bytes>, std::convert::Infallible>> =
            chunks.iter().map(|c| Ok(Frame::data(Bytes::copy_from_slice(c.as_bytes())))).collect();
        let body = StreamBody::new(futures_util::stream::iter(frames));
        let subs = subs.iter().map(|(s, r)| (s.as_bytes().to_vec(), r.as_bytes().to_vec())).collect();
        let out = Substituted::new(body, subs).collect().await.expect("body").to_bytes();
        String::from_utf8(out.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn swaps_across_chunk_boundaries() {
        let subs = [("fake_value_abc", "REAL"), ("fake_value_xyz", "OTHER")];
        assert_eq!(run(&["a=fake_val", "ue_abc&b=fake_value_x", "yz!"], &subs).await, "a=REAL&b=OTHER!");
        assert_eq!(run(&["fake_value_abc"], &subs).await, "REAL");
        assert_eq!(run(&["no placeholder here"], &subs).await, "no placeholder here");
        assert_eq!(run(&["f", "a", "k", "e"], &subs).await, "fake");
    }
}
