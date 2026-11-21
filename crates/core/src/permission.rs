//! Permission modes, approval channels, and execution pipeline processors.

mod approval;
mod escape;
mod gate;
mod grants;
mod mode;
mod precheck;
mod processor;
pub mod rules;
mod view;

pub use approval::{
    ApprovalDecision, ApprovalReceiver, ApprovalReply, ApprovalRequest, ApprovalSubject,
    CommandTarget, WriteAction, command_body,
};
pub use escape::escape_untrusted_text;
pub use mode::{ParsePermissionModeError, PermissionMode};
pub use processor::{PermissionDecision, PermissionDecisionKind, ToolPermissionClass};
pub use rules::{
    CommandRule, CommandRuleSet, RuleDecision, RuleMatch, RuleScope, RuleSource, parse_command,
    suggest_command_prefixes,
};
pub use view::PermissionView;

pub(crate) use gate::ModeGateProcessor;
pub(crate) use grants::{OutsideSandboxGrant, OutsideSandboxGrants};
pub(crate) use precheck::PrecheckProcessor;
pub(crate) use processor::PermissionProcessor;
#[cfg(test)]
#[cfg_attr(not(unix), allow(unused_imports))]
use processor::{PermissionVerdict, decide};
pub(crate) use view::{McpPermissionTable, McpToolInfo, PermissionHub};

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;

#[cfg(test)]
mod trust_tests;
