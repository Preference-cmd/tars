use crate::definition::SessionDefinition;
use crate::session::{ConversationId, SessionError, SubmitRequest, WorkRef};
use crate::store::StoreError;
use causa::kernel::Context;
use std::sync::Arc;

/// Opaque address of one instance on one host; a reopened conversation differs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionTarget {
    pub(super) host: uuid::Uuid,
    pub(super) generation: uuid::Uuid,
    pub(super) conversation_id: ConversationId,
}
impl SessionTarget {
    /// Stable conversation identity, independent of the active instance.
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }
}

/// Fixed creation input and identities, reusable after a cancelled wait.
#[derive(Clone, Debug)]
pub struct PreparedCreate {
    pub(super) conversation_id: ConversationId,
    pub(super) commit_id: String,
    pub(super) definition: SessionDefinition,
    pub(super) context: Context,
    pub(super) persistent: bool,
}
impl PreparedCreate {
    /// Prepare a durable initial snapshot before any asynchronous operation.
    pub fn persistent(definition: SessionDefinition, context: Context) -> Self {
        Self::new(definition, context, true)
    }
    /// Prepare a temporary conversation with no implicit store writes.
    pub fn transient(definition: SessionDefinition, context: Context) -> Self {
        Self::new(definition, context, false)
    }
    fn new(definition: SessionDefinition, context: Context, persistent: bool) -> Self {
        Self {
            conversation_id: uuid::Uuid::now_v7().to_string().into(),
            commit_id: uuid::Uuid::now_v7().to_string(),
            definition,
            context,
            persistent,
        }
    }
    /// Read the identity allocated synchronously by preparation.
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }
}

/// Host resource bounds; these bound counts, not total retained memory.
#[derive(Clone, Debug)]
pub struct HostOptions {
    /// Opening, active, blocked and closing slots together; must be positive.
    pub max_active_sessions: usize,
    /// Maximum concurrently running blocking storage operations; must be positive.
    pub max_storage_operations: usize,
    /// Whether opening a session connects the MCP servers its definition names.
    /// A host that only saves or repairs definitions turns this off so it
    /// starts no server processes; its sessions then offer only built-in tools.
    pub connect_mcp_servers: bool,
}
impl Default for HostOptions {
    fn default() -> Self {
        Self {
            max_active_sessions: 32,
            max_storage_operations: 4,
            connect_mcp_servers: true,
        }
    }
}

/// Instance lifecycle independent of execution and persistence state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Resource resolution or initial commit is in progress.
    Opening,
    /// Initial commit needs confirmation or retry; no handle is published.
    InitializationBlocked,
    /// Instance accepts new work if execution and save gates allow it.
    Open,
    /// Explicit close is in progress or blocked.
    Closing,
    /// Instance was released; retained observations remain readable.
    Closed,
    /// Abnormal owner or worker failure; no new work may start.
    Faulted,
    /// Opening failed before initial commit, and the slot was removed.
    Failed,
}

/// A material generation scoped to exactly one active instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialVersion {
    pub(super) target: SessionTarget,
    pub(super) generation: u64,
}

/// An owned, versioned material copy for controlled edits or explicit export.
#[derive(Clone, Debug)]
pub struct MaterialSnapshot {
    /// Version required by an atomic replacement edit.
    pub version: MaterialVersion,
    /// Current idle materials.
    pub context: Context,
    /// Rewrite record index matching this Context, in creation order.
    pub history: Vec<crate::compact::HistoryRecordEntry>,
}

/// Independent persistence outcome; never rewrites the low-level work result.
#[derive(Clone, Debug)]
pub enum SaveStatus {
    /// Temporary session with no automatic storage.
    NotTracked,
    /// Actual materials match a confirmed persistent revision.
    Saved {
        /// Store revision confirmed by the current writer.
        revision: u64,
        /// Material generation represented by that revision.
        material_version: MaterialVersion,
    },
    /// Work or edit changed materials and its fixed snapshot is being prepared.
    Dirty {
        /// Last confirmed revision, absent for a new record.
        base_revision: Option<u64>,
        /// Generation to be saved.
        material_version: MaterialVersion,
    },
    /// One immutable commit is in flight.
    Saving {
        /// Fixed identity reused on confirmation and retry.
        commit_id: String,
        /// Last confirmed revision.
        base_revision: Option<u64>,
        /// Generation represented by the pending snapshot.
        material_version: MaterialVersion,
    },
    /// Original pending snapshot is retained for confirmation or retry.
    SaveBlocked {
        /// Structured storage error without string parsing.
        error: Arc<StoreError>,
        /// Identity of the retained immutable commit.
        commit_id: String,
        /// Generation represented by that commit.
        material_version: MaterialVersion,
    },
}

/// Facts learned while an instance was opened, for optional caller reporting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// A saved `full-access` mode was replaced by `ask` because the frozen
    /// definition was not created with `full-access`.
    pub permission_downgraded: bool,
}

/// Running compaction settings for one open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Whether automatic compaction is enabled.
    pub auto_compact: bool,
    /// Trigger percentage of the context window.
    pub compact_threshold: u8,
    /// Frozen model context window, or none when unknown.
    pub context_window: Option<u64>,
}

/// Retained-work capacity of one open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkCapacity {
    /// Records currently retained, including terminal and unread ones.
    pub used: usize,
    /// Configured maximum; zero rejects new work.
    pub capacity: usize,
}

/// Current conservative request-size estimate and configured window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    /// Estimated prompt size of the next request, in tokens.
    pub estimate_tokens: u64,
    /// Frozen model context window, or none when unknown.
    pub context_window: Option<u64>,
    /// Whether automatic compaction is enabled.
    pub auto_compact: bool,
    /// Trigger percentage of the context window.
    pub compact_threshold: u8,
}

/// Read-only lookup and managed status; does not grant mutable owner access.
#[derive(Clone, Debug)]
pub struct SessionStatus {
    /// Exact active instance.
    pub target: SessionTarget,
    /// Opening, open, closing or failure state.
    pub lifecycle: Lifecycle,
    /// Last persistence progress or outcome.
    pub save: SaveStatus,
    /// Work awaiting execution completion or its associated save.
    pub active_work: Option<WorkRef>,
    /// Lifecycle failure, if any; work results remain independently observable.
    pub error: Option<ApplicationError>,
}

/// Explicit close policy; in-flight close policies cannot silently change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePolicy {
    /// Confirm the latest actual materials before releasing the writer.
    Save,
    /// Abandon local unsaved materials after running work and I/O have stopped.
    DiscardLocal,
}

/// Structured managed admission, opening, editing and lifecycle failures.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ApplicationError {
    /// Current materials, repair provenance or a prepared plan need attention.
    #[error(transparent)]
    Repair(#[from] super::repair::RepairError),
    /// Original low-level contract failure.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Original concrete store failure.
    #[error("session store: {0}")]
    Store(#[source] Arc<StoreError>),
    /// Definition rejected before resource resolution.
    #[error(transparent)]
    Definition(#[from] crate::definition::DefinitionError),
    /// Runtime resource resolution failed; no model call was made.
    #[error("session assembly failed: {0}")]
    Assembly(String),
    /// No root was configured for persistent operations.
    #[error("persistent session store is not configured")]
    StoreUnavailable,
    /// The supplied target belongs to another host or instance.
    #[error("stale session instance")]
    StaleInstance,
    /// The requested instance is no longer in the active directory.
    #[error("session instance is gone")]
    InstanceGone,
    /// A new key or edit attempted to cross a save barrier.
    #[error("session materials are being saved")]
    Saving,
    /// A retained save must be confirmed or retried first.
    #[error("session save is blocked: {0}")]
    SaveBlocked(#[source] Arc<StoreError>),
    /// Explicit close prevents new keys or edits.
    #[error("session is closing")]
    Closing,
    /// Instance was closed.
    #[error("session is closed")]
    Closed,
    /// No new mutations after abnormal owner/worker termination.
    #[error("session is faulted: {0}")]
    Faulted(String),
    /// Another close policy is currently executing.
    #[error("a different close policy is in flight")]
    ClosePolicyConflict,
    /// Prepared creation does not match the original fixed operation.
    #[error("creation parameters conflict with the existing operation")]
    CreationConflict,
    /// Supplied material version is no longer current.
    #[error("stale material version")]
    StaleMaterial,
    /// A configured resource count bound was reached.
    #[error("active session capacity {capacity} reached")]
    Capacity {
        /// Maximum active slots.
        capacity: usize,
    },
    /// Host bounds must be positive and fit the runtime semaphore capacity.
    #[error("host limits must be positive and within runtime semaphore capacity")]
    InvalidOptions,
    /// Work cannot be forgotten before its associated save is handled.
    #[error("work still has an unsettled associated save")]
    SavePending,
    /// This session has no persistent history or is not assembled.
    #[error("context compaction is unavailable for this session")]
    CompactionUnavailable,
    /// Materials are being compacted; new admissions and edits wait.
    #[error("session materials are being compacted")]
    Compacting,
    /// A manual compaction could not be completed.
    #[error("context compaction failed: {0}")]
    Compaction(String),
    /// Requested persistent record does not exist.
    #[error("saved conversation not found")]
    NotFound,
    /// Permission state is not available on this session instance.
    #[error("permission state is not available for this session instance")]
    PermissionUnavailable,
}
impl From<StoreError> for ApplicationError {
    fn from(error: StoreError) -> Self {
        Self::Store(Arc::new(error))
    }
}

/// Managed refusal preserving the complete original request and block identities.
#[derive(Debug, thiserror::Error)]
#[error("managed work rejected: {reason}")]
pub struct ManagedSubmitRejection {
    /// Structured application or low-level reason.
    #[source]
    pub reason: ApplicationError,
    /// Original input, returned without rebuilding identities.
    pub request: SubmitRequest,
}

/// A final record's summary, with per-record corruption and lock observations.
#[derive(Clone, Debug)]
pub struct SessionListing {
    /// Canonical identity from the final filename.
    pub conversation_id: ConversationId,
    /// Definition and revision, or the precise record validation error.
    pub record: Result<SavedSessionSummary, Arc<StoreError>>,
    /// Whether another writer currently holds this record's OS lock.
    pub active: Result<bool, Arc<StoreError>>,
    /// This host's opening or active state, when present.
    pub local: Option<SessionStatus>,
}
/// A validated saved definition and revision without materials or runtime resources.
#[derive(Clone, Debug)]
pub struct SavedSessionSummary {
    /// Frozen execution definition; no credentials are resolved by listing.
    pub definition: SessionDefinition,
    /// Current validated persistent revision.
    pub revision: u64,
    /// Number of Context blocks in the current record.
    pub block_count: usize,
    /// Session title derived from first user message, if any.
    pub title: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch, if derived from UUIDv7 ID.
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Confirmation of a pending commit when explicitly abandoning local materials.
#[derive(Clone, Debug)]
pub enum CommitDisposition {
    /// The exact retained commit is the currently confirmed complete record.
    Applied {
        /// Its confirmed persistent revision.
        revision: u64,
    },
    /// Same writer confirmed its exact old baseline; this pending commit was not applied.
    NotApplied,
    /// The pending snapshot never started any storage attempt.
    NotSubmitted,
    /// Confirmation failed; closing does not claim absence or rollback.
    Unresolved {
        /// Precise confirmation failure retained after releasing the local instance.
        error: ApplicationError,
    },
}

/// Local shutdown outcome distinct from the original work and persistence results.
#[derive(Clone, Debug)]
pub struct CloseOutcome {
    /// Explicit policy used to finish this instance.
    pub policy: ClosePolicy,
    /// Last actual persistence state, updated if close confirmed an existing commit.
    pub save: SaveStatus,
    /// Pending operation confirmation performed during DiscardLocal, when applicable.
    pub pending_commit: Option<CommitDisposition>,
    /// Local materials were abandoned without confirming their snapshot as current.
    pub discarded_unconfirmed: bool,
}
