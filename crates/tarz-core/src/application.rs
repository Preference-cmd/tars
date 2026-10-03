//! Shared instance ownership, supervised work, controlled materials and persistence.

mod execution;

pub use execution::text_input;

mod closing;
mod entry;
mod host;
mod managed;
mod opening;
mod saving;
mod supervisor;
mod types;

pub use crate::store::{CommitStage, StoreError};
pub use host::SessionHost;
pub use managed::ManagedSession;
pub use types::{
    ApplicationError, CloseOutcome, ClosePolicy, CommitDisposition, HostOptions, Lifecycle,
    ManagedSubmitRejection, MaterialSnapshot, MaterialVersion, PreparedCreate, SaveStatus,
    SavedSessionSummary, SessionListing, SessionStatus, SessionTarget,
};

#[cfg(test)]
mod measurements;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod roundtrip_tests;
