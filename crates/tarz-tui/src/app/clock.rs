#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

/// Injectable monotonic clock abstraction for `App`.
///
/// In production, uses real system time (`Instant::now()`).
/// In tests, uses a manually advanced clock without sleeping.
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
    /// Create a real system clock.
    pub(crate) fn real() -> Self {
        Self {
            source: ClockSource::Real,
        }
    }

    /// Create a manual clock initialized to the given instant.
    #[cfg(test)]
    pub(crate) fn manual(initial: Instant) -> Self {
        Self {
            source: ClockSource::Manual(Arc::new(Mutex::new(initial))),
        }
    }

    /// Current monotonic time.
    pub(crate) fn now(&self) -> Instant {
        match &self.source {
            ClockSource::Real => Instant::now(),
            #[cfg(test)]
            ClockSource::Manual(cell) => *cell.lock().unwrap(),
        }
    }

    /// Advance the manual clock by `duration`. Has no effect on real clock.
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
