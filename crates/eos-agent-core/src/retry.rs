use std::time::Duration;

/// Exponential backoff with jitter. A server's `retry-after` wins when longer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base: Duration,
    pub cap: Duration,
    /// Fraction of the delay added at random (0.25 = up to +25%).
    pub jitter: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_retries: 10, base: Duration::from_millis(500), cap: Duration::from_secs(32), jitter: 0.25 }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based).
    pub fn delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.saturating_sub(1).min(16)).min(self.cap);
        let jittered = exp.mul_f64(1.0 + self.jitter * fastrand::f64());
        retry_after.map_or(jittered, |server| server.max(jittered))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_then_caps() {
        let p = RetryPolicy { jitter: 0.0, ..RetryPolicy::default() };
        assert_eq!(p.delay(1, None), Duration::from_millis(500));
        assert_eq!(p.delay(3, None), Duration::from_secs(2));
        assert_eq!(p.delay(20, None), Duration::from_secs(32));
        assert_eq!(p.delay(1, Some(Duration::from_secs(7))), Duration::from_secs(7));
    }
}
