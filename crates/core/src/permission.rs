//! Permission modes, approval channels, and execution pipeline processors.

mod approval;
mod gate;
mod grants;
mod precheck;
mod processor;
pub mod rules;
mod view;

pub use crate::policy::{
    ParsePermissionModeError, PermissionDecision, PermissionDecisionKind, PermissionMode,
    RuleScope, RuleSource, ToolPermissionClass, WriteAction, escape_untrusted_text,
};
pub use approval::{
    ApprovalDecision, ApprovalReceiver, ApprovalReply, ApprovalRequest, ApprovalSubject,
    CommandTarget, command_body,
};
pub use rules::{
    CommandRule, CommandRuleSet, RuleDecision, RuleMatch, parse_command, suggest_command_prefixes,
};
pub use view::PermissionView;

#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
use crate::policy::{PermissionVerdict, decide};
pub(crate) use gate::ModeGateProcessor;
pub(crate) use grants::{OutsideSandboxGrant, OutsideSandboxGrants};
pub(crate) use precheck::PrecheckProcessor;
pub(crate) use processor::PermissionProcessor;
pub(crate) use view::{PermissionHub, WorkspaceRulesBackend};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;

#[cfg(test)]
mod trust_tests;
