//! Approval requests, decisions, and reviewer connection channel.

use super::mode::PermissionMode;
use crate::definition::CommandNetwork;
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
    /// Sandboxed shell command requiring confirmation.
    Command {
        /// Command text as provided by the model.
        command: String,
        /// Workspace-relative working directory, or "." for the root.
        workdir: String,
        /// Effective timeout in seconds.
        timeout_secs: u64,
        /// Network policy for this command.
        network: CommandNetwork,
    },
    /// MCP tool call requiring confirmation.
    McpCall {
        /// Server identifier configured in `mcp.json`.
        server: String,
        /// Original tool name advertised by the MCP server.
        tool: String,
        /// Compact JSON arguments, bounded to 1024 UTF-8 bytes with an ellipsis when truncated.
        arguments: String,
    },
}

/// Formats arguments JSON compactly, truncated at a UTF-8 character boundary to at most 1024 bytes.
/// If truncated, an ellipsis is appended.
pub(crate) fn format_mcp_arguments(value: &serde_json::Value) -> String {
    let full = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if full.len() <= 1024 {
        full
    } else {
        let mut end = 1024;
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &full[..end])
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_mcp_arguments_within_limit_outputs_compact_json() {
        let val = json!({
            "key": "value",
            "nested": {
                "num": 42,
                "bool": true,
            }
        });
        let formatted = format_mcp_arguments(&val);
        assert_eq!(
            formatted,
            r#"{"key":"value","nested":{"bool":true,"num":42}}"#
        );
        assert!(!formatted.ends_with('…'));
        assert!(formatted.len() <= 1024);
    }

    #[test]
    fn format_mcp_arguments_exceeding_1024_bytes_truncates_with_ellipsis() {
        let long_string = "a".repeat(1200);
        let val = json!({ "data": long_string });
        let formatted = format_mcp_arguments(&val);
        assert!(formatted.ends_with('…'));
        let without_ellipsis = formatted.strip_suffix('…').unwrap();
        assert_eq!(without_ellipsis.len(), 1024);
    }

    #[test]
    fn format_mcp_arguments_multibyte_utf8_boundary_truncation() {
        // The JSON prefix is 8 bytes, so 1015 'a's put the first byte of 中
        // at 1023 and its next byte at 1024, inside the character.
        let padding = "a".repeat(1015);
        let val = json!({ "msg": format!("{padding}中extra") });
        let full = serde_json::to_string(&val).unwrap();
        assert!(!full.is_char_boundary(1024));

        let formatted = format_mcp_arguments(&val);
        assert!(formatted.ends_with('…'));
        let without_ellipsis = formatted.strip_suffix('…').unwrap();
        assert_eq!(without_ellipsis.len(), 1023);
        assert!(without_ellipsis.is_char_boundary(without_ellipsis.len()));
        assert_eq!(&without_ellipsis[without_ellipsis.len() - 1..], "a");
    }
}
