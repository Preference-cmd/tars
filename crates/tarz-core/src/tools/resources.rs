use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Shared host-wide filesystem concurrency slots.
pub struct ToolResources {
    pub(crate) slots: Arc<Semaphore>,
    pub(crate) counters: Arc<Counters>,
}

impl ToolResources {
    /// Construct the host's actual-operation limit. Zero admits no operations.
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            slots: Arc::new(Semaphore::new(limit)),
            counters: Arc::new(Counters::default()),
        })
    }
}

/// Counters for actual operations; rejected parameters and permit waits do not
/// increment starts. Scan bytes count bytes actually returned by file reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg(test)]
pub struct ToolMetrics {
    /// Number of filesystem operations started.
    pub started: usize,
    /// Number of blocking closures that have released their resources.
    pub completed: usize,
    /// Current number of actual blocking operations.
    pub active: usize,
    /// Highest observed number of simultaneous operations in this session.
    pub peak_active: usize,
    /// Actual file bytes scanned across all calls.
    pub scanned_bytes: usize,
}

#[derive(Default)]
pub(crate) struct Counters {
    // Deterministic integration-test boundary: the real wrapper was entered,
    // unlike a declaration-stage Waiting observation. Never shipped as API.
    #[cfg(test)]
    pub test_wrappers_entered: AtomicUsize,
    pub started: AtomicUsize,
    pub completed: AtomicUsize,
    pub active: AtomicUsize,
    pub peak: AtomicUsize,
    pub bytes: AtomicUsize,
}

impl Counters {
    #[cfg(test)]
    pub fn snapshot(&self) -> ToolMetrics {
        ToolMetrics {
            started: self.started.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            peak_active: self.peak.load(Ordering::Relaxed),
            scanned_bytes: self.bytes.load(Ordering::Relaxed),
        }
    }
}

pub(crate) struct OperationGuard {
    pub counters: Arc<Counters>,
    pub host_counters: Arc<Counters>,
    pub _local: OwnedSemaphorePermit,
    pub _host: OwnedSemaphorePermit,
}

impl OperationGuard {
    pub fn start(&self) {
        for counters in [&self.counters, &self.host_counters] {
            counters.started.fetch_add(1, Ordering::Relaxed);
            let active = counters.active.fetch_add(1, Ordering::Relaxed) + 1;
            counters.peak.fetch_max(active, Ordering::Relaxed);
        }
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        for counters in [&self.counters, &self.host_counters] {
            counters.active.fetch_sub(1, Ordering::Relaxed);
            counters.completed.fetch_add(1, Ordering::Relaxed);
        }
    }
}
