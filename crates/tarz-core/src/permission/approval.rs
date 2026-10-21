//! Approval requests, decisions, and reviewer connection channel.

use super::mode::PermissionMode;
use crate::session::WorkRef;
use causa::kernel::BlockId;
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

/// Decision returned by a reviewer answering an approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow the tool call to proceed to execution.
    Allow,
    /// Deny the tool call, optionally providing a reason.
    Deny {
        /// Optional explanation for why the operation was declined.
        reason: Option<String>,
    },
}

/// Action performed by a file write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteAction {
    /// A new file was created.
    Created,
    /// An existing file was updated.
    Updated,
}

impl WriteAction {
    /// Canonical action text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
        }
    }
}

impl std::fmt::Display for WriteAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// The subject of an approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalSubject {
    /// File write operation.
    FileWrite {
        /// Target file path relative to workspace root.
        path: String,
        /// Whether the file is being created or updated.
        action: WriteAction,
        /// Unified diff showing the proposed changes.
        diff: String,
    },
}

/// Single-use handle for replying to an approval request.
pub struct ApprovalReply {
    sender: oneshot::Sender<ApprovalDecision>,
}

impl ApprovalReply {
    pub(crate) fn new(sender: oneshot::Sender<ApprovalDecision>) -> Self {
        Self { sender }
    }

    /// Send a decision to the waiting processor.
    pub fn send(self, decision: ApprovalDecision) -> Result<(), ApprovalDecision> {
        self.sender.send(decision)
    }
}

/// An approval request generated when a tool call requires confirmation.
pub struct ApprovalRequest {
    /// Work reference that declared the tool call.
    pub work: WorkRef,
    /// Identity of the tool call declaration.
    pub call_block_id: BlockId,
    /// Name of the tool being called.
    pub tool_name: String,
    /// Active permission mode when this decision was initiated.
    pub mode: PermissionMode,
    /// Revision of the permission state when this decision was initiated.
    pub revision: u64,
    /// The specific operation details requiring confirmation.
    pub subject: ApprovalSubject,
    /// Single-use handle for delivering the reviewer's decision.
    pub reply: ApprovalReply,
}

/// Receiver endpoint for approval requests from a session instance.
pub type ApprovalReceiver = mpsc::Receiver<ApprovalRequest>;

/// Hub managing the single active approval receiver connection.
pub(crate) struct ApprovalsHub {
    sender: Mutex<Option<mpsc::Sender<ApprovalRequest>>>,
}

impl ApprovalsHub {
    pub(crate) fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    pub(crate) fn connect(&self) -> ApprovalReceiver {
        let (tx, rx) = mpsc::channel(16);
        let mut guard = self.sender.lock().expect("approvals lock");
        *guard = Some(tx);
        rx
    }

    pub(crate) fn get_sender(&self) -> Option<mpsc::Sender<ApprovalRequest>> {
        self.sender.lock().expect("approvals lock").clone()
    }
}
