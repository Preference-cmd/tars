use std::collections::BTreeMap;
use std::path::PathBuf;

/// Relative resource paths found under a skill directory.
///
/// Only paths are recorded; resource file contents are never pre-read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillResources {
    /// Relative paths under `scripts/`, sorted lexicographically.
    pub scripts: Vec<String>,
    /// Relative paths under `references/`, sorted lexicographically.
    pub references: Vec<String>,
    /// Relative paths under `assets/`, sorted lexicographically.
    pub assets: Vec<String>,
}

impl SkillResources {
    /// Returns `true` when no resource files exist under any resource directory.
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty() && self.references.is_empty() && self.assets.is_empty()
    }
}

/// Metadata and canonical location for a validated skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillEntry {
    /// Canonical skill name matching the parent directory name.
    pub name: String,
    /// Concise description of what the skill does and when to use it.
    pub description: String,
    /// Canonical path to the skill directory.
    pub dir: PathBuf,
    /// Canonical path to the skill's `SKILL.md` file.
    pub skill_file: PathBuf,
    /// Lowercase hex SHA-256 digest of the `SKILL.md` bytes at discovery time.
    pub sha256: String,
    /// Relative paths of bundled `scripts/`, `references/`, and `assets/`.
    pub resources: SkillResources,
}

/// Category of non-fatal warning recorded during skill discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillDiagnosticKind {
    /// A configured skill directory could not be opened or read.
    DirectoryUnavailable,
    /// A `SKILL.md` file had missing or malformed YAML frontmatter.
    InvalidFrontmatter,
    /// The `name` field violated `agentskills.io` naming constraints.
    InvalidName,
    /// The `name` field did not match the skill's parent directory name.
    NameMismatch,
    /// The `description` field was missing, blank, or over 1024 characters.
    InvalidDescription,
    /// A `SKILL.md` or resource entry resolved outside its skill directory.
    SymlinkEscape,
    /// Reading a skill directory or `SKILL.md` failed due to I/O.
    IoError,
    /// A `SKILL.md` file did not contain valid UTF-8.
    InvalidUtf8,
    /// A `SKILL.md` file exceeded the maximum allowed size.
    TooLarge,
}

/// Structured warning emitted when a single skill or directory is skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    /// Path to the skill directory or `SKILL.md` file that caused the warning.
    pub path: PathBuf,
    /// Categorized reason the entry was skipped.
    pub kind: SkillDiagnosticKind,
    /// Human-readable explanation of the failure.
    pub message: String,
}

/// Deduplicated catalog of valid skills ordered lexicographically by `name`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillRegistry {
    entries: BTreeMap<String, SkillEntry>,
}

impl SkillRegistry {
    /// Creates an empty skill registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when no skills are registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of registered skills.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Looks up a skill by exact name.
    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.entries.get(name)
    }

    /// Inserts or overrides a skill entry by its `name`.
    pub fn insert(&mut self, entry: SkillEntry) {
        self.entries.insert(entry.name.clone(), entry);
    }

    /// Iterates over all registered skills in lexicographical name order.
    pub fn iter(&self) -> impl Iterator<Item = &SkillEntry> {
        self.entries.values()
    }

    /// Iterates over all registered skill names in lexicographical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Result of scanning configured skill directories and optional workspace skills.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillDiscovery {
    /// Deduplicated, sorted registry of valid skills.
    pub registry: SkillRegistry,
    /// Non-fatal diagnostics for skipped skills or unreadable directories.
    pub diagnostics: Vec<SkillDiagnostic>,
}
