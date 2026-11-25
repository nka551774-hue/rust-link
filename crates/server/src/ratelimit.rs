//! Bounded per-route and per-key token-bucket rate limiters.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn sanitize_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        0.0
    }
}

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
        self.tokens + now.saturating_duration_since(self.at).as_secs_f64() * rate >= burst
    }
}

impl Limiter {
    pub fn new(rate: f64, burst: usize) -> Limiter {
        let burst = burst.max(1) as f64;
        Limiter {
            rate: sanitize_rate(rate),
            burst,
            inner: Mutex::new(Bucket { tokens: burst, at: Instant::now() }),
        }
    }

    pub fn allow(&self) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).allow(
            self.rate,
            self.burst,
            Instant::now(),
        )
    }
}

/// Bounded per-key token buckets with idle eviction and shared overflow fallback.
pub struct Keyed {
    rate: f64,
    burst: f64,
    max_keys: usize,
    table: Mutex<Table>,
    overflow: Limiter,
}

struct Table {
    buckets: HashMap<String, Bucket>,
    swept: Option<Instant>,
}

impl Keyed {
    pub fn new(rate: f64, burst: usize, max_keys: usize) -> Keyed {
        let sanitized_rate = sanitize_rate(rate);
        Keyed {
            rate: sanitized_rate,
            burst: burst.max(1) as f64,
            max_keys: max_keys.max(1),
            table: Mutex::new(Table { buckets: HashMap::new(), swept: None }),
            overflow: Limiter::new(sanitized_rate, burst),
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        let mut table = self.table.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let (rate, burst) = (self.rate, self.burst);
        if let Some(bucket) = table.buckets.get_mut(key) {
            return bucket.allow(rate, burst, now);
        }
        if table.buckets.len() >= self.max_keys
            && table
                .swept
                .is_none_or(|at| now.saturating_duration_since(at) >= Duration::from_secs(1))
        {
            table.buckets.retain(|_, bucket| !bucket.idle(rate, burst, now));
            table.swept = Some(now);
        }
        if table.buckets.len() >= self.max_keys {
            drop(table);
            return self.overflow.allow();
        }
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
        assert!(keyed.allow("a") && keyed.allow("a") && !keyed.allow("a"));
        assert!(keyed.allow("b") && keyed.allow("b"));
        assert_eq!(keyed.keys(), 2);
    }

    #[test]
    fn inventing_keys_cannot_grow_the_table_or_buy_more_requests() {
        let keyed = Keyed::new(0.0, 1, 3);
        for key in ["a", "b", "c", "d"] {
            assert!(keyed.allow(key));
        }
        for key in ["e", "f", "g", "h"] {
            assert!(!keyed.allow(key));
        }
        assert_eq!(keyed.keys(), 3);
    }

    #[test]
    fn refilled_buckets_are_forgotten_to_make_room() {
        let keyed = Keyed::new(1000.0, 1, 2);
        assert!(keyed.allow("a") && keyed.allow("b"));
        std::thread::sleep(Duration::from_millis(20));
        assert!(keyed.allow("c"));
        assert_eq!(keyed.keys(), 1);
    }

    #[test]
    fn limiter_handles_burst_refill_zero_rate_and_zero_burst() {
        let limiter = Limiter::new(1.0, 3);
        assert!(limiter.allow() && limiter.allow() && limiter.allow() && !limiter.allow());

        let refilling = Limiter::new(20.0, 1);
        assert!(refilling.allow() && !refilling.allow());
        std::thread::sleep(Duration::from_millis(120));
        assert!(refilling.allow());

        let fixed = Limiter::new(0.0, 2);
        assert!(fixed.allow() && fixed.allow() && !fixed.allow());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!fixed.allow());

        let clamped = Limiter::new(1.0, 0);
        assert!(clamped.allow() && !clamped.allow());
    }
}
