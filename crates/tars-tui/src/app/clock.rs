#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

/// Injectable monotonic clock: real in production, manually advanced in tests
/// so guard-window behavior needs no sleeping.
#[derive(Clone, Debug)]
pub(crate) struct Clock {
    source: ClockSource,
}

#[derive(Clone, Debug)]
enum ClockSource {
    Real,
    #[cfg(test)]
    Manual(Arc<Mutex<Instant>>),
}

impl Default for Clock {
    fn default() -> Self {
        Self::real()
    }
}

impl Clock {
    pub(crate) fn real() -> Self {
        Self {
            source: ClockSource::Real,
        }
    }

    #[cfg(test)]
    pub(crate) fn manual(initial: Instant) -> Self {
        Self {
            source: ClockSource::Manual(Arc::new(Mutex::new(initial))),
        }
    }

    pub(crate) fn now(&self) -> Instant {
        match &self.source {
            ClockSource::Real => Instant::now(),
            #[cfg(test)]
            ClockSource::Manual(cell) => *cell.lock().unwrap(),
        }
    }

    /// Advance the manual clock; a real clock ignores this.
    #[cfg(test)]
    pub(crate) fn advance(&self, duration: Duration) {
        match &self.source {
            ClockSource::Real => {}
            ClockSource::Manual(cell) => {
                let mut guard = cell.lock().unwrap();
                *guard += duration;
            }
        }
    }
}
