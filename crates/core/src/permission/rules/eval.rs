//! Command rule matching, three-layer merging, and permission mode composition.

use super::parse::parse_command;
use crate::permission::processor::{PermissionDecisionKind, PermissionVerdict};
use serde::Deserialize;

/// Decision configured by a command rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    /// Allow matching commands without prompting (except in `read-only` mode).
    Allow,
    /// Require confirmation for matching commands (even in `accept-edits` or `full-access`).
    Ask,
    /// Reject matching commands in every mode (including `full-access`).
    Deny,
}

/// Layer from which a command rule originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RuleScope {
    /// User-level rules from `<user_tars_dir>/permissions.toml`.
    User,
    /// Workspace-level rules from `<workspace_root>/.tars/permissions.toml`.
    Workspace,
    /// In-memory rules added during the current session.
    Session,
}

impl RuleScope {
    /// Canonical lowercase label for this rule scope.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }
}

impl std::fmt::Display for RuleScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Provenance of a matched command rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSource {
    /// Scope layer that supplied the rule.
    pub scope: RuleScope,
    /// Prefix words of the matched rule.
    pub prefix: Vec<String>,
}

/// A single validated command prefix rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRule {
    /// Exact word prefix that must match the leading words of a simple command.
    pub prefix: Vec<String>,
    /// Rule decision when matched.
    pub decision: RuleDecision,
    /// Whether an `allow` rule also applies when `outside_sandbox = true`.
    pub outside_sandbox: bool,
}

/// Effective outcome of evaluating a command against a [`CommandRuleSet`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleMatch {
    /// Effective merged decision across all simple commands and layers.
    pub decision: RuleDecision,
    /// Deterministic representative rule source for this decision.
    pub source: RuleSource,
}

/// Layered set of user, workspace, and in-memory session command rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandRuleSet {
    user: Vec<CommandRule>,
    workspace: Vec<CommandRule>,
    session: Vec<CommandRule>,
}

impl CommandRuleSet {
    /// Create an empty rule set across all layers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a rule set from explicit user, workspace, and session rule slices.
    pub fn with_layers(
        user: Vec<CommandRule>,
        workspace: Vec<CommandRule>,
        session: Vec<CommandRule>,
    ) -> Self {
        Self {
            user,
            workspace,
            session,
        }
    }

    /// User-level rules in file order.
    pub fn user_rules(&self) -> &[CommandRule] {
        &self.user
    }

    /// Active workspace-level rules in file order.
    pub fn workspace_rules(&self) -> &[CommandRule] {
        &self.workspace
    }

    /// In-memory session-level rules in insertion order.
    pub fn session_rules(&self) -> &[CommandRule] {
        &self.session
    }

    /// Replace the user-level rules.
    pub fn set_user_rules(&mut self, rules: Vec<CommandRule>) {
        self.user = rules;
    }

    /// Replace the active workspace-level rules.
    pub fn set_workspace_rules(&mut self, rules: Vec<CommandRule>) {
        self.workspace = rules;
    }

    /// Append an in-memory session rule if an identical rule is not already present.
    pub fn add_session_rule(&mut self, rule: CommandRule) {
        if !self.session.contains(&rule) {
            self.session.push(rule);
        }
    }

    /// Remove an in-memory session rule by index, returning it if present.
    pub fn remove_session_rule(&mut self, index: usize) -> Option<CommandRule> {
        if index < self.session.len() {
            Some(self.session.remove(index))
        } else {
            None
        }
    }

    /// Clear all in-memory session rules.
    pub fn clear_session_rules(&mut self) {
        self.session.clear();
    }

    /// Append a rule to the in-memory workspace layer if not already present.
    pub fn add_workspace_rule(&mut self, rule: CommandRule) {
        if !self.workspace.contains(&rule) {
            self.workspace.push(rule);
        }
    }

    /// Evaluate `command` against all three rule layers using strict
    /// `deny > ask > allow` merging across all simple commands.
    ///
    /// Returns `None` when `command` cannot be parsed conservatively or when no
    /// merged rule decision applies.
    pub fn evaluate(&self, command: &str, outside_sandbox: bool) -> Option<RuleMatch> {
        let segments = parse_command(command)?;
        let layers = [
            (RuleScope::User, self.user.as_slice()),
            (RuleScope::Workspace, self.workspace.as_slice()),
            (RuleScope::Session, self.session.as_slice()),
        ];

        // 1. Any segment matching a `deny` rule in any layer denies the whole command.
        for (scope, rules) in layers {
            for rule in rules {
                if rule.decision == RuleDecision::Deny
                    && segments
                        .iter()
                        .any(|segment| rule_matches_segment(rule, segment))
                {
                    return Some(RuleMatch {
                        decision: RuleDecision::Deny,
                        source: RuleSource {
                            scope,
                            prefix: rule.prefix.clone(),
                        },
                    });
                }
            }
        }

        // 2. Otherwise any segment matching an `ask` rule in any layer asks.
        for (scope, rules) in layers {
            for rule in rules {
                if rule.decision == RuleDecision::Ask
                    && segments
                        .iter()
                        .any(|segment| rule_matches_segment(rule, segment))
                {
                    return Some(RuleMatch {
                        decision: RuleDecision::Ask,
                        source: RuleSource {
                            scope,
                            prefix: rule.prefix.clone(),
                        },
                    });
                }
            }
        }

        // 3. Otherwise every segment must match at least one effective `allow` rule.
        let mut representative: Option<RuleSource> = None;
        for segment in &segments {
            let mut segment_source: Option<RuleSource> = None;
            'search: for (scope, rules) in layers {
                for rule in rules {
                    if rule.decision == RuleDecision::Allow
                        && (!outside_sandbox || rule.outside_sandbox)
                        && rule_matches_segment(rule, segment)
                    {
                        segment_source = Some(RuleSource {
                            scope,
                            prefix: rule.prefix.clone(),
                        });
                        break 'search;
                    }
                }
            }
            let source = segment_source?;
            if representative.is_none() {
                representative = Some(source);
            }
        }

        representative.map(|source| RuleMatch {
            decision: RuleDecision::Allow,
            source,
        })
    }
}

/// Result of combining the base permission mode verdict with a command rule match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EvaluatedPermission {
    /// Effective verdict (`Execute`, `Ask`, or `Reject`).
    pub(crate) verdict: PermissionVerdict,
    /// Immediate decision kind when `verdict` does not prompt (`DirectExecution`,
    /// `RuleAllowed`, `ModeRefused`, or `RuleDenied`).
    pub(crate) fixed_kind: Option<PermissionDecisionKind>,
    /// Rule provenance when a rule governed or modified the verdict.
    pub(crate) rule: Option<RuleSource>,
}

/// Combine the base permission mode verdict with a command rule match.
///
/// Both `ModeGateProcessor` and `PermissionProcessor` evaluate calls through
/// this single transition table so gating and execution never diverge.
pub(crate) fn apply_command_rule(
    base: PermissionVerdict,
    matched: Option<RuleMatch>,
) -> EvaluatedPermission {
    match matched {
        Some(RuleMatch {
            decision: RuleDecision::Deny,
            source,
        }) => EvaluatedPermission {
            verdict: PermissionVerdict::Reject,
            fixed_kind: Some(PermissionDecisionKind::RuleDenied),
            rule: Some(source),
        },
        Some(RuleMatch {
            decision: RuleDecision::Ask,
            source,
        }) => match base {
            PermissionVerdict::Execute | PermissionVerdict::Ask => EvaluatedPermission {
                verdict: PermissionVerdict::Ask,
                fixed_kind: None,
                rule: Some(source),
            },
            PermissionVerdict::Reject => EvaluatedPermission {
                verdict: PermissionVerdict::Reject,
                fixed_kind: Some(PermissionDecisionKind::ModeRefused),
                rule: None,
            },
        },
        Some(RuleMatch {
            decision: RuleDecision::Allow,
            source,
        }) => match base {
            PermissionVerdict::Ask => EvaluatedPermission {
                verdict: PermissionVerdict::Execute,
                fixed_kind: Some(PermissionDecisionKind::RuleAllowed),
                rule: Some(source),
            },
            PermissionVerdict::Execute => EvaluatedPermission {
                verdict: PermissionVerdict::Execute,
                fixed_kind: Some(PermissionDecisionKind::DirectExecution),
                rule: None,
            },
            PermissionVerdict::Reject => EvaluatedPermission {
                verdict: PermissionVerdict::Reject,
                fixed_kind: Some(PermissionDecisionKind::ModeRefused),
                rule: None,
            },
        },
        None => match base {
            PermissionVerdict::Execute => EvaluatedPermission {
                verdict: PermissionVerdict::Execute,
                fixed_kind: Some(PermissionDecisionKind::DirectExecution),
                rule: None,
            },
            PermissionVerdict::Ask => EvaluatedPermission {
                verdict: PermissionVerdict::Ask,
                fixed_kind: None,
                rule: None,
            },
            PermissionVerdict::Reject => EvaluatedPermission {
                verdict: PermissionVerdict::Reject,
                fixed_kind: Some(PermissionDecisionKind::ModeRefused),
                rule: None,
            },
        },
    }
}

fn rule_matches_segment(rule: &CommandRule, segment: &[String]) -> bool {
    !rule.prefix.is_empty()
        && segment.len() >= rule.prefix.len()
        && segment[..rule.prefix.len()] == rule.prefix[..]
}
