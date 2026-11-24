use crate::definition::{BuiltinTool, Temperature};
use crate::events::AgentInteraction;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Origin layer of a discovered [`AgentPreset`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPresetSource {
    /// Compiled-in default preset (`explore`, `general`).
    Builtin,
    /// User-level `<config_dir>/agents/*.md`.
    User,
    /// Explicitly configured directory from `config.toml` (`agents.dirs` or profile `agent_dirs`).
    ConfiguredDir,
    /// Workspace-local `<workspace_root>/.tars/agents/*.md` (requires explicit workspace trust).
    Workspace,
}

/// Optional model and generation configuration declared in an agent preset's frontmatter.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelConfig {
    /// Authorized named profile from `config.toml` (`[profiles.<name>]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Explicit provider model identifier override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Explicit reasoning effort override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Explicit sampling temperature override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<Temperature>,
    /// Explicit per-call output token cap override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Validated agent preset discovered from built-in defaults or Markdown frontmatter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPreset {
    /// Validated preset name (`1..=64` `[a-z0-9-]`).
    pub name: String,
    /// Non-empty description (`1..=1024` scalar values) exposed in `<available_agents>`.
    pub description: String,
    /// Discovery origin layer.
    pub source: AgentPresetSource,
    /// Optional explicit built-in tool allowlist (`None` inherits parent's narrowed capabilities).
    ///
    /// Note: `load_skill` is currently bound only on entry agents (validated against caller
    /// `prelude.skills_enabled` during spawn narrowing), and `recall` follows session
    /// persistence/compaction settings on assembled agents rather than per-preset toggles.
    pub tools: Option<Vec<BuiltinTool>>,
    /// Optional model/generation defaults from frontmatter.
    pub model: Option<AgentModelConfig>,
    /// Optional per-operation model round ceiling.
    pub max_model_rounds: Option<u32>,
    /// Optional per-operation timeout ceiling in seconds (`1..=86400`).
    pub timeout_secs: Option<u64>,
    /// Default interaction lifetime policy (`single_work` or `reusable`).
    pub default_interaction: AgentInteraction,
    /// Optional system role prompt from the Markdown body.
    pub role_prompt: Option<String>,
    /// Canonical path to the source `.md` file (`None` for built-in presets).
    pub source_file: Option<PathBuf>,
    /// SHA-256 hex digest of the source `.md` file bytes (`None` for built-in presets).
    pub sha256: Option<String>,
}

impl AgentPreset {
    /// Returns `true` when this preset is the compiled-in read-only `explore` preset.
    ///
    /// Built-in `explore` intersects with the caller's read-only capabilities and forces
    /// `PermissionMode::ReadOnly`, never failing if the caller only holds a subset of
    /// read-only tools.
    pub fn is_builtin_readonly(&self) -> bool {
        self.source == AgentPresetSource::Builtin && self.name == "explore"
    }
}

/// Deterministic, deduplicated registry of active [`AgentPreset`] definitions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRegistry {
    presets: BTreeMap<String, AgentPreset>,
}

impl AgentRegistry {
    /// Creates an empty agent registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces an agent preset by its validated `name`.
    pub fn insert(&mut self, preset: AgentPreset) {
        self.presets.insert(preset.name.clone(), preset);
    }

    /// Looks up a preset by exact `name`.
    pub fn get(&self, name: &str) -> Option<&AgentPreset> {
        self.presets.get(name)
    }

    /// Returns `true` if a preset with `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.presets.contains_key(name)
    }

    /// Iterates over all presets in deterministic alphabetical order by `name`.
    pub fn iter(&self) -> impl Iterator<Item = &AgentPreset> {
        self.presets.values()
    }

    /// Iterates over preset names in deterministic alphabetical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.presets.keys().map(String::as_str)
    }

    /// Number of registered presets.
    pub fn len(&self) -> usize {
        self.presets.len()
    }

    /// Returns `true` when no presets are registered.
    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }
}

/// Category of non-fatal warning produced while scanning agent preset directories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentDiagnosticKind {
    /// Configured or workspace agents directory could not be opened or is not a directory.
    DirectoryUnavailable,
    /// Reading an agent definition file failed with an I/O error.
    IoError,
    /// Symlink resolved outside the containing agents or workspace directory.
    SymlinkEscape,
    /// Agent definition file or workspace manifest exceeded the byte limit.
    TooLarge,
    /// Agent definition file was not valid UTF-8.
    InvalidUtf8,
    /// YAML frontmatter syntax or unknown/duplicate field error.
    InvalidFrontmatter,
    /// Missing or invalid `name` field.
    InvalidName,
    /// Missing or invalid `description` field.
    InvalidDescription,
    /// Invalid or duplicate entry in `tools`.
    InvalidTools,
    /// Invalid nested `model` configuration.
    InvalidModel,
    /// Invalid `max_model_rounds`, `timeout_secs`, or `default_interaction` value.
    InvalidLimits,
}

/// Non-fatal diagnostic recorded when an individual agent file or directory is skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDiagnostic {
    /// Path associated with the diagnostic.
    pub path: PathBuf,
    /// Categorized failure kind.
    pub kind: AgentDiagnosticKind,
    /// Human-readable reason for skipping the entry.
    pub message: String,
}

/// Result of discovering agent presets across precedence layers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentDiscovery {
    /// Merged and deduplicated active agent presets.
    pub registry: AgentRegistry,
    /// Non-fatal diagnostics for skipped files or unavailable directories.
    pub diagnostics: Vec<AgentDiagnostic>,
}

/// Immutable snapshot of `<workspace_root>/.tars/agents/*.md` captured at session open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceAgentsSnapshot {
    /// Canonicalized workspace root.
    pub workspace_root: PathBuf,
    /// Canonicalized or expected `.tars/agents` directory path.
    pub agents_dir: PathBuf,
    /// Deterministic sorted `<filename>:<sha256>\n` manifest over all `.md` files.
    pub manifest_text: String,
    /// Validated workspace agent presets parsed at snapshot time.
    pub presets: Vec<AgentPreset>,
    /// Non-fatal parse/safety diagnostics encountered during scanning.
    pub diagnostics: Vec<AgentDiagnostic>,
}

/// Workspace-level agent preset trust status observed when opening or creating a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceAgentsReport {
    /// Path to `<canonical_workspace_root>/.tars/agents`, when a workspace is configured.
    pub agents_dir: Option<PathBuf>,
    /// Untrusted workspace preset names currently excluded until the user trusts them.
    pub untrusted_agents: Vec<String>,
    /// Whether workspace agents are currently trusted (also `true` when absent/empty).
    pub trusted: bool,
    /// Whether this session has a Store that can persist workspace agent trust.
    pub can_trust: bool,
    /// Non-fatal diagnostics encountered while scanning `<workspace_root>/.tars/agents`.
    pub diagnostics: Vec<AgentDiagnostic>,
}

impl Default for WorkspaceAgentsReport {
    fn default() -> Self {
        Self {
            agents_dir: None,
            untrusted_agents: Vec::new(),
            trusted: true,
            can_trust: false,
            diagnostics: Vec::new(),
        }
    }
}
