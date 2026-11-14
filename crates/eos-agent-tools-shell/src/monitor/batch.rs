//! Turning a monitor's stdout into events: lines grouped per 200 ms batch,
//! batches rate-limited like Claude Code's.

use std::time::{Duration, Instant};

/// Lines arriving within this window after the first share one event.
pub(super) const BATCH_WINDOW: Duration = Duration::from_millis(200);
const LINE_MAX_CHARS: usize = 500;
const BATCH_MAX_CHARS: usize = 3000;
/// A line without a newline yet keeps at most its last this-many bytes.
const PARTIAL_MAX_BYTES: usize = 64 * 1024;

/// Events allowed in a burst, then one more per REFILL.
const BUCKET: u32 = 10;
const REFILL: Duration = Duration::from_millis(2000);
/// Suppressing for longer than this stops the monitor.
const SUPPRESS_LIMIT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub(super) struct Lines {
    partial: Vec<u8>,
    pending: Vec<String>,
    pending_chars: usize,
}

impl Lines {
    pub(super) fn push(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            self.add(&String::from_utf8_lossy(&line));
        }
        if self.partial.len() > PARTIAL_MAX_BYTES {
            let excess = self.partial.len() - PARTIAL_MAX_BYTES;
            self.partial.drain(..excess);
        }
    }

    fn add(&mut self, line: &str) {
        let line = line.trim();
        // Past the batch cap a line would be cut off anyway.
        if line.is_empty() || self.pending_chars > BATCH_MAX_CHARS {
            return;
        }
        let line = truncate(line, LINE_MAX_CHARS, "...(truncated)");
        self.pending_chars += line.chars().count() + 1;
        self.pending.push(line);
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The pending lines as one event; `last` also takes an unfinished line.
    pub(super) fn take(&mut self, last: bool) -> Option<String> {
        if last {
            let rest = std::mem::take(&mut self.partial);
            self.add(&String::from_utf8_lossy(&rest));
        }
        if self.pending.is_empty() {
            return None;
        }
        let joined = self.pending.join("\n");
        self.pending.clear();
        self.pending_chars = 0;
        Some(truncate(&joined, BATCH_MAX_CHARS, "\n...(truncated)"))
    }
}

fn truncate(s: &str, max: usize, marker: &str) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}{marker}", &s[..at]),
        None => s.to_owned(),
    }
}

/// What to do with a batch.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Deliver it, after a notice about suppressed events when there is one.
    Deliver {
        notice: Option<String>,
    },
    Suppress,
    /// Suppressed too long: stop the monitor with this notice.
    Stop(String),
}

#[derive(Debug)]
pub(super) struct RateLimit {
    tokens: u32,
    refilled_at: Instant,
    suppressed: u32,
    suppressing_since: Option<Instant>,
    last_suppressed: Option<Instant>,
}

impl RateLimit {
    pub(super) fn new(now: Instant) -> Self {
        Self { tokens: BUCKET, refilled_at: now, suppressed: 0, suppressing_since: None, last_suppressed: None }
    }

    pub(super) fn judge(&mut self, now: Instant) -> Verdict {
        if self.try_take(now) {
            let notice = (self.suppressed > 0).then(|| {
                format!(
                    "[{} events suppressed \u{2014} output rate too high. Consider using TaskStop to restart this \
                     monitor with a more selective filter.]",
                    self.suppressed
                )
            });
            self.suppressed = 0;
            if self.last_suppressed.is_some_and(|t| now.duration_since(t) > REFILL * 3) {
                self.suppressing_since = None;
            }
            return Verdict::Deliver { notice };
        }
        self.suppressed += 1;
        self.last_suppressed = Some(now);
        let since = *self.suppressing_since.get_or_insert(now);
        let span = now.duration_since(since);
        if span > SUPPRESS_LIMIT {
            return Verdict::Stop(format!(
                "[Monitor stopped \u{2014} too much output ({} events suppressed over {}s). Restart with a more \
                 selective source.]",
                self.suppressed,
                span.as_secs_f64().round()
            ));
        }
        Verdict::Suppress
    }

    fn try_take(&mut self, now: Instant) -> bool {
        let periods = now.duration_since(self.refilled_at).as_millis() / REFILL.as_millis();
        if periods > 0 {
            let periods = u32::try_from(periods).unwrap_or(u32::MAX);
            self.tokens = self.tokens.saturating_add(periods).min(BUCKET);
            self.refilled_at += REFILL * periods;
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_trim_and_truncate() {
        let mut lines = Lines::default();
        lines.push(b"one\n  two  \n\nthr");
        assert_eq!(lines.take(false).as_deref(), Some("one\ntwo"));
        lines.push(b"ee\n");
        lines.push("x".repeat(600).as_bytes());
        assert_eq!(lines.take(false).as_deref(), Some("three"));
        let last = lines.take(true).unwrap_or_default();
        assert_eq!(last, format!("{}...(truncated)", "x".repeat(500)));
        assert_eq!(lines.take(true), None);
    }

    #[test]
    fn batches_are_capped() {
        let mut lines = Lines::default();
        for _ in 0..100 {
            lines.push(format!("{}\n", "y".repeat(100)).as_bytes());
        }
        let batch = lines.take(false).unwrap_or_default();
        assert!(batch.ends_with("\n...(truncated)"));
        assert_eq!(batch.chars().count(), BATCH_MAX_CHARS + "\n...(truncated)".len());
    }

    #[test]
    fn rate_limit_suppresses_then_reports_then_stops() {
        let start = Instant::now();
        let mut limit = RateLimit::new(start);
        for _ in 0..BUCKET {
            assert_eq!(limit.judge(start), Verdict::Deliver { notice: None });
        }
        assert_eq!(limit.judge(start), Verdict::Suppress);
        assert_eq!(limit.judge(start), Verdict::Suppress);
        match limit.judge(start + REFILL) {
            Verdict::Deliver { notice: Some(n) } => assert!(n.starts_with("[2 events suppressed")),
            other => panic!("expected a notice, got {other:?}"),
        }
        // A steady flood (a batch every 200 ms) is stopped after 30 s of suppression.
        let mut flood = RateLimit::new(start);
        let mut stopped_at = None;
        for tick in 0..300u32 {
            if let Verdict::Stop(notice) = flood.judge(start + BATCH_WINDOW * tick) {
                assert!(notice.starts_with("[Monitor stopped \u{2014} too much output ("), "{notice}");
                stopped_at = Some(BATCH_WINDOW * tick);
                break;
            }
        }
        let stopped_at = stopped_at.unwrap_or_default();
        assert!(stopped_at > SUPPRESS_LIMIT && stopped_at < SUPPRESS_LIMIT + Duration::from_secs(3), "{stopped_at:?}");
    }
}
