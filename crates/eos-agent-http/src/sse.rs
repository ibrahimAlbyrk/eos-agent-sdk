use bytes::{Buf, BytesMut};

/// Incremental server-sent-events framing. Bytes go in as they arrive; whole
/// `(event, data)` frames come out. Partial frames wait in one buffer.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: BytesMut,
    /// How far into `buf` no frame end was found, so a frame that arrives in
    /// many chunks is not searched from its start on every one.
    scanned: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub event: String,
    pub data: String,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// The next complete frame, if one is buffered.
    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            // A boundary may straddle the end of the last search (`\r\n\r\n` is 4 bytes).
            let Some((end, sep)) = find_boundary(&self.buf, self.scanned.saturating_sub(3)) else {
                self.scanned = self.buf.len();
                return None;
            };
            self.scanned = 0;
            let raw = self.buf.split_to(end);
            self.buf.advance(sep);
            if let Some(frame) = parse(&raw) {
                return Some(frame);
            }
        }
    }

    /// Bytes left that never formed a frame (a stream cut mid-frame).
    pub fn has_partial(&self) -> bool {
        self.buf.iter().any(|b| !b.is_ascii_whitespace())
    }
}

/// The first frame end at or after `from`: where it starts and its length.
fn find_boundary(buf: &[u8], from: usize) -> Option<(usize, usize)> {
    let lf = buf[from..].windows(2).position(|w| w == b"\n\n").map(|i| from + i);
    // Only a `\r\n\r\n` that starts before the first `\n\n` comes first.
    let limit = lf.map_or(buf.len(), |at| (at + 3).min(buf.len()));
    let crlf = buf[from..limit].windows(4).position(|w| w == b"\r\n\r\n").map(|i| from + i);
    match (crlf, lf) {
        (Some(at), _) => Some((at, 4)),
        (None, Some(at)) => Some((at, 2)),
        (None, None) => None,
    }
}

fn parse(raw: &[u8]) -> Option<Frame> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut event = String::from("message");
    let mut data = String::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(v) = line.strip_prefix("event:") {
            event = v.trim_start().to_owned();
        } else if let Some(v) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    (!data.is_empty()).then_some(Frame { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks() {
        let mut d = SseDecoder::default();
        d.push(b"event: ping\nda");
        assert_eq!(d.next_frame(), None);
        d.push(b"ta: {\"type\":\"ping\"}\n\nevent: message_stop\ndata: {}\n\n: comment\n\n");
        assert_eq!(d.next_frame(), Some(Frame { event: "ping".into(), data: r#"{"type":"ping"}"#.into() }));
        assert_eq!(d.next_frame(), Some(Frame { event: "message_stop".into(), data: "{}".into() }));
        assert_eq!(d.next_frame(), None);
        assert!(!d.has_partial());
    }

    #[test]
    fn frames_arriving_byte_by_byte() {
        let stream = b"event: a\r\ndata: 1\r\n\r\nevent: b\ndata: 2\n\ndata: 3\r\n\r\n: c\n\nevent: d\ndata: 4\n\n";
        let mut d = SseDecoder::default();
        let mut frames = Vec::new();
        for byte in stream {
            d.push(&[*byte]);
            while let Some(frame) = d.next_frame() {
                frames.push((frame.event, frame.data));
            }
        }
        let want = [("a", "1"), ("b", "2"), ("message", "3"), ("d", "4")];
        assert_eq!(frames, want.map(|(e, v)| (e.to_owned(), v.to_owned())));
        assert!(!d.has_partial());
    }

    #[test]
    fn crlf_and_multiline_data() {
        let mut d = SseDecoder::default();
        d.push(b"event: x\r\ndata: a\r\ndata: b\r\n\r\n");
        assert_eq!(d.next_frame(), Some(Frame { event: "x".into(), data: "a\nb".into() }));
    }
}
