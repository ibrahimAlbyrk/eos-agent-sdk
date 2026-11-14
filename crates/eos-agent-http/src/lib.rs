//! HTTP plumbing the provider adapters share: a request body sent as pieces
//! of shared bytes, and server-sent-event framing.

mod sse;

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body::{Frame, SizeHint};

pub use sse::{Frame as SseFrame, SseDecoder};

/// Small pieces are joined into frames of up to this many bytes.
const FRAME: usize = 16 * 1024;

/// A body sent piece by piece, with its exact length declared up front.
#[derive(Debug)]
pub struct Pieces {
    pieces: std::vec::IntoIter<Bytes>,
    remaining: u64,
}

impl Pieces {
    pub fn new(pieces: Vec<Bytes>) -> Self {
        let remaining = pieces.iter().map(|p| p.len() as u64).sum();
        Self { pieces: pieces.into_iter(), remaining }
    }
}

impl http_body::Body for Pieces {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let Some(first) = self.pieces.by_ref().find(|p| !p.is_empty()) else { return Poll::Ready(None) };
        // Large pieces go out as they are; runs of small ones (messages, the
        // JSON between them) are joined, so a long conversation is a few
        // frames rather than one per message.
        let (mut size, mut joins) = (first.len(), 0);
        for next in self.pieces.as_slice() {
            if size + next.len() > FRAME {
                break;
            }
            size += next.len();
            joins += 1;
        }
        let frame = if first.len() >= FRAME || joins == 0 {
            first
        } else {
            let mut joined = bytes::BytesMut::with_capacity(size);
            joined.extend_from_slice(&first);
            for piece in self.pieces.by_ref().take(joins) {
                joined.extend_from_slice(&piece);
            }
            joined.freeze()
        };
        self.remaining -= frame.len() as u64;
        Poll::Ready(Some(Ok(Frame::data(frame))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_join_small_runs_and_keep_large_ones() {
        use http_body::Body as _;
        let big = Bytes::from(vec![b'x'; FRAME + 1]);
        let small: Vec<Bytes> = (0..100).map(|i| Bytes::from(format!("{i:04}"))).collect();
        let pieces: Vec<Bytes> = small.iter().cloned().chain([big.clone()]).chain(small.iter().cloned()).collect();
        let expected = pieces.concat();
        let mut body = Pieces::new(pieces);
        assert_eq!(body.size_hint().exact(), Some(expected.len() as u64));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut frames = Vec::new();
        while let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut cx) {
            frames.push(frame.into_data().expect("data"));
        }
        assert!(body.is_end_stream());
        assert_eq!(frames.concat(), expected);
        assert_eq!(frames.len(), 3, "two joined runs around the large piece");
        assert_eq!(frames[1].as_ptr(), big.as_ptr(), "the large piece is not copied");
    }
}
