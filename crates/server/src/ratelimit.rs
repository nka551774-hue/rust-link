//! Request throttling.
//!
//! Each route gets its own bucket, so a client that hammers one route does not
//! spend the allowance of another. A bucket is a token bucket: tokens arrive at
//! a fixed rate up to a ceiling, one is spent per request, and a request that
//! finds none is refused rather than queued. Queueing a burst would turn abuse
//! into latency, which hides it instead of reporting it.

use std::sync::Mutex;
use std::time::Instant;

/// A token bucket, with the rate in tokens per second and the ceiling in
/// tokens.
pub struct Limiter {
    rate: f64,
    burst: f64,
    inner: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Limiter {
    /// Builds a bucket that starts full, so a client that has been quiet is not
    /// made to wait before its first request.
    pub fn new(rate: f64, burst: usize) -> Limiter {
        let burst = burst.max(1) as f64;
        Limiter {
            rate: if rate > 0.0 { rate } else { 0.0 },
            burst,
            inner: Mutex::new(Bucket { tokens: burst, at: Instant::now() }),
        }
    }

    /// Spends one token and reports whether there was one.
    pub fn allow(&self) -> bool {
        let mut bucket = self.inner.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.at = now;
        bucket.tokens = (bucket.tokens + elapsed * self.rate).min(self.burst);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Limiter;
    use std::time::Duration;

    #[test]
    fn a_bucket_starts_full_and_then_refuses() {
        let limiter = Limiter::new(1.0, 3);
        for _ in 0..3 {
            assert!(limiter.allow());
        }
        assert!(!limiter.allow());
    }

    #[test]
    fn tokens_come_back_over_time() {
        let limiter = Limiter::new(20.0, 1);
        assert!(limiter.allow());
        assert!(!limiter.allow());
        std::thread::sleep(Duration::from_millis(120));
        assert!(limiter.allow());
    }

    #[test]
    fn a_bucket_with_no_rate_never_refills() {
        let limiter = Limiter::new(0.0, 2);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!limiter.allow());
    }

    #[test]
    fn a_burst_of_zero_still_lets_one_through() {
        let limiter = Limiter::new(1.0, 0);
        assert!(limiter.allow());
        assert!(!limiter.allow());
    }
}
