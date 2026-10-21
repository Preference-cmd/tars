//! Permission modes, approval channels, and execution pipeline processors.

mod approval;
mod escape;
mod mode;
mod precheck;
mod processor;
mod view;

pub use approval::{
    ApprovalDecision, ApprovalReceiver, ApprovalReply, ApprovalRequest, ApprovalSubject,
    WriteAction,
};
pub use escape::escape_untrusted_text;
pub use mode::{ParsePermissionModeError, PermissionMode};
pub use processor::{PermissionDecision, PermissionDecisionKind, ToolPermissionClass};
pub use view::PermissionView;

pub(crate) use precheck::PrecheckProcessor;
pub(crate) use processor::PermissionProcessor;
pub(crate) use view::PermissionHub;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
