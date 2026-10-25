//! A synchronous resolution keeps one budget across queueing, retries and
//! catalogue pages. This scoped thread-local also covers nested provider calls
//! without changing the public catalogue API; it is restored on unwind.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::SourceError;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(super) struct Scope(Option<Instant>);

pub(super) fn enter(deadline: Instant) -> Scope {
    let previous = DEADLINE.replace(Some(DEADLINE.get().map_or(deadline, |old| old.min(deadline))));
    Scope(previous)
}

impl Drop for Scope {
    fn drop(&mut self) {
        DEADLINE.set(self.0);
    }
}

pub(super) fn timeout(maximum: Duration) -> Result<Duration, SourceError> {
    let remaining = DEADLINE.get().map_or(maximum, |deadline| {
        deadline.saturating_duration_since(Instant::now()).min(maximum)
    });
    if remaining.is_zero() {
        Err(SourceError::Http("source resolution deadline exceeded".into()))
    } else {
        Ok(remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_budgets_cannot_extend_the_outer_deadline_and_restore_it() {
        assert_eq!(timeout(Duration::from_secs(10)).unwrap(), Duration::from_secs(10));
        let outer = enter(Instant::now() + Duration::from_millis(100));
        {
            let _inner = enter(Instant::now() + Duration::from_secs(20));
            assert!(timeout(Duration::from_secs(10)).unwrap() <= Duration::from_millis(100));
        }
        assert!(timeout(Duration::from_secs(10)).unwrap() <= Duration::from_millis(100));
        drop(outer);
        assert_eq!(timeout(Duration::from_secs(10)).unwrap(), Duration::from_secs(10));
        let _expired = enter(Instant::now());
        assert!(timeout(Duration::from_secs(10)).is_err());
    }
}
