//! Tool permission classifications, mode verdict evaluation, and decision records.

use super::mode::PermissionMode;
use super::rules::RuleSource;

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

/// Categorization of a tool call for permission mode checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolPermissionClass {
    /// Read-only built-in tool or an MCP tool under writes declaring read_only without destructive.
    KnownReadOnly,
    /// Built-in workspace write tools: `write_file`, `edit_file`.
    BuiltinWrite,
    /// Built-in sandboxed command tool.
    BuiltinCommand,
    /// Built-in command tool requested to run outside the sandbox.
    CommandOutsideSandbox,
    /// MCP tool requiring confirmation: prompt approval, or writes approval without read-only or with destructive.
    McpPrompt,
    /// MCP tool configured with approve approval.
    McpApprove,
}

/// Verdict returned when evaluating a tool category under an active permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionVerdict {
    /// Execute directly without asking.
    Execute,
    /// Ask for confirmation before execution.
    Ask,
    /// Reject the call without asking.
    Reject,
}

/// Evaluates a tool category against the active permission mode according to the policy table.
pub(crate) fn decide(mode: PermissionMode, category: ToolPermissionClass) -> PermissionVerdict {
    match mode {
        PermissionMode::ReadOnly => match category {
            ToolPermissionClass::KnownReadOnly => PermissionVerdict::Execute,
            ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::CommandOutsideSandbox
            | ToolPermissionClass::McpPrompt
            | ToolPermissionClass::McpApprove => PermissionVerdict::Reject,
        },
        PermissionMode::Ask => match category {
            ToolPermissionClass::KnownReadOnly | ToolPermissionClass::McpApprove => {
                PermissionVerdict::Execute
            }
            ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::CommandOutsideSandbox
            | ToolPermissionClass::McpPrompt => PermissionVerdict::Ask,
        },
        PermissionMode::AcceptEdits => match category {
            ToolPermissionClass::KnownReadOnly
            | ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::McpApprove => PermissionVerdict::Execute,
            ToolPermissionClass::CommandOutsideSandbox | ToolPermissionClass::McpPrompt => {
                PermissionVerdict::Ask
            }
        },
        PermissionMode::FullAccess => PermissionVerdict::Execute,
    }
}

/// The specific permission decision recorded for a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDecision {
    /// Mode active when the decision was made.
    pub mode: PermissionMode,
    /// Revision of the permission state when the decision was made.
    pub revision: u64,
    /// Kind of permission decision rendered.
    pub kind: PermissionDecisionKind,
    /// Command rule provenance when a rule governed or modified the decision.
    pub rule: Option<RuleSource>,
}

impl PermissionDecision {
    /// Human-readable provenance summary when a command rule governed or
    /// prompted this decision, suitable for tool progress and transcript lines.
    pub fn rule_summary(&self) -> Option<String> {
        let source = self.rule.as_ref()?;
        match self.kind {
            PermissionDecisionKind::RuleAllowed => Some(format!("allowed by {source}")),
            PermissionDecisionKind::RuleDenied => Some(format!("denied by {source}")),
            PermissionDecisionKind::AskedAndAllowed => Some(format!("asked by {source} (allowed)")),
            PermissionDecisionKind::AskedAndDenied => Some(format!("asked by {source} (denied)")),
            PermissionDecisionKind::NoConfirmationRoute => {
                Some(format!("asked by {source} (no confirmation route)"))
            }
            _ => None,
        }
    }
}

/// Kind of permission decision rendered for a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecisionKind {
    /// Direct execution without asking.
    DirectExecution,
    /// Execution allowed without asking by a command rule.
    RuleAllowed,
    /// Execution approved after asking.
    AskedAndAllowed,
    /// Call rejected after asking.
    AskedAndDenied,
    /// Call rejected directly by permission mode policy.
    ModeRefused,
    /// Call rejected directly by a command rule.
    RuleDenied,
    /// Write call rejected because no prepared write record was found.
    Unprepared,
    /// Outside-sandbox call rejected because its parameters could not be
    /// decoded for permission handling.
    UnpreparedOutsideSandbox,
    /// Call rejected because no confirmation route was available.
    NoConfirmationRoute,
    /// Call rejected because the tool is unknown or unavailable.
    Unavailable,
}
