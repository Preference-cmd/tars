use crate::agents::{AgentDiscovery, AgentRegistry, ModelProfile, WorkspaceAgentsReport};
use crate::definition::SessionDefinition;
use crate::prelude::{
    PreludeBudgetConfig, PreludeDiagnostic, PreludeError, PreludeRuntimeState,
    build_initial_prelude_with_registries, discover_skills_for_prelude, has_prelude,
};
use crate::session::{ConversationId, SessionError, SubmitRequest, WorkRef};
use crate::store::StoreError;
use causa::kernel::{BlockContent, BlockId, BlockMeta, Context, ContextBlock, ToolChange};
use std::sync::Arc;

/// Opaque address of one instance on one host; a reopened conversation differs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionTarget {
    pub(super) host: uuid::Uuid,
    pub(super) generation: uuid::Uuid,
    pub(super) conversation_id: ConversationId,
}
impl SessionTarget {
    /// Stable conversation identity, independent of the active instance.
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }
}

/// Fixed creation input and identities, reusable after a cancelled wait.
#[derive(Clone, Debug)]
pub struct PreparedCreate {
    pub(super) conversation_id: ConversationId,
    pub(super) commit_id: String,
    pub(super) definition: SessionDefinition,
    pub(super) context: Context,
    pub(super) persistent: bool,
    pub(super) prelude_diagnostics: Vec<PreludeDiagnostic>,
    pub(super) skill_registry: Option<crate::skills::SkillRegistry>,
    pub(super) media_blobs: Vec<(String, Vec<u8>)>,
}
impl PreparedCreate {
    /// Prepare a durable initial snapshot before any asynchronous operation.
    pub fn persistent(definition: SessionDefinition, context: Context) -> Self {
        Self::new(definition, context, true, Vec::new(), None)
    }
    /// Prepare a temporary conversation with no implicit store writes.
    pub fn transient(definition: SessionDefinition, context: Context) -> Self {
        Self::new(definition, context, false, Vec::new(), None)
    }
    /// Prepare a durable initial snapshot with unified prelude injection when
    /// `imported_context` does not already begin with prelude blocks.
    pub fn prepare_persistent(
        definition: SessionDefinition,
        imported_context: Option<Context>,
    ) -> Result<Self, PreludeError> {
        Self::prepare(definition, imported_context, true)
    }
    /// Prepare a temporary conversation with unified prelude injection when
    /// `imported_context` does not already begin with prelude blocks.
    pub fn prepare_transient(
        definition: SessionDefinition,
        imported_context: Option<Context>,
    ) -> Result<Self, PreludeError> {
        Self::prepare(definition, imported_context, false)
    }
    fn prepare(
        definition: SessionDefinition,
        imported_context: Option<Context>,
        persistent: bool,
    ) -> Result<Self, PreludeError> {
        let base_context = imported_context.unwrap_or_default();
        let state = PreludeRuntimeState::from_definition(
            &definition,
            definition.initial_permission_mode(),
            persistent,
        );
        let (registry, mut diagnostics) = discover_skills_for_prelude(&definition.prelude, &state);
        let (agent_registry, agent_diagnostics) = if definition.prelude.agents_enabled {
            let discovery = AgentDiscovery::discover_non_workspace(
                definition.prelude.config_dir.as_deref(),
                &definition.prelude.agent_dirs,
            );
            let diags: Vec<PreludeDiagnostic> = discovery
                .diagnostics
                .into_iter()
                .map(PreludeDiagnostic::from)
                .collect();
            (discovery.registry, diags)
        } else {
            (AgentRegistry::new(), Vec::new())
        };
        diagnostics.extend(agent_diagnostics);
        if has_prelude(base_context.blocks()) {
            return Ok(Self::new(
                definition,
                base_context,
                persistent,
                Vec::new(),
                Some(registry),
            ));
        }
        let include_load_skill = definition.prelude.skills_enabled && !registry.is_empty();
        let budget = prelude_budget_config(&definition, persistent, include_load_skill);
        let initial = build_initial_prelude_with_registries(
            &definition.prelude,
            &state,
            &budget,
            registry,
            agent_registry,
            &std::collections::BTreeMap::new(),
            diagnostics,
        )?;
        let mut blocks = initial.blocks;
        blocks.extend(base_context.into_blocks());
        let context =
            Context::from_blocks(blocks).expect("fresh prelude blocks have unique block ids");
        Ok(Self::new(
            definition,
            context,
            persistent,
            initial.diagnostics,
            Some(initial.registry),
        ))
    }
    fn new(
        definition: SessionDefinition,
        context: Context,
        persistent: bool,
        prelude_diagnostics: Vec<PreludeDiagnostic>,
        skill_registry: Option<crate::skills::SkillRegistry>,
    ) -> Self {
        Self {
            conversation_id: uuid::Uuid::now_v7().to_string().into(),
            commit_id: uuid::Uuid::now_v7().to_string(),
            definition,
            context,
            persistent,
            prelude_diagnostics,
            skill_registry,
            media_blobs: Vec::new(),
        }
    }
    /// Attach verified sidecar media blobs (e.g. from [`crate::material::load_with_media`])
    /// to seed the session's media vault when created.
    pub fn with_media_blobs(
        mut self,
        blobs: &std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, crate::media::MediaError> {
        let mut verified = Vec::with_capacity(blobs.len());
        for (reference, bytes) in blobs {
            crate::media::verify_managed_blob(reference, bytes)?;
            verified.push((reference.clone(), bytes.clone()));
        }
        self.media_blobs = verified;
        Ok(self)
    }
    /// Read the identity allocated synchronously by preparation.
    pub fn conversation_id(&self) -> &ConversationId {
        &self.conversation_id
    }
    /// Read the prepared initial `Context`.
    pub fn context(&self) -> &Context {
        &self.context
    }
    /// Non-fatal prelude diagnostics collected during preparation.
    pub fn prelude_diagnostics(&self) -> &[PreludeDiagnostic] {
        &self.prelude_diagnostics
    }
}

pub(crate) fn prelude_budget_config(
    definition: &SessionDefinition,
    persistent: bool,
    include_load_skill: bool,
) -> PreludeBudgetConfig {
    let command_settings = definition.effective_command_settings().unwrap_or_default();
    let write_limits = definition.write_limits.unwrap_or_default();
    let mut tools: Vec<causa::kernel::ToolDefinition> = definition
        .tools
        .iter()
        .map(|&kind| {
            crate::tools::workspace_tool_definition(
                kind,
                &definition.tool_limits,
                &write_limits,
                command_settings,
            )
        })
        .collect();
    if persistent {
        tools.push(crate::compact::recall::recall_tool_definition());
    }
    if include_load_skill {
        tools.push(crate::skills::load_skill_tool_definition());
    }
    if definition.prelude.agents_enabled {
        tools.extend(crate::collaboration::collaboration_tool_definitions());
    }
    let folded_tool_declaration_estimate = if tools.is_empty() {
        0
    } else {
        let block = ContextBlock::new(
            BlockId::new(uuid::Uuid::nil()),
            BlockContent::ToolChange(ToolChange::between(&[], &tools)),
            BlockMeta::default(),
        );
        let bytes = serde_json::to_vec(&block)
            .map(|v| u64::try_from(v.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        crate::compact::estimate::estimate_bytes(bytes)
    };
    PreludeBudgetConfig {
        context_window: definition.context_window,
        compact_threshold: definition.compact_threshold,
        folded_tool_declaration_estimate,
    }
}

/// Host resource bounds; these bound counts, not total retained memory.
#[derive(Clone, Debug)]
pub struct HostOptions {
    /// Opening, active, blocked and closing slots together; must be positive.
    pub max_active_sessions: usize,
    /// Maximum concurrently running blocking storage operations; must be positive.
    pub max_storage_operations: usize,
    /// Whether opening a session connects the MCP servers its definition names.
    /// A host that only saves or repairs definitions turns this off so it
    /// starts no server processes; its sessions then offer only built-in tools.
    pub connect_mcp_servers: bool,
    /// Optional path to the user-level permissions rules file (`permissions.toml`).
    /// Defaults to `None` so hosts without explicit configuration stay isolated
    /// from the process user's `~/.tars/permissions.toml`.
    pub user_rules_path: Option<std::path::PathBuf>,
    /// Optional path to the isolated credentials store file (`credentials.toml`).
    /// Defaults to `None` so hosts without explicit configuration stay isolated
    /// from the process user's `~/.tars/credentials.toml`.
    pub credentials_path: Option<std::path::PathBuf>,
    /// Multi-agent coordinator limits and event scheduling configuration.
    pub coordinator: super::CoordinatorConfig,
    /// Authorized named model profiles selectable via `spawn_agent` `model.profile`.
    pub model_profiles: std::collections::BTreeMap<String, ModelProfile>,
}
impl Default for HostOptions {
    fn default() -> Self {
        Self {
            max_active_sessions: 32,
            max_storage_operations: 4,
            connect_mcp_servers: true,
            user_rules_path: None,
            credentials_path: None,
            coordinator: super::CoordinatorConfig::default(),
            model_profiles: std::collections::BTreeMap::new(),
        }
    }
}

/// Instance lifecycle independent of execution and persistence state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Resource resolution or initial commit is in progress.
    Opening,
    /// Initial commit needs confirmation or retry; no handle is published.
    InitializationBlocked,
    /// Instance accepts new work if execution and save gates allow it.
    Open,
    /// Explicit close is in progress or blocked.
    Closing,
    /// Instance was released; retained observations remain readable.
    Closed,
    /// Abnormal owner or worker failure; no new work may start.
    Faulted,
    /// Opening failed before initial commit, and the slot was removed.
    Failed,
}

/// A material generation scoped to exactly one active instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialVersion {
    pub(super) target: SessionTarget,
    pub(super) generation: u64,
}

/// An owned, versioned material copy for controlled edits or explicit export.
#[derive(Clone, Debug)]
pub struct MaterialSnapshot {
    /// Version required by an atomic replacement edit.
    pub version: MaterialVersion,
    /// Current idle materials.
    pub context: Context,
    /// Rewrite record index matching this Context, in creation order.
    pub history: Vec<crate::compact::HistoryRecordEntry>,
}

impl MaterialSnapshot {
    /// Return a copy of the Context with history references detached for export.
    pub fn detached_context(&self) -> Context {
        crate::material::detach_history_references(&self.context, &self.history)
    }
}

/// Independent persistence outcome; never rewrites the low-level work result.
#[derive(Clone, Debug)]
pub enum SaveStatus {
    /// Temporary session with no automatic storage.
    NotTracked,
    /// Actual materials match a confirmed persistent revision.
    Saved {
        /// Store revision confirmed by the current writer.
        revision: u64,
        /// Material generation represented by that revision.
        material_version: MaterialVersion,
    },
    /// Work or edit changed materials and its fixed snapshot is being prepared.
    Dirty {
        /// Last confirmed revision, absent for a new record.
        base_revision: Option<u64>,
        /// Generation to be saved.
        material_version: MaterialVersion,
    },
    /// One immutable commit is in flight.
    Saving {
        /// Fixed identity reused on confirmation and retry.
        commit_id: String,
        /// Last confirmed revision.
        base_revision: Option<u64>,
        /// Generation represented by the pending snapshot.
        material_version: MaterialVersion,
    },
    /// Original pending snapshot is retained for confirmation or retry.
    SaveBlocked {
        /// Structured storage error without string parsing.
        error: Arc<StoreError>,
        /// Identity of the retained immutable commit.
        commit_id: String,
        /// Generation represented by that commit.
        material_version: MaterialVersion,
    },
}

/// Workspace-level command rules status observed when opening or creating a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRulesReport {
    /// Untrusted `allow` rules currently ignored until the user trusts the file.
    pub allow_rules: Vec<crate::permission::CommandRule>,
    /// Path to `<canonical_workspace_root>/.tars/permissions.toml`, when a workspace is configured.
    pub file_path: Option<std::path::PathBuf>,
    /// Previously trusted file text from the Store when the workspace file changed.
    pub previous_trusted_text: Option<String>,
    /// Unified diff between `previous_trusted_text` and the current file text, when changed.
    pub diff: Option<String>,
    /// Whether this session has a Store that can persist workspace trust.
    pub can_trust: bool,
    /// Whether the workspace rules file is currently trusted (also `true` when absent).
    pub trusted: bool,
}

impl Default for WorkspaceRulesReport {
    fn default() -> Self {
        Self {
            allow_rules: Vec::new(),
            file_path: None,
            previous_trusted_text: None,
            diff: None,
            can_trust: false,
            trusted: true,
        }
    }
}

/// Facts learned while an instance was opened, for optional caller reporting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// A saved `full-access` mode was replaced by `ask` because the frozen
    /// definition was not created with `full-access`.
    pub permission_downgraded: bool,
    /// Trust and activation status of workspace command permission rules.
    pub workspace_rules: WorkspaceRulesReport,
    /// Trust and activation status of workspace `.tars/agents/*.md` presets.
    pub workspace_agents: WorkspaceAgentsReport,
}

/// Running compaction settings for one open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionSettings {
    /// Whether automatic compaction is enabled.
    pub auto_compact: bool,
    /// Trigger percentage of the context window.
    pub compact_threshold: u8,
    /// Frozen model context window, or none when unknown.
    pub context_window: Option<u64>,
}

/// Retained-work capacity of one open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkCapacity {
    /// Records currently retained, including terminal and unread ones.
    pub used: usize,
    /// Configured maximum; zero rejects new work.
    pub capacity: usize,
}

/// Current conservative request-size estimate and configured window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextUsage {
    /// Estimated prompt size of the next request, in tokens.
    pub estimate_tokens: u64,
    /// Frozen model context window, or none when unknown.
    pub context_window: Option<u64>,
    /// Whether automatic compaction is enabled.
    pub auto_compact: bool,
    /// Trigger percentage of the context window.
    pub compact_threshold: u8,
}

/// Read-only lookup and managed status; does not grant mutable owner access.
#[derive(Clone, Debug)]
pub struct SessionStatus {
    /// Exact active instance.
    pub target: SessionTarget,
    /// Opening, open, closing or failure state.
    pub lifecycle: Lifecycle,
    /// Last persistence progress or outcome.
    pub save: SaveStatus,
    /// Work awaiting execution completion or its associated save.
    pub active_work: Option<WorkRef>,
    /// Lifecycle failure, if any; work results remain independently observable.
    pub error: Option<ApplicationError>,
}

/// Explicit close policy; in-flight close policies cannot silently change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePolicy {
    /// Confirm the latest actual materials before releasing the writer.
    Save,
    /// Abandon local unsaved materials after running work and I/O have stopped.
    DiscardLocal,
}

/// Structured managed admission, opening, editing and lifecycle failures.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ApplicationError {
    /// Current materials, repair provenance or a prepared plan need attention.
    #[error(transparent)]
    Repair(#[from] super::repair::RepairError),
    /// Original low-level contract failure.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// Original concrete store failure.
    #[error("session store: {0}")]
    Store(#[source] Arc<StoreError>),
    /// Definition rejected before resource resolution.
    #[error(transparent)]
    Definition(#[from] crate::definition::DefinitionError),
    /// Runtime resource resolution failed; no model call was made.
    #[error("session assembly failed: {0}")]
    Assembly(#[source] crate::assembly::AssemblyError),
    /// No root was configured for persistent operations.
    #[error("persistent session store is not configured")]
    StoreUnavailable,
    /// The supplied target belongs to another host or instance.
    #[error("stale session instance")]
    StaleInstance,
    /// The requested instance is no longer in the active directory.
    #[error("session instance is gone")]
    InstanceGone,
    /// A new key or edit attempted to cross a save barrier.
    #[error("session materials are being saved")]
    Saving,
    /// A retained save must be confirmed or retried first.
    #[error("session save is blocked: {0}")]
    SaveBlocked(#[source] Arc<StoreError>),
    /// Explicit close prevents new keys or edits.
    #[error("session is closing")]
    Closing,
    /// Instance was closed.
    #[error("session is closed")]
    Closed,
    /// No new mutations after abnormal owner/worker termination.
    #[error("session is faulted: {0}")]
    Faulted(String),
    /// Another close policy is currently executing.
    #[error("a different close policy is in flight")]
    ClosePolicyConflict,
    /// Prepared creation does not match the original fixed operation.
    #[error("creation parameters conflict with the existing operation")]
    CreationConflict,
    /// Supplied material version is no longer current.
    #[error("stale material version")]
    StaleMaterial,
    /// A configured resource count bound was reached.
    #[error("active session capacity {capacity} reached")]
    Capacity {
        /// Maximum active slots.
        capacity: usize,
    },
    /// Host bounds must be positive and fit the runtime semaphore capacity.
    #[error("host limits must be positive and within runtime semaphore capacity")]
    InvalidOptions,
    /// Work cannot be forgotten before its associated save is handled.
    #[error("work still has an unsettled associated save")]
    SavePending,
    /// This session has no persistent history or is not assembled.
    #[error("context compaction is unavailable for this session")]
    CompactionUnavailable,
    /// Materials are being compacted; new admissions and edits wait.
    #[error("session materials are being compacted")]
    Compacting,
    /// A manual compaction could not be completed.
    #[error("context compaction failed: {0}")]
    Compaction(String),
    /// Requested persistent record does not exist.
    #[error("saved conversation not found")]
    NotFound,
    /// Another writer or process currently holds the session's OS lock.
    #[error("session is locked by another writer: {0}")]
    SessionLocked(String),
    /// The session is currently open or active in this host.
    #[error("session is active in this host; close it first: {0}")]
    SessionActive(String),
    /// A session title failed validation.
    #[error("invalid session title: {0}")]
    InvalidTitle(String),
    /// An open session cannot be forked in its current state.
    #[error("session cannot be forked right now: {0}")]
    ForkNotReady(String),
    /// Referenced child operation does not exist in this session.
    #[error("unknown operation: {0}")]
    UnknownOperation(String),
    /// Multiple agents share the same display name; caller must disambiguate using `agent_id`.
    #[error("multiple agents named '{name}' exist ({}); specify the UUID agent_id instead", candidates.join(", "))]
    AmbiguousAgent {
        /// Ambiguous display name passed as selector.
        name: String,
        /// Candidate UUID `agent_id` values sharing that name.
        candidates: Vec<String>,
    },
    /// Target agent is currently busy executing an operation and cannot yield idle materials.
    #[error("agent {agent_id} is busy with active operation {active_operation_id}")]
    AgentBusy {
        /// Canonical agent identifier.
        agent_id: String,
        /// Active operation identifier currently executing on the agent.
        active_operation_id: String,
    },
    /// Permission state is not available on this session instance.
    #[error("permission state is not available for this session instance")]
    PermissionUnavailable,
    /// User-level or workspace-level command permission rules file is invalid.
    #[error(transparent)]
    PermissionRules(#[from] crate::permission::rules::RulesFileError),
    /// Media ingestion, quota, reference, or integrity validation failed.
    #[error(transparent)]
    Media(#[from] crate::media::MediaError),
    /// Material export or sidecar verification failed.
    #[error("material operation failed: {0}")]
    Material(#[source] Arc<crate::material::MaterialError>),
}
impl From<StoreError> for ApplicationError {
    fn from(error: StoreError) -> Self {
        Self::Store(Arc::new(error))
    }
}
impl From<crate::material::MaterialError> for ApplicationError {
    fn from(error: crate::material::MaterialError) -> Self {
        Self::Material(Arc::new(error))
    }
}

/// Managed refusal preserving the complete original request and block identities.
#[derive(Debug, thiserror::Error)]
#[error("managed work rejected: {reason}")]
pub struct ManagedSubmitRejection {
    /// Structured application or low-level reason.
    #[source]
    pub reason: ApplicationError,
    /// Original input, returned without rebuilding identities.
    pub request: SubmitRequest,
}

/// A final record's summary, with per-record corruption and lock observations.
#[derive(Clone, Debug)]
pub struct SessionListing {
    /// Canonical identity from the final filename.
    pub conversation_id: ConversationId,
    /// Definition and revision, or the precise record validation error.
    pub record: Result<SavedSessionSummary, Arc<StoreError>>,
    /// Whether another writer currently holds this record's OS lock.
    pub active: Result<bool, Arc<StoreError>>,
    /// This host's opening or active state, when present.
    pub local: Option<SessionStatus>,
}
/// A validated saved definition and revision without materials or runtime resources.
#[derive(Clone, Debug)]
pub struct SavedSessionSummary {
    /// Frozen execution definition; no credentials are resolved by listing.
    pub definition: SessionDefinition,
    /// Current validated persistent revision.
    pub revision: u64,
    /// Number of Context blocks in the current record.
    pub block_count: usize,
    /// Session title derived from first user message, if any.
    pub title: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch, if derived from UUIDv7 ID.
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Read-only inspection of one saved session record without resolving credentials or locking.
#[derive(Clone, Debug)]
pub struct SavedSessionInspection {
    /// Canonical conversation identity.
    pub conversation_id: ConversationId,
    /// Frozen execution definition.
    pub definition: SessionDefinition,
    /// Current validated persistent revision.
    pub revision: u64,
    /// Number of Context blocks in the current record.
    pub block_count: usize,
    /// Conservative request-size estimate and running compaction settings.
    pub context_usage: ContextUsage,
    /// Retained-work capacity (`used` is 0 for a saved record on disk).
    pub work_capacity: WorkCapacity,
    /// Number of rewrite records in the saved history index.
    pub history_records: usize,
    /// Cumulative model usage totals stored in the record.
    pub usage: crate::usage::SessionUsage,
    /// Session title derived from the first user message, if any.
    pub title: Option<String>,
    /// Creation timestamp in milliseconds since Unix epoch, if derived from UUIDv7 ID.
    pub created_at_ms: Option<u64>,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
    /// Number of available skills discovered under the frozen skill directories (`0` when disabled).
    pub skills_count: usize,
    /// Whether `dynamic_custom` is a file path that currently exists as a regular file (`None` when not a file).
    pub dynamic_context_file_exists: Option<bool>,
    /// Lock-free summary of all persisted agents belonging to this session.
    pub agents: Vec<SavedAgentSummary>,
}

/// Summary of one persisted agent row within a saved session.
#[derive(Clone, Debug)]
pub struct SavedAgentSummary {
    /// Canonical UUID agent identifier (`entry_agent_id` equals the session ID).
    pub agent_id: String,
    /// Display name of the agent (`"main"` for the root entry agent).
    pub name: String,
    /// Optional preset name from which the agent was created.
    pub preset_name: Option<String>,
    /// Agent interaction policy (`reusable` or `one_shot`).
    pub interaction: crate::events::AgentInteraction,
    /// Whether this agent is the root entry agent of the session.
    pub is_entry: bool,
    /// Monotonic material revision of this agent's stored Context.
    pub material_revision: u64,
    /// Number of Context blocks currently stored for this agent.
    pub block_count: usize,
    /// Number of rewrite records in this agent's stored history index.
    pub history_records: usize,
    /// Number of persisted operations targeting this agent.
    pub operation_count: usize,
    /// Number of pending inbox events targeting this agent.
    pub pending_events: usize,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Read-only summary of one persisted operation targeting an agent.
#[derive(Clone, Debug)]
pub struct SavedOperationSummary {
    /// Canonical UUID operation identifier.
    pub operation_id: String,
    /// Parent operation identifier, if spawned by a child operation.
    pub parent_operation_id: Option<String>,
    /// Parent agent identifier that admitted this operation.
    pub parent_agent_id: Option<String>,
    /// Execution mode (`sync` or `background`).
    pub execution_mode: crate::events::OperationExecutionMode,
    /// Parent-completion lifecycle policy (`cancel` or `continue`).
    pub on_parent_complete: crate::events::OnParentComplete,
    /// Delegation depth relative to the root entry agent.
    pub depth: u32,
    /// Persisted operation status.
    pub status: crate::events::OperationStatus,
    /// Byte length of the archived result text, if any.
    pub result_bytes: Option<usize>,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Completion timestamp in milliseconds since Unix epoch, if terminal.
    pub finished_at_ms: Option<u64>,
}

/// Detailed lock-free read-only inspection of one persisted agent in a saved session.
#[derive(Clone, Debug)]
pub struct SavedAgentInspection {
    /// Canonical conversation/session identity.
    pub conversation_id: ConversationId,
    /// Canonical UUID agent identifier.
    pub agent_id: String,
    /// Display name of the agent.
    pub name: String,
    /// Optional preset name from which the agent was created.
    pub preset_name: Option<String>,
    /// Agent interaction policy (`reusable` or `one_shot`).
    pub interaction: crate::events::AgentInteraction,
    /// Whether this agent is the root entry agent of the session.
    pub is_entry: bool,
    /// Frozen execution definition of this agent.
    pub definition: SessionDefinition,
    /// Monotonic material revision of this agent's stored Context.
    pub material_revision: u64,
    /// Number of Context blocks currently stored for this agent.
    pub block_count: usize,
    /// Conservative request-size estimate and compaction settings for this agent.
    pub context_usage: ContextUsage,
    /// Number of rewrite records in this agent's stored history index.
    pub history_records: usize,
    /// Cumulative model usage totals stored for this agent.
    pub usage: crate::usage::SessionUsage,
    /// Persisted operations targeting this agent, ordered by creation time.
    pub operations: Vec<SavedOperationSummary>,
    /// Number of pending inbox events targeting this agent.
    pub pending_events: usize,
    /// Creation timestamp in milliseconds since Unix epoch.
    pub created_at_ms: u64,
    /// Last update timestamp in milliseconds since Unix epoch.
    pub updated_at_ms: u64,
}

/// Confirmation of a pending commit when explicitly abandoning local materials.
#[derive(Clone, Debug)]
pub enum CommitDisposition {
    /// The exact retained commit is the currently confirmed complete record.
    Applied {
        /// Its confirmed persistent revision.
        revision: u64,
    },
    /// Same writer confirmed its exact old baseline; this pending commit was not applied.
    NotApplied,
    /// The pending snapshot never started any storage attempt.
    NotSubmitted,
    /// Confirmation failed; closing does not claim absence or rollback.
    Unresolved {
        /// Precise confirmation failure retained after releasing the local instance.
        error: ApplicationError,
    },
}

/// Local shutdown outcome distinct from the original work and persistence results.
#[derive(Clone, Debug)]
pub struct CloseOutcome {
    /// Explicit policy used to finish this instance.
    pub policy: ClosePolicy,
    /// Last actual persistence state, updated if close confirmed an existing commit.
    pub save: SaveStatus,
    /// Pending operation confirmation performed during DiscardLocal, when applicable.
    pub pending_commit: Option<CommitDisposition>,
    /// Local materials were abandoned without confirming their snapshot as current.
    pub discarded_unconfirmed: bool,
}

/// Summary of a stored session returned when a short-ID prefix is ambiguous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCandidate {
    /// Full conversation identifier.
    pub conversation_id: ConversationId,
    /// Short display prefix (first 8 characters or full ID if shorter).
    pub short_id: String,
    /// Optional user-assigned or derived session title.
    pub title: Option<String>,
    /// Model identifier saved in the session definition.
    pub model: String,
    /// Last update timestamp in Unix epoch milliseconds.
    pub updated_at_ms: u64,
}

/// Structured failures when resolving a session selector (`id`, prefix, or `latest`).
#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionSelectorError {
    /// Selector was empty or contained only whitespace.
    #[error("session selector must not be empty")]
    EmptySelector,
    /// No stored session matched the selector (or store is empty for `latest`).
    #[error("session not found: {}", escape_single_line(.selector))]
    NotFound {
        /// Original trimmed selector string.
        selector: String,
    },
    /// Prefix matched multiple stored sessions; up to 5 candidates are included.
    #[error("{}", format_ambiguous_selector(.selector, *.total_matches, .candidates))]
    Ambiguous {
        /// Original trimmed selector string.
        selector: String,
        /// Total number of matching sessions before truncating `candidates`.
        total_matches: usize,
        /// Up to 5 top-ranked candidate summaries (newest `updated_at_ms` first).
        candidates: Vec<SessionCandidate>,
    },
    /// Persistent session store is not configured on this host.
    #[error("session store is unavailable")]
    StoreUnavailable,
    /// Underlying store read error while listing sessions.
    #[error("session store: {0}")]
    Store(#[from] Arc<StoreError>),
}

fn escape_single_line(value: &str) -> String {
    crate::policy::escape_untrusted_text(value).replace('\n', "\\n")
}

fn format_ambiguous_selector(
    selector: &str,
    total_matches: usize,
    candidates: &[SessionCandidate],
) -> String {
    use std::fmt::Write;
    let escaped_selector = escape_single_line(selector);
    let mut out =
        format!("session selector '{escaped_selector}' is ambiguous ({total_matches} matches):");
    for candidate in candidates {
        let model = escape_single_line(&candidate.model);
        let short_id = escape_single_line(&candidate.short_id);
        let _ = write!(out, "\n  {short_id} ({model})");
        if let Some(title) = candidate.title.as_deref() {
            let escaped_title = escape_single_line(title);
            let _ = write!(out, " \"{escaped_title}\"");
        }
    }
    if total_matches > candidates.len() {
        let remaining = total_matches - candidates.len();
        let _ = write!(
            out,
            "\n  ... and {remaining} more (total {total_matches} matches)"
        );
    }
    out
}

/// Resolve a session selector (`<full-id>`, `<prefix>`, or `"latest"`) from listed records.
///
/// Resolution priority:
/// 1. Empty or whitespace-only selector -> [`SessionSelectorError::EmptySelector`].
/// 2. Exact `conversation_id` match (including literal `"latest"` or a prefix of another ID).
/// 3. Exact lowercase `"latest"` -> valid session with greatest `updated_at_ms`,
///    tie-broken deterministically by descending `conversation_id`.
/// 4. ASCII case-insensitive prefix match -> unique match returns `Ok`, 0 matches returns
///    [`SessionSelectorError::NotFound`], and >= 2 matches returns
///    [`SessionSelectorError::Ambiguous`] with up to 5 candidates ordered by descending
///    `updated_at_ms`, then ascending `conversation_id`.
pub fn resolve_session_selector_from_listings(
    selector: &str,
    listings: &[SessionListing],
) -> Result<ConversationId, SessionSelectorError> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return Err(SessionSelectorError::EmptySelector);
    }

    if let Some(exact) = listings
        .iter()
        .find(|item| item.conversation_id.as_str() == trimmed)
    {
        return Ok(exact.conversation_id.clone());
    }

    if trimmed == "latest" {
        let latest = listings
            .iter()
            .filter_map(|item| item.record.as_ref().ok().map(|rec| (item, rec)))
            .max_by(|(a_item, a_rec), (b_item, b_rec)| {
                a_rec.updated_at_ms.cmp(&b_rec.updated_at_ms).then_with(|| {
                    a_item
                        .conversation_id
                        .as_str()
                        .cmp(b_item.conversation_id.as_str())
                })
            });
        return latest
            .map(|(item, _)| item.conversation_id.clone())
            .ok_or_else(|| SessionSelectorError::NotFound {
                selector: trimmed.to_owned(),
            });
    }

    let needle = trimmed.to_ascii_lowercase();
    let mut matches: Vec<&SessionListing> = listings
        .iter()
        .filter(|item| {
            item.conversation_id
                .as_str()
                .to_ascii_lowercase()
                .starts_with(&needle)
        })
        .collect();

    match matches.len() {
        0 => Err(SessionSelectorError::NotFound {
            selector: trimmed.to_owned(),
        }),
        1 => Ok(matches[0].conversation_id.clone()),
        total_matches => {
            matches.sort_by(|a, b| {
                let a_key = a
                    .record
                    .as_ref()
                    .map_or((false, 0), |rec| (true, rec.updated_at_ms));
                let b_key = b
                    .record
                    .as_ref()
                    .map_or((false, 0), |rec| (true, rec.updated_at_ms));
                b_key
                    .cmp(&a_key)
                    .then_with(|| a.conversation_id.as_str().cmp(b.conversation_id.as_str()))
            });
            let candidates = matches
                .into_iter()
                .take(5)
                .map(|item| {
                    let short_id = item.conversation_id.as_str().chars().take(8).collect();
                    match &item.record {
                        Ok(rec) => SessionCandidate {
                            conversation_id: item.conversation_id.clone(),
                            short_id,
                            title: rec.title.clone(),
                            model: rec.definition.model.clone(),
                            updated_at_ms: rec.updated_at_ms,
                        },
                        Err(_) => SessionCandidate {
                            conversation_id: item.conversation_id.clone(),
                            short_id,
                            title: None,
                            model: "(corrupt)".to_owned(),
                            updated_at_ms: 0,
                        },
                    }
                })
                .collect();
            Err(SessionSelectorError::Ambiguous {
                selector: trimmed.to_owned(),
                total_matches,
                candidates,
            })
        }
    }
}
