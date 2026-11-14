use bytes::{Buf, BytesMut};

/// Incremental server-sent-events framing. Bytes go in as they arrive; whole
/// `(event, data)` frames come out. Partial frames wait in one buffer.
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: BytesMut,
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
            let (end, sep) = find_boundary(&self.buf)?;
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

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
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
    fn crlf_and_multiline_data() {
        let mut d = SseDecoder::default();
        d.push(b"event: x\r\ndata: a\r\ndata: b\r\n\r\n");
        assert_eq!(d.next_frame(), Some(Frame { event: "x".into(), data: "a\nb".into() }));
    }
}
