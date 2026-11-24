//! Approval requests, decisions, and reviewer connection channel.

use crate::definition::CommandNetwork;
use crate::policy::{PermissionMode, WriteAction, escape_untrusted_text};
use crate::session::WorkRef;
use causa::kernel::BlockId;
use std::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

/// Decision returned by a reviewer answering an approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Allow the tool call to proceed to execution once.
    Allow,
    /// Allow matching command prefixes for the remainder of the current in-memory session.
    AllowForSession,
    /// Persist matching command prefixes to `<workspace_root>/.tars/permissions.toml`
    /// and allow matching commands in this workspace.
    AllowInWorkspace,
    /// Deny the tool call, optionally providing a reason.
    Deny {
        /// Optional explanation for why the operation was declined.
        reason: Option<String>,
    },
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
        /// Candidate rule prefixes for session/workspace allow rules (empty if
        /// unmatchable or a single-word interpreter/wrapper).
        suggested_prefixes: Vec<Vec<String>>,
        /// Whether saving to `<workspace_root>/.tars/permissions.toml` is available
        /// (requires non-empty `suggested_prefixes`, a workspace root, a Store, and
        /// currently trusted workspace rules).
        can_save_workspace_rule: bool,
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
        suggested_prefixes,
        ..
    } = subject
    else {
        return None;
    };
    let mut lines = Vec::new();
    let is_outside = matches!(target, CommandTarget::Outside { .. });
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
    if !suggested_prefixes.is_empty() {
        let formatted: Vec<String> = suggested_prefixes
            .iter()
            .map(|prefix| escape_untrusted_text(&prefix.join(" ")))
            .collect();
        let label = if formatted.len() == 1 {
            "Rule prefix"
        } else {
            "Rule prefixes"
        };
        let suffix = if is_outside {
            " (includes outside_sandbox = true)"
        } else {
            ""
        };
        lines.push(format!("{label}: {}{suffix}", formatted.join("; ")));
        if is_outside {
            lines.push(
                "Rule note: allows any trailing args; outside_sandbox = true bypasses sandbox."
                    .to_owned(),
            );
        } else {
            lines.push("Rule note: allows any trailing args.".to_owned());
        }
    }
    lines.push(String::new());
    let escaped_command = escape_untrusted_text(command);
    let mut command = escaped_command.lines();
    lines.push(format!("$ {}", command.next().unwrap_or_default()));
    lines.extend(command.map(str::to_owned));
    Some(lines)
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

/// Origin metadata identifying the agent and operation that initiated an approval request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalOrigin {
    /// Canonical identifier of the agent requesting confirmation.
    pub agent_id: String,
    /// Display name of the agent requesting confirmation.
    pub agent_name: String,
    /// Canonical identifier of the operation requesting confirmation.
    pub operation_id: String,
}

impl ApprovalOrigin {
    /// Construct an explicit approval origin.
    pub fn new(
        agent_id: impl Into<String>,
        agent_name: impl Into<String>,
        operation_id: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            agent_name: agent_name.into(),
            operation_id: operation_id.into(),
        }
    }

    /// Derive default origin metadata from a single-agent [`WorkRef`].
    pub fn for_work(work: &WorkRef) -> Self {
        Self {
            agent_id: work.conversation_id.as_str().to_owned(),
            agent_name: "main".to_owned(),
            operation_id: work.turn_id.0.clone(),
        }
    }
}

/// An approval request generated when a tool call requires confirmation.
pub struct ApprovalRequest {
    /// Work reference that declared the tool call.
    pub work: WorkRef,
    /// Canonical identifier of the agent requesting confirmation.
    pub agent_id: String,
    /// Display name of the agent requesting confirmation.
    pub agent_name: String,
    /// Canonical identifier of the operation requesting confirmation.
    pub operation_id: String,
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

    #[test]
    fn command_body_describes_both_targets_and_escapes_untrusted_text() {
        let sandboxed = ApprovalSubject::Command {
            command: "printf a\nprintf b".into(),
            workdir: "src".into(),
            timeout_secs: 30,
            target: CommandTarget::Sandboxed {
                network: CommandNetwork::None,
            },
            suggested_prefixes: Vec::new(),
            can_save_workspace_rule: false,
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
            suggested_prefixes: vec![vec!["printf".into(), "a".into()]],
            can_save_workspace_rule: true,
        };
        let lines = command_body(&outside).unwrap();
        assert!(lines[0].starts_with("NOT in the sandbox"));
        assert_eq!(lines[1], "Justification: line one");
        assert_eq!(lines[2], "line two\\x1b");
        assert_eq!(lines[3], "Working directory: .");
        assert_eq!(lines[4], "Timeout: 120s");
        assert_eq!(
            lines[5],
            "Rule prefix: printf a (includes outside_sandbox = true)"
        );
        assert!(lines[6].contains("outside_sandbox = true bypasses sandbox"));
        assert_eq!(lines[7], "");
        assert_eq!(lines[8], "$ printf a");

        let mcp = ApprovalSubject::McpCall {
            server: "srv".into(),
            tool: "tool".into(),
            arguments: "{}".into(),
        };
        assert!(command_body(&mcp).is_none());
    }
}
