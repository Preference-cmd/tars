//! SQLite v3 session snapshots, workspace trust, and activity-lifetime write ownership.
//!
//! The host retains an `Arc<Writer>` in every blocking operation. Each session
//! uses a stable OS advisory lock file (`<store_root>/locks/<session_id>.lock`)
//! separate from the WAL database (`<store_root>/tars.db`); no public product
//! entry point exposes a writer. Local process interruption is covered, whereas
//! power-loss directory durability and network filesystems are outside scope.

mod error;
mod legacy;
mod multi_agent;
mod record;
pub(crate) mod schema;
mod sqlite;
mod trust;

pub use error::{CommitStage, StoreError};
pub use legacy::LegacyImportSummary;
#[allow(unused_imports)]
pub(crate) use multi_agent::{
    AdmitOperationOutcome, AdmitOperationRequest, AgentMaterialUpdate, NewAgentRow,
    OperationTerminalUpdate, OutboxCommitRequest, OutboxCommitResult, ReadAgentPage,
    StoredAgentRow, StoredOperationRow,
};
pub(crate) use record::{PreparedCommit, RecordMetadata, SavedSessionRecord};
pub(crate) use sqlite::{CommitResolution, MAX_RECORD_BYTES, SessionStore, Writer};

#[cfg(test)]
pub(crate) use sqlite::{FaultAction, FaultHook};
#[cfg(test)]
mod multi_agent_tests;
#[cfg(test)]
mod tests;
