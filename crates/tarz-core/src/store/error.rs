use std::{io, sync::Arc};

/// Stage determines whether replacement could have occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStage {
    /// Serialize stage of the snapshot commit.
    Serialize,
    /// Write stage of the snapshot commit.
    Write,
    /// Sync stage of the snapshot commit.
    Sync,
    /// Replace stage of the snapshot commit.
    Replace,
    /// Confirm stage of the snapshot commit.
    Confirm,
    /// Reply stage of the snapshot commit.
    Reply,
}

/// Structured original causes survive cloning into host observations.
#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    /// An identity is not a lowercase hyphenated UUID.
    #[error("invalid canonical UUID: {0}")]
    InvalidIdentity(String),
    /// Another activity owns the stable lock file.
    #[error("session writer is occupied: {0}")]
    Occupied(String),
    /// The platform or filesystem does not support locking.
    #[error("file locking is unsupported: {0}")]
    LockUnsupported(#[source] Arc<io::Error>),
    /// A read, directory or lock operation failed.
    #[error("session store I/O failed: {0}")]
    Io(#[source] Arc<io::Error>),
    /// JSON decoding failed, including duplicate Context block identities.
    #[error("invalid session JSON: {0}")]
    Json(#[source] Arc<serde_json::Error>),
    /// The record names a format this implementation cannot read.
    #[error("unsupported session format: {0}")]
    UnsupportedFormat(String),
    /// The record belongs to a different conversation.
    #[error("record identity {actual} does not match {expected}")]
    IdentityMismatch {
        /// Expected conversation identity.
        expected: String,
        /// Identity found inside the record.
        actual: String,
    },
    /// The saved execution choices are invalid.
    #[error("invalid session definition: {0}")]
    Definition(#[source] Arc<crate::definition::DefinitionError>),
    /// Material contains an unsupported external media or artifact reference.
    #[error("session reference validation failed: {0}")]
    References(#[source] Arc<crate::material::MaterialError>),
    /// The full serialized record exceeds the configured byte limit.
    #[error("record exceeds the {limit}-byte resource limit")]
    RecordTooLarge {
        /// Maximum accepted bytes.
        limit: usize,
    },
    /// Revision must be the checked successor of its baseline.
    #[error("revision must be the checked successor of its baseline")]
    InvalidRevision,
    /// The complete baseline or its revision changed.
    #[error("session revision conflict: expected {expected:?}, actual {actual:?}")]
    Conflict {
        /// Required prior revision.
        expected: Option<u64>,
        /// Latest actual revision, if any.
        actual: Option<u64>,
    },
    /// The same commit identity was supplied with different content.
    #[error("the same commit identity was supplied with different content")]
    CommitIdentityConflict,
    /// A different unconfirmed commit is already pending.
    #[error("a different unconfirmed commit is already pending")]
    PendingCommit,
    /// The current record cannot establish whether this commit was applied.
    #[error("the current record cannot establish whether this commit was applied")]
    ConfirmationUnknown,
    /// Store writer coordination was poisoned.
    #[error("store writer coordination was poisoned")]
    Poisoned,
    /// An I/O stage failed; replacement-stage and later failures need confirmation.
    #[error("commit failed at {stage:?}: {source}")]
    CommitIo {
        /// Stage whose operation failed.
        stage: CommitStage,
        /// Original I/O cause.
        #[source]
        source: Arc<io::Error>,
    },
}

impl From<io::Error> for StoreError {
    fn from(source: io::Error) -> Self {
        Self::Io(Arc::new(source))
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(Arc::new(source))
    }
}
