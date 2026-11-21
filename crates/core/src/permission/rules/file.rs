//! Loading, strict validation, and TOML formatting for command permission rule files.

use super::eval::{CommandRule, RuleDecision};
use serde::Deserialize;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Maximum size of a permissions rules file (256 KiB).
pub const MAX_RULES_FILE_BYTES: usize = 256 * 1024;

/// Maximum number of `[[command]]` rules allowed in a single rules file.
pub const MAX_RULES_COUNT: usize = 1000;

/// Interpreter and wrapper commands that cannot be allowed as a single-word prefix.
pub const FORBIDDEN_SINGLE_WORD_ALLOW: &[&str] = &[
    "bash", "sh", "zsh", "env", "xargs", "find", "sudo", "eval", "exec", "command", "nohup",
    "time", "timeout", "nice", "python", "python3", "node", "ruby", "perl",
];

/// Validated rules file together with its exact raw UTF-8 text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedRulesFile {
    /// Validated command rules in file order.
    pub rules: Vec<CommandRule>,
    /// Exact raw file text for byte-for-byte trust comparison and diffing.
    pub raw_text: String,
}

/// Errors returned when loading or validating a permissions rules file.
///
/// Error messages never echo TOML values or rule contents.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RulesFileError {
    /// File access failed, without echoing its contents.
    #[error("permissions file {path:?}: {kind:?}")]
    Io {
        /// Requested rules file path.
        path: PathBuf,
        /// Original filesystem error category.
        kind: io::ErrorKind,
    },
    /// Invalid TOML syntax, duplicate key, unknown field, or wrong value type.
    #[error(
        "permissions file {path:?}: invalid TOML or rule field at line {line}, column {column}"
    )]
    Syntax {
        /// Rules file path.
        path: PathBuf,
        /// One-based line number.
        line: usize,
        /// One-based column number.
        column: usize,
    },
    /// File exceeds the 256 KiB bound.
    #[error("permissions file {path:?}: exceeds 256 KiB")]
    TooLarge {
        /// Rules file path.
        path: PathBuf,
    },
    /// File contains more than 1000 command rules.
    #[error("permissions file {path:?}: exceeds 1000 command rules")]
    TooManyRules {
        /// Rules file path.
        path: PathBuf,
    },
    /// Unsupported `version` field (must be 1).
    #[error("permissions file {path:?}: unsupported rules version; expected 1")]
    UnsupportedVersion {
        /// Rules file path.
        path: PathBuf,
    },
    /// A `[[command]]` entry has an empty `prefix` array.
    #[error("permissions file {path:?}: command rule #{index} has an empty prefix")]
    EmptyPrefix {
        /// Rules file path.
        path: PathBuf,
        /// One-based rule index.
        index: usize,
    },
    /// A `[[command]]` entry contains an empty or NUL-containing prefix word.
    #[error(
        "permissions file {path:?}: command rule #{index} contains an empty or NUL prefix word"
    )]
    InvalidPrefixWord {
        /// Rules file path.
        path: PathBuf,
        /// One-based rule index.
        index: usize,
    },
    /// `outside_sandbox = true` was combined with a non-`allow` decision.
    #[error(
        "permissions file {path:?}: command rule #{index} sets outside_sandbox = true without decision = \"allow\""
    )]
    OutsideSandboxRequiresAllow {
        /// Rules file path.
        path: PathBuf,
        /// One-based rule index.
        index: usize,
    },
    /// A single-word `allow` rule targets an interpreter or wrapper command.
    #[error(
        "permissions file {path:?}: command rule #{index} cannot allow a single-word interpreter or wrapper command"
    )]
    ForbiddenSingleWordAllow {
        /// Rules file path.
        path: PathBuf,
        /// One-based rule index.
        index: usize,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRulesFile {
    version: u32,
    #[serde(default)]
    command: Vec<RawCommandRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommandRule {
    prefix: Vec<String>,
    decision: RuleDecision,
    #[serde(default)]
    outside_sandbox: Option<bool>,
}

/// Load and strictly validate a permissions rules file.
///
/// Returns `Ok(None)` when `path` does not exist.
pub fn load_rules_file(path: &Path) -> Result<Option<LoadedRulesFile>, RulesFileError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RulesFileError::Io {
                path: path.to_owned(),
                kind: error.kind(),
            });
        }
    };

    let mut bytes = Vec::new();
    file.take((MAX_RULES_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| RulesFileError::Io {
            path: path.to_owned(),
            kind: error.kind(),
        })?;
    if bytes.len() > MAX_RULES_FILE_BYTES {
        return Err(RulesFileError::TooLarge {
            path: path.to_owned(),
        });
    }

    let raw_text = String::from_utf8(bytes).map_err(|_| RulesFileError::Io {
        path: path.to_owned(),
        kind: io::ErrorKind::InvalidData,
    })?;

    let rules = parse_rules_toml(&raw_text, path)?;
    Ok(Some(LoadedRulesFile { rules, raw_text }))
}

/// Parse and strictly validate rules TOML text, attributing errors to `path`.
pub fn parse_rules_toml(text: &str, path: &Path) -> Result<Vec<CommandRule>, RulesFileError> {
    if text.len() > MAX_RULES_FILE_BYTES {
        return Err(RulesFileError::TooLarge {
            path: path.to_owned(),
        });
    }

    let raw: RawRulesFile = toml::from_str(text).map_err(|error: toml::de::Error| {
        let offset = error.span().map_or(0, |span| span.start).min(text.len());
        let prefix = &text[..text.floor_char_boundary(offset)];
        RulesFileError::Syntax {
            path: path.to_owned(),
            line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        }
    })?;

    if raw.version != 1 {
        return Err(RulesFileError::UnsupportedVersion {
            path: path.to_owned(),
        });
    }

    if raw.command.len() > MAX_RULES_COUNT {
        return Err(RulesFileError::TooManyRules {
            path: path.to_owned(),
        });
    }

    let mut rules = Vec::with_capacity(raw.command.len());
    for (idx, entry) in raw.command.into_iter().enumerate() {
        let index = idx + 1;
        if entry.prefix.is_empty() {
            return Err(RulesFileError::EmptyPrefix {
                path: path.to_owned(),
                index,
            });
        }
        if entry
            .prefix
            .iter()
            .any(|word| word.is_empty() || word.contains('\0'))
        {
            return Err(RulesFileError::InvalidPrefixWord {
                path: path.to_owned(),
                index,
            });
        }
        let outside_sandbox = entry.outside_sandbox.unwrap_or(false);
        if outside_sandbox && entry.decision != RuleDecision::Allow {
            return Err(RulesFileError::OutsideSandboxRequiresAllow {
                path: path.to_owned(),
                index,
            });
        }
        if entry.decision == RuleDecision::Allow
            && entry.prefix.len() == 1
            && is_forbidden_single_word_allow(&entry.prefix[0])
        {
            return Err(RulesFileError::ForbiddenSingleWordAllow {
                path: path.to_owned(),
                index,
            });
        }

        rules.push(CommandRule {
            prefix: entry.prefix,
            decision: entry.decision,
            outside_sandbox,
        });
    }

    Ok(rules)
}

/// Format a [`CommandRule`] as a standalone `[[command]]` TOML block suitable
/// for appending to `.tars/permissions.toml`.
pub fn format_command_rule_block(rule: &CommandRule) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    out.push_str("[[command]]\nprefix = [");
    for (idx, word) in rule.prefix.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        write_toml_basic_string(&mut out, word);
    }
    let decision = match rule.decision {
        RuleDecision::Allow => "allow",
        RuleDecision::Ask => "ask",
        RuleDecision::Deny => "deny",
    };
    let _ = writeln!(out, "]\ndecision = \"{decision}\"");
    if rule.outside_sandbox {
        out.push_str("outside_sandbox = true\n");
    }
    out
}

/// Compute a unified diff between `previous_trusted` and `current_text` for
/// `<workspace_root>/.tars/permissions.toml`.
pub fn workspace_rules_diff(previous_trusted: &str, current_text: &str) -> String {
    similar::TextDiff::from_lines(previous_trusted, current_text)
        .unified_diff()
        .context_radius(3)
        .header("a/.tars/permissions.toml", "b/.tars/permissions.toml")
        .to_string()
}

pub(crate) fn is_forbidden_single_word_allow(word: &str) -> bool {
    let base = word
        .split(['/', '\\'])
        .rfind(|segment| !segment.is_empty() && *segment != ".")
        .unwrap_or("");
    !base.is_empty()
        && FORBIDDEN_SINGLE_WORD_ALLOW
            .iter()
            .any(|forbidden| forbidden.eq_ignore_ascii_case(base))
}

fn write_toml_basic_string(out: &mut String, value: &str) {
    use std::fmt::Write;

    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
