//! Concrete local JSON session snapshots and activity-lifetime write ownership.
//!
//! The host retains an `Arc<Writer>` in every blocking operation. The lock file
//! is stable and separate from the atomically replaced record; no public product
//! entry point exposes a writer. Local process interruption is covered, whereas
//! power-loss directory durability and network filesystems are outside scope.

mod error;
mod json;
mod record;

pub use error::{CommitStage, StoreError};
pub(crate) use json::{CommitResolution, SessionStore, Writer};
pub(crate) use record::{PreparedCommit, SavedSessionRecord};

#[cfg(test)]
pub(crate) use json::{FaultAction, FaultHook};
#[cfg(test)]
mod tests;
