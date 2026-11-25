//! Request throttling.
//!
//! Each route gets its own bucket, so a client that hammers one route does not
//! spend the allowance of another. A bucket is a token bucket: tokens arrive at
//! a fixed rate up to a ceiling, one is spent per request, and a request that
//! finds none is refused rather than queued. Queueing a burst would turn abuse
//! into latency, which hides it instead of reporting it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

impl Bucket {
    fn new(burst: f64, now: Instant) -> Self {
        Self { tokens: burst, at: now }
    }

    fn allow(&mut self, rate: f64, burst: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * rate).min(burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn idle(&self, rate: f64, burst: f64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens + elapsed * rate >= burst
    }
}

impl Limiter {
    /// Builds a bucket that starts full, so a client that has been quiet is not
    /// made to wait before its first request.
    pub fn new(rate: f64, burst: usize) -> Limiter {
        let burst = burst.max(1) as f64;
        Limiter {
            rate: if rate.is_finite() && rate > 0.0 { rate } else { 0.0 },
            burst,
            inner: Mutex::new(Bucket::new(burst, Instant::now())),
        }
    }

    /// Spends one token and reports whether there was one.
    pub fn allow(&self) -> bool {
        let mut bucket = self.inner.lock().unwrap();
        bucket.allow(self.rate, self.burst, Instant::now())
    }
}

/// One token bucket per key, such as a session or a remote address, so a client
/// can only spend its own allowance.
///
/// The number of keys is bounded, because the keys can come from someone who
/// pays nothing to invent them: at the bound the buckets that have refilled are
/// forgotten, and when none can be, the rest of the keys share one bucket
/// instead of growing the table.
pub struct Keyed {
    rate: f64,
    burst: f64,
    max_keys: usize,
    table: Mutex<Table>,
    overflow: Limiter,
}

struct Table {
    buckets: HashMap<String, Bucket>,
    /// When refilled buckets were last forgotten, so a table that stays full
    /// of busy keys is not swept on every request.
    swept: Instant,
}

impl Keyed {
    pub fn new(rate: f64, burst: usize, max_keys: usize) -> Keyed {
        let sanitized_rate = if rate.is_finite() && rate > 0.0 { rate } else { 0.0 };
        let sanitized_burst = burst.max(1) as f64;
        Keyed {
            rate: sanitized_rate,
            burst: sanitized_burst,
            max_keys: max_keys.max(1),
            table: Mutex::new(Table { buckets: HashMap::new(), swept: Instant::now() }),
            overflow: Limiter::new(sanitized_rate, burst),
        }
    }

    /// Spends a token from `key`'s bucket, creating it full on first use.
    pub fn allow(&self, key: &str) -> bool {
        let mut table = self.table.lock().unwrap();
        let now = Instant::now();
        let rate = self.rate;
        let burst = self.burst;
        if let Some(bucket) = table.buckets.get_mut(key) {
            return bucket.allow(rate, burst, now);
        }
        if table.buckets.len() >= self.max_keys
            && now.saturating_duration_since(table.swept) >= Duration::from_secs(1)
        {
            table.buckets.retain(|_, bucket| !bucket.idle(rate, burst, now));
            table.swept = now;
        }
        if table.buckets.len() >= self.max_keys {
            drop(table);
            return self.overflow.allow();
        }
        // Burst is clamped to >= 1.0 in `new`, so a brand-new bucket always
        // starts full and immediately spends one token.
        table.buckets.insert(key.to_owned(), Bucket { tokens: burst - 1.0, at: now });
        true
    }

    #[cfg(test)]
    fn keys(&self) -> usize {
        self.table.lock().unwrap().buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Keyed, Limiter};
    use std::time::Duration;

    #[test]
    fn each_key_spends_only_its_own_bucket() {
        let keyed = Keyed::new(0.0, 2, 10);
        assert!(keyed.allow("a") && keyed.allow("a"));
        assert!(!keyed.allow("a"), "a is spent");
        assert!(keyed.allow("b") && keyed.allow("b"), "b has its own");
        assert_eq!(keyed.keys(), 2);
    }

    #[test]
    fn inventing_keys_cannot_grow_the_table_or_buy_more_requests() {
        let keyed = Keyed::new(0.0, 1, 3);
        for key in ["a", "b", "c"] {
            assert!(keyed.allow(key));
        }
        // The table is full of spent buckets, so new keys share one overflow
        // bucket: the first of them passes, the rest are refused.
        assert!(keyed.allow("d"));
        for key in ["e", "f", "g", "h"] {
            assert!(!keyed.allow(key), "{key} must not get a bucket of its own");
        }
        assert_eq!(keyed.keys(), 3, "the table never passes its bound");
    }

    #[test]
    fn refilled_buckets_are_forgotten_to_make_room() {
        let keyed = Keyed::new(1000.0, 1, 2);
        assert!(keyed.allow("a") && keyed.allow("b"));
        std::thread::sleep(Duration::from_millis(1100));
        // Both buckets have refilled, so they are swept and "c" gets its own.
        assert!(keyed.allow("c"));
        assert_eq!(keyed.keys(), 1);
    }

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
