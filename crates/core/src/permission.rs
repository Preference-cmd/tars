//! Permission modes, approval channels, and execution pipeline orchestrator.

mod approval;
mod orchestrator;
pub mod rules;
mod view;

pub use crate::policy::{
    ParsePermissionModeError, PermissionDecision, PermissionDecisionKind, PermissionMode,
    RuleScope, RuleSource, ToolPermissionClass, WriteAction, escape_untrusted_text,
};
pub use approval::{
    ApprovalDecision, ApprovalOrigin, ApprovalReceiver, ApprovalReply, ApprovalRequest,
    ApprovalSubject, CommandTarget, command_body,
};
pub use rules::{
    CommandRule, CommandRuleSet, RuleDecision, RuleMatch, parse_command, suggest_command_prefixes,
};
pub use view::PermissionView;

#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
use crate::policy::{PermissionVerdict, decide};
pub(crate) use orchestrator::ToolPolicyProcessor;
pub(crate) use view::{PermissionHub, WorkspaceRulesBackend};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;

#[cfg(test)]
mod trust_tests;
