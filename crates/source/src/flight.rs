//! Per-identifier publication: every existing waiter sees the same completion,
//! including failures and results too large to cache.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::{LoadResult, SourceError};

type Outcome = Result<Arc<LoadResult>, SourceError>;

#[derive(Default)]
pub(super) struct Flight {
    outcome: Mutex<Option<Outcome>>,
    ready: Condvar,
}

impl Flight {
    pub(super) fn wait(&self, deadline: Instant) -> Outcome {
        let mut held = self.outcome.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if let Some(outcome) = held.as_ref() {
                return outcome.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SourceError::Http("timed out waiting for source resolution".into()));
            }
            held = self.ready.wait_timeout(held, left).unwrap_or_else(|error| error.into_inner()).0;
        }
    }

    fn publish(&self, outcome: Outcome) {
        let mut held = self.outcome.lock().unwrap_or_else(|error| error.into_inner());
        if held.is_none() {
            *held = Some(outcome);
            self.ready.notify_all();
        }
    }
}

pub(super) struct Leader<'a> {
    pub(super) flight: Arc<Flight>,
    pub(super) key: &'a str,
    pub(super) inflight: &'a Mutex<HashMap<String, Arc<Flight>>>,
}

impl Leader<'_> {
    pub(super) fn finish(self, outcome: Outcome) -> Outcome {
        self.flight.publish(outcome.clone());
        outcome
    }
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        // Unwinding must not leave callers sleeping until timeout or a key
        // permanently registered. A completed result is never overwritten.
        self.flight.publish(Err(SourceError::Http("source resolver was interrupted".into())));
        let mut inflight = self.inflight.lock().unwrap_or_else(|error| error.into_inner());
        if inflight.get(self.key).is_some_and(|flight| Arc::ptr_eq(flight, &self.flight)) {
            inflight.remove(self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn waiting_has_a_deadline_and_does_not_cancel_the_leader() {
        let flight = Flight::default();
        let start = Instant::now();
        assert!(flight.wait(start + Duration::from_millis(20)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        flight.publish(Ok(Arc::new(LoadResult::Empty)));
        assert!(flight.wait(Instant::now()).is_ok());
    }

    #[test]
    fn dropping_a_leader_wakes_waiters_with_an_error() {
        let flight = Arc::new(Flight::default());
        let inflight = Mutex::new(HashMap::from([("key".to_string(), Arc::clone(&flight))]));
        drop(Leader { flight: Arc::clone(&flight), key: "key", inflight: &inflight });
        assert!(flight.wait(Instant::now() + Duration::from_secs(1)).is_err());
        assert!(inflight.lock().unwrap().is_empty());
    }
}
