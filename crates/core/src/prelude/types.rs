use super::contract::DynamicSection;
use crate::agents::{AgentDiagnostic, AgentDiagnosticKind, AgentRegistry};
use crate::definition::{DefinitionError, SessionDefinition, WorkspaceTool};
use crate::policy::PermissionMode;
use crate::skills::{SkillDiagnostic, SkillDiagnosticKind, SkillRegistry};
use causa::kernel::ContextBlock;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

fn default_true() -> bool {
    true
}

/// Static custom prompt appended to the `Role::System` prelude region after `base`.
///
/// Frozen at session creation and never re-read or mutated across the session lifetime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StaticCustomSlot(pub String);

impl StaticCustomSlot {
    /// Creates a static custom slot from text.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Returns the inner text slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns `true` when the text is empty or whitespace-only.
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl From<String> for StaticCustomSlot {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StaticCustomSlot {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Dynamic custom context slot injected at the end of the `Role::User` dynamic region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "kebab-case")]
pub enum DynamicCustomSlot {
    /// Fixed inline text frozen at session creation.
    Inline(String),
    /// Canonicalized file path watched on each turn and reopen.
    File(PathBuf),
}

/// Frozen prelude and skill-discovery settings persisted inside [`SessionDefinition`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreludeDefinition {
    /// Optional static custom text appended after `tars:prelude:base`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_custom: Option<StaticCustomSlot>,
    /// Optional dynamic custom context (inline text or frozen file path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic_custom: Option<DynamicCustomSlot>,
    /// Frozen configuration directory used to discover `<config_dir>/AGENTS.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// Ordered, canonicalized skill directories (later entries override earlier ones).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_dirs: Vec<PathBuf>,
    /// Whether `<workspace_root>/.tars/skills` is included with highest precedence.
    #[serde(default = "default_true")]
    pub include_workspace_skills: bool,
    /// Whether skills discovery, `<available_skills>`, and `load_skill` are enabled.
    #[serde(default = "default_true")]
    pub skills_enabled: bool,
    /// Ordered, canonicalized agent preset directories (later entries override earlier ones).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_dirs: Vec<PathBuf>,
    /// Whether trusted `<workspace_root>/.tars/agents` is included with highest precedence.
    #[serde(default = "default_true")]
    pub include_workspace_agents: bool,
    /// Whether agent discovery, `<available_agents>`, and the 4 collaboration tools are enabled.
    #[serde(default = "default_true")]
    pub agents_enabled: bool,
}

impl Default for PreludeDefinition {
    fn default() -> Self {
        Self {
            static_custom: None,
            dynamic_custom: None,
            config_dir: None,
            skill_dirs: Vec::new(),
            include_workspace_skills: true,
            skills_enabled: true,
            agent_dirs: Vec::new(),
            include_workspace_agents: true,
            agents_enabled: true,
        }
    }
}

impl PreludeDefinition {
    /// Validates that every persisted path is non-empty and absolute.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        if let Some(DynamicCustomSlot::File(path)) = &self.dynamic_custom
            && (path.as_os_str().is_empty() || !path.is_absolute())
        {
            return Err(DefinitionError::InvalidPreludeConfig);
        }
        if let Some(dir) = &self.config_dir
            && (dir.as_os_str().is_empty() || !dir.is_absolute())
        {
            return Err(DefinitionError::InvalidPreludeConfig);
        }
        for dir in &self.skill_dirs {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                return Err(DefinitionError::InvalidPreludeConfig);
            }
        }
        for dir in &self.agent_dirs {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                return Err(DefinitionError::InvalidPreludeConfig);
            }
        }
        Ok(())
    }
}

/// Active built-in capability categories advertised inside `tars:prelude:env`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityCategory {
    /// Workspace read/list/search/find tools (`list_directory`, `read_file`, `search_text`, `find_files`).
    WorkspaceRead,
    /// Workspace write/edit tools (`write_file`, `edit_file`).
    WorkspaceWrite,
    /// Sandboxed command execution (`run_command`).
    Command,
    /// History and artifact inspection (`recall`).
    Recall,
    /// On-demand skill loading (`load_skill`).
    Skills,
    /// Configured MCP server tools.
    Mcp,
}

impl CapabilityCategory {
    /// Stable lowercase identifier rendered in `<env>`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceRead => "workspace_read",
            Self::WorkspaceWrite => "workspace_write",
            Self::Command => "command",
            Self::Recall => "recall",
            Self::Skills => "skills",
            Self::Mcp => "mcp",
        }
    }
}

/// Mutable runtime facts evaluated when building or refreshing the dynamic prelude.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreludeRuntimeState {
    /// Canonicalized workspace root, if configured.
    pub workspace_root: Option<PathBuf>,
    /// Active permission mode for the current turn.
    pub permission_mode: PermissionMode,
    /// Active built-in capability categories (sorted deterministically).
    pub capabilities: BTreeSet<CapabilityCategory>,
    /// Host OS platform name (defaults to `std::env::consts::OS`).
    pub platform: Cow<'static, str>,
}

impl PreludeRuntimeState {
    /// Constructs runtime state from a [`SessionDefinition`], current mode, and persistence flag.
    pub fn from_definition(
        definition: &SessionDefinition,
        permission_mode: PermissionMode,
        persistent: bool,
    ) -> Self {
        let mut capabilities = BTreeSet::new();
        for tool in &definition.tools {
            match tool {
                WorkspaceTool::ListDirectory
                | WorkspaceTool::ReadFile
                | WorkspaceTool::SearchText
                | WorkspaceTool::FindFiles => {
                    capabilities.insert(CapabilityCategory::WorkspaceRead);
                }
                WorkspaceTool::WriteFile | WorkspaceTool::EditFile => {
                    capabilities.insert(CapabilityCategory::WorkspaceWrite);
                }
                WorkspaceTool::RunCommand => {
                    capabilities.insert(CapabilityCategory::Command);
                }
            }
        }
        if persistent {
            capabilities.insert(CapabilityCategory::Recall);
        }
        if !definition.mcp_servers.is_empty() {
            capabilities.insert(CapabilityCategory::Mcp);
        }
        Self {
            workspace_root: definition.workspace_root.clone(),
            permission_mode,
            capabilities,
            platform: Cow::Borrowed(std::env::consts::OS),
        }
    }
}

/// Budget inputs controlling default cap and small-window tightening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreludeBudgetConfig {
    /// Model context window in tokens, or `None` when unknown.
    pub context_window: Option<u64>,
    /// Compaction trigger percentage (`10..=95`, default `70`).
    pub compact_threshold: u8,
    /// Estimated tokens occupied by the folded tool declarations.
    pub folded_tool_declaration_estimate: u64,
}

impl Default for PreludeBudgetConfig {
    fn default() -> Self {
        Self {
            context_window: None,
            compact_threshold: 70,
            folded_tool_declaration_estimate: 0,
        }
    }
}

/// Category of non-fatal warning produced while refreshing dynamic prelude state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreludeDiagnosticKind {
    /// Reading a dynamic file encountered a temporary I/O error.
    IoError,
    /// A dynamic file contained invalid UTF-8 bytes.
    InvalidUtf8,
    /// A dynamic file exceeded the maximum read size.
    TooLarge,
    /// A configured dynamic file path did not point to a regular file.
    NotAFile,
    /// A non-fatal skill discovery warning.
    Skill(SkillDiagnosticKind),
    /// A non-fatal agent discovery warning.
    Agent(AgentDiagnosticKind),
}

/// Structured warning emitted during prelude assembly or runtime refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreludeDiagnostic {
    /// Affected dynamic section, if applicable.
    pub section: Option<DynamicSection>,
    /// Filesystem path associated with the warning.
    pub path: PathBuf,
    /// Categorized diagnostic kind.
    pub kind: PreludeDiagnosticKind,
    /// Human-readable description of the warning.
    pub message: String,
}

impl From<SkillDiagnostic> for PreludeDiagnostic {
    fn from(diag: SkillDiagnostic) -> Self {
        Self {
            section: Some(DynamicSection::Skills),
            path: diag.path,
            kind: PreludeDiagnosticKind::Skill(diag.kind),
            message: diag.message,
        }
    }
}

impl From<AgentDiagnostic> for PreludeDiagnostic {
    fn from(diag: AgentDiagnostic) -> Self {
        Self {
            section: Some(DynamicSection::Agents),
            path: diag.path,
            kind: PreludeDiagnosticKind::Agent(diag.kind),
            message: diag.message,
        }
    }
}

/// Fatal error raised in strict creation mode when an explicit or present prelude file cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum PreludeError {
    /// I/O error reading a prelude file.
    #[error("failed to read prelude file {path:?}: {kind:?}")]
    Io {
        /// File path that failed.
        path: PathBuf,
        /// I/O error category.
        kind: io::ErrorKind,
    },
    /// File is not valid UTF-8.
    #[error("prelude file {path:?} is not valid UTF-8")]
    InvalidUtf8 {
        /// File path that failed UTF-8 decoding.
        path: PathBuf,
    },
    /// File exceeds the bounded prelude read limit.
    #[error("prelude file {path:?} exceeds {limit} bytes")]
    TooLarge {
        /// File path that exceeded the limit.
        path: PathBuf,
        /// Maximum allowed bytes.
        limit: usize,
    },
    /// Path exists (or was explicitly requested) but is not a regular file.
    #[error("prelude file {path:?} is not a regular file")]
    NotAFile {
        /// Path that was not a regular file.
        path: PathBuf,
    },
}

/// Output of [`super::build_initial_prelude`].
#[derive(Clone, Debug)]
pub struct InitialPrelude {
    /// Ordered static (`Role::System`) followed by dynamic (`Role::User`) blocks.
    pub blocks: Vec<ContextBlock>,
    /// Validated skill registry discovered during initial assembly.
    pub registry: SkillRegistry,
    /// Validated agent registry discovered during initial assembly.
    pub agent_registry: AgentRegistry,
    /// Non-fatal diagnostics (e.g. skipped malformed `SKILL.md` or agent `.md` files).
    pub diagnostics: Vec<PreludeDiagnostic>,
    /// Effective byte budget applied to the prelude.
    pub effective_budget_bytes: usize,
}

/// Output of [`super::evaluate_dynamic_updates`].
#[derive(Clone, Debug)]
pub struct DynamicUpdate {
    /// Incremental `tars:prelude:update:*` `Role::User` blocks to append before the next user message.
    pub blocks: Vec<ContextBlock>,
    /// Freshly discovered skill registry for the current turn.
    pub registry: SkillRegistry,
    /// Freshly discovered agent registry for the current turn.
    pub agent_registry: AgentRegistry,
    /// Non-fatal warnings recorded during tolerant refresh.
    pub diagnostics: Vec<PreludeDiagnostic>,
    /// Whether the `<available_skills>` prelude section changed on this turn.
    pub skills_changed: bool,
    /// Whether the `<available_agents>` prelude section changed on this turn.
    pub agents_changed: bool,
}
