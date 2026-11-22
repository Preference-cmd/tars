//! Command approval rules: conservative shell parsing, TOML loading, and evaluation.

mod eval;
mod file;
mod parse;

#[cfg(test)]
mod tests;

pub use crate::policy::{RuleScope, RuleSource};
pub use eval::{CommandRule, CommandRuleSet, RuleDecision, RuleMatch};
pub(crate) use eval::{EvaluatedPermission, apply_command_rule};
pub use file::{
    FORBIDDEN_SINGLE_WORD_ALLOW, LoadedRulesFile, MAX_RULES_COUNT, MAX_RULES_FILE_BYTES,
    RulesFileError, format_command_rule_block, load_rules_file, parse_rules_toml,
    workspace_rules_diff,
};
pub(crate) use file::{WorkspaceAppendError, append_workspace_rules_file};
pub use parse::{parse_command, suggest_command_prefixes};
