//! Approval requests, decisions, and reviewer connection channel.

use super::escape::escape_untrusted_text;
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

/// Where a confirmed command would run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandTarget {
    /// Inside the sandbox, with the session's network policy.
    Sandboxed {
        /// Network policy for this command.
        network: CommandNetwork,
    },
    /// Outside the sandbox, with the model's justification.
    Outside {
        /// The model's reason for requesting an outside run.
        justification: String,
    },
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
    /// Shell command requiring confirmation.
    Command {
        /// Command text as provided by the model.
        command: String,
        /// Workspace-relative working directory, or "." for the root.
        workdir: String,
        /// Effective timeout in seconds.
        timeout_secs: u64,
        /// Where the command would run and what governs it.
        target: CommandTarget,
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

/// Body lines for a command confirmation, with untrusted text escaped.
///
/// Renderers should use this instead of laying out the fields themselves, so
/// the visible body and its scroll range cannot drift apart. Returns `None`
/// for non-command subjects.
pub fn command_body(subject: &ApprovalSubject) -> Option<Vec<String>> {
    let ApprovalSubject::Command {
        command,
        workdir,
        timeout_secs,
        target,
    } = subject
    else {
        return None;
    };
    let mut lines = Vec::new();
    match target {
        CommandTarget::Outside { justification } => {
            lines.push(
                "NOT in the sandbox: full disk read/write, network and credentials are available"
                    .to_owned(),
            );
            let escaped_justification = escape_untrusted_text(justification);
            let mut justification = escaped_justification.lines();
            lines.push(format!(
                "Justification: {}",
                justification.next().unwrap_or_default()
            ));
            lines.extend(justification.map(str::to_owned));
            lines.push(format!(
                "Working directory: {}",
                escape_untrusted_text(workdir)
            ));
            lines.push(format!("Timeout: {timeout_secs}s"));
        }
        CommandTarget::Sandboxed { network } => {
            lines.push(format!(
                "Working directory: {}",
                escape_untrusted_text(workdir)
            ));
            lines.push(format!("Timeout: {timeout_secs}s"));
            lines.push(format!("Network: {network}"));
            lines.push(
                "Workspace: writable; .git hooks and config read-only; credentials unreadable"
                    .to_owned(),
            );
        }
    }
    lines.push(String::new());
    let escaped_command = escape_untrusted_text(command);
    let mut command = escaped_command.lines();
    lines.push(format!("$ {}", command.next().unwrap_or_default()));
    lines.extend(command.map(str::to_owned));
    Some(lines)
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
    fn command_body_describes_both_targets_and_escapes_untrusted_text() {
        let sandboxed = ApprovalSubject::Command {
            command: "printf a\nprintf b".into(),
            workdir: "src".into(),
            timeout_secs: 30,
            target: CommandTarget::Sandboxed {
                network: CommandNetwork::None,
            },
        };
        let lines = command_body(&sandboxed).unwrap();
        assert_eq!(lines[0], "Working directory: src");
        assert_eq!(lines[1], "Timeout: 30s");
        assert_eq!(lines[2], "Network: none");
        assert!(lines[3].contains(".git hooks and config read-only"));
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "$ printf a");
        assert_eq!(lines[6], "printf b");

        let outside = ApprovalSubject::Command {
            command: "printf a".into(),
            workdir: ".".into(),
            timeout_secs: 120,
            target: CommandTarget::Outside {
                justification: "line one\nline two\x1b".into(),
            },
        };
        let lines = command_body(&outside).unwrap();
        assert!(lines[0].starts_with("NOT in the sandbox"));
        assert_eq!(lines[1], "Justification: line one");
        assert_eq!(lines[2], "line two\\x1b");
        assert_eq!(lines[3], "Working directory: .");
        assert_eq!(lines[4], "Timeout: 120s");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "$ printf a");

        let mcp = ApprovalSubject::McpCall {
            server: "srv".into(),
            tool: "tool".into(),
            arguments: "{}".into(),
        };
        assert!(command_body(&mcp).is_none());
    }

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
