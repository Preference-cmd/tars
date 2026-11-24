//! Public and internal domain types for homogeneous session agents.

use crate::application::{
    OriginalBatchPreview, RepairDiagnosis, RepairMode, RepairSummary, SessionTarget,
};
use crate::compact::HistoryRecordEntry;
use crate::definition::SessionDefinition;
use crate::events::AgentInteraction;
use crate::usage::SessionUsage;
use causa::kernel::Context;

/// Frozen identity and effective execution definition of one agent inside a session.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDescriptor {
    /// Canonical UUIDv7 identifier of this agent (equals `session_id` for the entry agent).
    pub agent_id: String,
    /// Display name of this agent (for example, `"main"` or `"explore"`).
    pub name: String,
    /// Optional preset name from which this agent was spawned.
    pub preset_name: Option<String>,
    /// Interaction lifetime policy (`single_work` or `reusable`).
    pub interaction: AgentInteraction,
    /// Frozen, capability-narrowed execution definition.
    pub definition: SessionDefinition,
    /// Optional frozen system role prompt prepended on creation.
    pub role_prompt: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Point-in-time read-only status snapshot of one agent inside a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// Canonical agent identifier.
    pub agent_id: String,
    /// Display name of the agent.
    pub name: String,
    /// Optional preset name from which the agent was created.
    pub preset_name: Option<String>,
    /// Interaction lifetime policy (`single_work` or `reusable`).
    pub interaction: AgentInteraction,
    /// Current persisted `agents.material_revision`.
    pub material_revision: u64,
    /// Current in-memory material generation for optimistic concurrency edits.
    pub generation: u64,
    /// Active operation identifier if the agent is currently executing, otherwise `None`.
    pub active_operation_id: Option<String>,
    /// Number of unconsumed pending `agent_events` in this agent's causal mailbox.
    pub pending_inbox_count: usize,
    /// Whether this agent can currently be assembled and executed.
    pub executable: bool,
    /// Last assembly failure recorded for this agent (for example, missing credentials).
    pub assembly_error: Option<String>,
    /// Cumulative model usage totals for this agent.
    pub usage: SessionUsage,
}

/// Material version strictly bound to `(SessionInstance, AgentId, generation)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMaterialVersion {
    /// Exact active session instance target.
    pub target: SessionTarget,
    /// Canonical agent identifier within that session.
    pub agent_id: String,
    /// Monotonic in-memory material generation for this agent.
    pub generation: u64,
}

/// Owned, versioned material copy for one specific agent.
#[derive(Clone, Debug)]
pub struct AgentMaterialSnapshot {
    /// Version required for an atomic replacement edit or repair application.
    pub version: AgentMaterialVersion,
    /// Current idle Context of this agent.
    pub context: Context,
    /// Rewrite record index matching this Context, in creation order.
    pub history: Vec<HistoryRecordEntry>,
}

impl AgentMaterialSnapshot {
    /// Return a copy of the Context with history references detached for export.
    pub fn detached_context(&self) -> Context {
        crate::material::detach_history_references(&self.context, &self.history)
    }
}

/// Read-only repair inspection scoped to one specific agent.
#[derive(Clone, Debug)]
pub struct AgentRepairInspection {
    /// Exact `(SessionTarget, agent_id, generation)` inspected.
    pub version: AgentMaterialVersion,
    /// Complete counts and bounded material diagnostics.
    pub diagnosis: RepairDiagnosis,
    /// Original batch observation when an active/retained work reference was supplied.
    pub original: Option<OriginalBatchPreview>,
}

/// Opaque, validated repair candidate bound to `(SessionTarget, agent_id, generation)`.
#[derive(Debug)]
pub struct AgentRepairPlan {
    pub(crate) base: AgentMaterialVersion,
    pub(crate) claims_revision: u64,
    pub(crate) mode: RepairMode,
    pub(crate) candidate: Context,
    pub(crate) summary: RepairSummary,
}

impl AgentRepairPlan {
    /// Construct a validated agent repair plan from its exact baseline and candidate.
    pub(crate) fn new(
        base: AgentMaterialVersion,
        claims_revision: u64,
        mode: RepairMode,
        candidate: Context,
        summary: RepairSummary,
    ) -> Self {
        Self {
            base,
            claims_revision,
            mode,
            candidate,
            summary,
        }
    }

    /// Exact baseline required when applying this repair plan.
    pub fn base(&self) -> &AgentMaterialVersion {
        &self.base
    }

    /// Repair choice used to produce this candidate.
    pub fn mode(&self) -> &RepairMode {
        &self.mode
    }

    /// Bounded display summary of the proposed material changes.
    pub fn summary(&self) -> &RepairSummary {
        &self.summary
    }

    /// Complete immutable candidate Context, including any adopted original results.
    pub fn candidate(&self) -> &Context {
        &self.candidate
    }
}
