//! Shared permission policy vocabulary, mode decisions, and untrusted-text escaping.

mod decision;
mod escape;
mod mode;
mod rules;

pub use decision::{PermissionDecision, PermissionDecisionKind, ToolPermissionClass, WriteAction};
pub(crate) use decision::{PermissionVerdict, decide};
pub use escape::escape_untrusted_text;
pub use mode::{ParsePermissionModeError, PermissionMode};
pub use rules::{RuleScope, RuleSource};
