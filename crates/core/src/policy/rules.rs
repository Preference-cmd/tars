//! Shared command rule provenance vocabulary.

use super::escape::escape_untrusted_text;

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

impl std::fmt::Display for RuleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = escape_untrusted_text(&self.prefix.join(" "));
        write!(f, "{} rule `{prefix}`", self.scope)
    }
}
