//! Homogeneous in-memory execution unit (`AgentUnit`) for any agent in a session.

use super::types::{AgentDescriptor, AgentSnapshot};
use crate::compact::HistoryRecordEntry;
use crate::compact::history::HistoryStore;
use crate::events::{DEFAULT_NOTIFICATION_INLINE_MAX_BYTES, InboxDrainHandle, InboxEvent};
use crate::permission::{ApprovalOrigin, PermissionHub};
use crate::session::{Session, SessionHandle, WorkRef};
use crate::usage::SessionUsage;
use causa::kernel::Context;
use std::path::Path;
use std::sync::Arc;

/// Homogeneous runtime state for any agent (entry or child) managed by `SessionCoordinator`.
pub(crate) struct AgentUnit {
    pub(crate) descriptor: AgentDescriptor,
    pub(crate) depth: u32,
    pub(crate) material_revision: u64,
    pub(crate) generation: u64,
    /// Idle authoritative Context snapshot; always readable even when `owner` is `None`.
    pub(crate) context: Context,
    pub(crate) history: Vec<HistoryRecordEntry>,
    pub(crate) usage: SessionUsage,
    pub(crate) owner: Option<Session>,
    pub(crate) handle: Option<SessionHandle>,
    pub(crate) permissions: Option<Arc<PermissionHub>>,
    pub(crate) assembly_error: Option<String>,
    pub(crate) inbox: Arc<InboxDrainHandle>,
    pub(crate) active_operation_id: Option<String>,
    pub(crate) active_work: Option<WorkRef>,
    pub(crate) saving: bool,
    pub(crate) compacting: bool,
    pub(crate) auto_wakeups_in_cycle: usize,
    pub(crate) confirmed_work_count: usize,
    /// Authoritative mirror of pending `agent_events` targeting this agent.
    pub(crate) pending_events: Vec<InboxEvent>,
    pub(crate) history_store: Option<Arc<HistoryStore>>,
}

impl AgentUnit {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: &str,
        descriptor: AgentDescriptor,
        depth: u32,
        material_revision: u64,
        context: Context,
        history: Vec<HistoryRecordEntry>,
        usage: SessionUsage,
        pending_events: Vec<InboxEvent>,
        confirmed_work_count: usize,
        store_root: Option<&Path>,
    ) -> Self {
        let inbox = Arc::new(InboxDrainHandle::with_initial(
            pending_events.clone(),
            DEFAULT_NOTIFICATION_INLINE_MAX_BYTES,
        ));
        let history_store = store_root.map(|root| {
            Arc::new(HistoryStore::new_strict_for_agent(
                root,
                session_id,
                &descriptor.agent_id,
                history.clone(),
            ))
        });
        Self {
            descriptor,
            depth,
            material_revision,
            generation: 0,
            context,
            history,
            usage,
            owner: None,
            handle: None,
            permissions: None,
            assembly_error: None,
            inbox,
            active_operation_id: None,
            active_work: None,
            saving: false,
            compacting: false,
            auto_wakeups_in_cycle: 0,
            confirmed_work_count,
            pending_events,
            history_store,
        }
    }

    /// Whether this agent is currently idle and eligible for reactive wakeup evaluation.
    pub(crate) fn is_idle_for_wakeup(&self) -> bool {
        self.active_operation_id.is_none()
            && self.active_work.is_none()
            && !self.saving
            && !self.compacting
    }

    /// Produce a public read-only [`AgentSnapshot`] of this agent's current state.
    pub(crate) fn snapshot(&self) -> AgentSnapshot {
        AgentSnapshot {
            agent_id: self.descriptor.agent_id.clone(),
            name: self.descriptor.name.clone(),
            preset_name: self.descriptor.preset_name.clone(),
            interaction: self.descriptor.interaction,
            material_revision: self.material_revision,
            generation: self.generation,
            active_operation_id: self.active_operation_id.clone(),
            pending_inbox_count: self.pending_events.len(),
            executable: self.assembly_error.is_none(),
            assembly_error: self.assembly_error.clone(),
            usage: self.usage,
        }
    }

    /// Check whether this agent's idle materials require repair before admitting new work.
    pub(crate) fn needs_repair(&self) -> bool {
        let owner_history = self
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .and_then(|runtime| runtime.history());
        let artifacts = owner_history.as_deref().or(self.history_store.as_deref());
        crate::application::repair::admission_check(&self.context, &self.history, artifacts)
            .is_err()
    }

    /// Update the live approval origin on the assembled [`Session`] when starting an operation.
    pub(crate) fn set_approval_origin(&self, operation_id: &str) {
        if let Some(owner) = &self.owner {
            owner.set_approval_origin(ApprovalOrigin::new(
                &self.descriptor.agent_id,
                &self.descriptor.name,
                operation_id,
            ));
        }
    }
}
