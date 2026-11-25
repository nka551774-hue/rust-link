//! Thread-local resolution budget restored on unwind across nested calls.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::SourceError;

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

pub(super) struct Scope(Option<Instant>);

pub(super) fn bound(deadline: Instant) -> Instant {
    DEADLINE.get().map_or(deadline, |old| old.min(deadline))
}

pub(super) fn enter(deadline: Instant) -> Scope {
    Scope(DEADLINE.replace(Some(bound(deadline))))
}

impl Drop for Scope {
    fn drop(&mut self) {
        DEADLINE.set(self.0);
    }
}

pub(super) fn timeout(maximum: Duration) -> Result<Duration, SourceError> {
    let rem = DEADLINE
        .get()
        .map_or(maximum, |d| d.saturating_duration_since(Instant::now()).min(maximum));
    (!rem.is_zero())
        .then_some(rem)
        .ok_or_else(|| SourceError::Http("source resolution deadline exceeded".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_budgets_cannot_extend_the_outer_deadline_and_restore_it() {
        let ten = Duration::from_secs(10);
        assert_eq!(timeout(ten).unwrap(), ten);
        let outer = enter(Instant::now() + Duration::from_millis(100));
        {
            let _inner = enter(Instant::now() + Duration::from_secs(20));
            assert!(timeout(ten).unwrap() <= Duration::from_millis(100));
        }
        assert!(timeout(ten).unwrap() <= Duration::from_millis(100));
        drop(outer);
        assert_eq!(timeout(ten).unwrap(), ten);
        let _expired = enter(Instant::now());
        assert!(timeout(ten).is_err());
    }
}
