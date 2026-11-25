//! Singleflight publication per identifier, panic-safe on unwind.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::{lock, LoadResult, SourceError};

type Outcome = Result<Arc<LoadResult>, SourceError>;

#[derive(Default)]
pub(super) struct Flight {
    outcome: Mutex<Option<Outcome>>,
    ready: Condvar,
}

impl Flight {
    pub(super) fn wait(&self, deadline: Instant) -> Outcome {
        let mut held = lock(&self.outcome);
        loop {
            if let Some(outcome) = held.as_ref() {
                return outcome.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SourceError::Http("timed out waiting for source resolution".into()));
            }
            held = self.ready.wait_timeout(held, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    fn publish(&self, outcome: Outcome) {
        let mut held = lock(&self.outcome);
        if held.is_none() {
            *held = Some(outcome);
            drop(held);
            self.ready.notify_all();
        }
    }
}

pub(super) struct Leader<'a> {
    flight: Arc<Flight>,
    key: &'a str,
    inflight: &'a Mutex<HashMap<String, Arc<Flight>>>,
    done: bool,
}

impl<'a> Leader<'a> {
    pub(super) fn new(
        flight: Arc<Flight>,
        key: &'a str,
        inflight: &'a Mutex<HashMap<String, Arc<Flight>>>,
    ) -> Self {
        Self { flight, key, inflight, done: false }
    }

    pub(super) fn finish(mut self, outcome: Outcome) -> Outcome {
        self.flight.publish(outcome.clone());
        self.done = true;
        outcome
    }
}

impl Drop for Leader<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.flight.publish(Err(SourceError::Http("source resolver was interrupted".into())));
        }
        let mut inflight = lock(self.inflight);
        if inflight.get(self.key).is_some_and(|f| Arc::ptr_eq(f, &self.flight)) {
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
        drop(Leader::new(Arc::clone(&flight), "key", &inflight));
        assert!(flight.wait(Instant::now() + Duration::from_secs(1)).is_err());
        assert!(inflight.lock().unwrap().is_empty());
    }
}
