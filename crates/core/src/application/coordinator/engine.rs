//! Core multi-agent `SessionCoordinator` and reactive event-driven scheduling loop.

use super::types::*;
use crate::agent::{
    AgentDescriptor, AgentMaterialSnapshot, AgentMaterialVersion, AgentRepairInspection,
    AgentRepairPlan, AgentSnapshot, AgentUnit,
};
use crate::application::RepairMode;
use crate::application::entry::Entry;
use crate::application::host::HostInner;
use crate::application::repair;
use crate::application::saving;
use crate::application::types::*;
use crate::assembly::{SessionResources, assemble};
use crate::compact::HistoryRecordEntry;
use crate::definition::{
    ChildNarrowingRequest, SessionDefinition, check_permission_mode_narrowing,
    narrow_child_definition, permission_mode_rank,
};
use crate::events::{
    AgentInteraction, AgentNotificationPayload, BackgroundTaskKind, BackgroundTaskRegistration,
    BackgroundTaskRegistry, BackgroundTaskSnapshot, ConsumedInboxEvents,
    DEFAULT_NOTIFICATION_INLINE_MAX_BYTES, DrainedNotification, EventDeliveryState, EventPriority,
    EventSourceKind, InboxEvent, MAX_OPERATION_INPUT_BYTES, MAX_PROMPT_BYTES, NewInboxEvent,
    OnParentComplete, OperationExecutionMode, OperationStatus, SessionBus, SessionEvent,
    TASK_NOTIFICATION_SOURCE, TaskLifecyclePolicy, WakePolicy, slice_utf8_page,
    truncate_utf8_boundary,
};
use crate::permission::{ApprovalOrigin, ApprovalsHub, CommandRuleSet, WorkspaceRulesBackend};
use crate::policy::PermissionMode;
use crate::session::{ConversationId, SessionError, SubmitRequest, WorkRef, WorkResult, WorkState};
use crate::store::{
    AdmitOperationOutcome, AdmitOperationRequest, AgentMaterialUpdate, NewAgentRow,
    OperationTerminalUpdate, OutboxCommitRequest, StoreError, StoredAgentRow, StoredOperationRow,
};
use crate::usage::SessionUsage;
use causa::kernel::{
    BlockContent, BlockMeta, CancellationToken, Context, ContextBlock, ControlError, Role, TurnId,
};
use causa::runtime::{TurnInterruption, TurnResult, new_block_id};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;

const ROLE_PROMPT_SOURCE: &str = "tars:agent-role";

#[derive(Clone)]
pub(super) struct OperationRecord {
    pub(super) operation_id: String,
    pub(super) target_agent_id: String,
    pub(super) parent_operation_id: Option<String>,
    pub(super) parent_agent_id: Option<String>,
    pub(super) root_turn_id: Option<TurnId>,
    pub(super) dedup_fingerprint: Option<String>,
    pub(super) execution_mode: OperationExecutionMode,
    pub(super) on_parent_complete: OnParentComplete,
    pub(super) depth: u32,
    pub(super) status: OperationStatus,
    pub(super) result_text: Option<String>,
    pub(super) error: Option<String>,
    pub(super) finished_at_ms: Option<u64>,
    pub(super) cancel_token: CancellationToken,
    pub(super) suppress_wake: bool,
    pub(super) completion_notify: Arc<Notify>,
}

impl OperationRecord {
    fn to_receipt(&self, replayed: bool) -> OperationReceipt {
        OperationReceipt {
            agent_id: self.target_agent_id.clone(),
            operation_id: self.operation_id.clone(),
            status: self.status,
            replayed,
        }
    }

    fn to_outcome(&self) -> OperationOutcome {
        let result_bytes = self.result_text.as_deref().map_or(0, str::len);
        OperationOutcome {
            agent_id: self.target_agent_id.clone(),
            operation_id: self.operation_id.clone(),
            status: self.status,
            result_text: self.result_text.clone(),
            result_bytes,
            error: self.error.clone(),
        }
    }
}

pub(super) struct QueuedOutbox {
    pub(super) req: OutboxCommitRequest,
    /// `true` when queued after `StoreError::PendingCommit` (never touched
    /// `WriterState::pending_outbox`), so `expected_session_revision` can safely
    /// refresh on retry once the blocking commit clears.
    pub(super) deferred_before_write: bool,
}

pub(super) struct PendingReactiveAdmit {
    pub(super) admit_req: AdmitOperationRequest,
    pub(super) prompt: String,
    pub(super) timeout_secs: u64,
    pub(super) drained: DrainedNotification,
}

pub(super) struct CoordinatorState {
    pub(super) config: CoordinatorConfig,
    pub(super) agents: HashMap<String, AgentUnit>,
    pub(super) agent_order: Vec<String>,
    pub(super) operations: HashMap<String, OperationRecord>,
    pub(super) dedup_index: HashMap<(Option<String>, String), String>,
    pub(super) active_root_turn: Option<TurnId>,
    pub(super) auto_wakeup_turns: HashSet<TurnId>,
    pub(super) command_rules: CommandRuleSet,
    pub(super) workspace_rules_backend: Option<WorkspaceRulesBackend>,
    pub(super) save_blocked: Option<Arc<StoreError>>,
    pub(super) pending_admits: VecDeque<PendingReactiveAdmit>,
    pub(super) pending_outboxes: VecDeque<QueuedOutbox>,
    pub(super) active_tasks: HashMap<String, JoinHandle<()>>,
    pub(super) utility_tasks: Vec<JoinHandle<()>>,
    pub(super) scheduler_task: Option<JoinHandle<()>>,
    pub(super) child_saves_in_flight: usize,
    pub(super) admissions_in_flight: usize,
}

struct AdmissionInFlightGuard(Arc<SessionCoordinator>);

impl Drop for AdmissionInFlightGuard {
    fn drop(&mut self) {
        {
            let mut state = self.0.state.lock().expect("coord lock");
            state.admissions_in_flight = state.admissions_in_flight.saturating_sub(1);
        }
        self.0.state_changed.notify_waiters();
    }
}

struct TargetAgentReservation {
    coord: Arc<SessionCoordinator>,
    target_agent_id: String,
    operation_id: String,
    committed: bool,
}

impl Drop for TargetAgentReservation {
    fn drop(&mut self) {
        if !self.committed {
            {
                let mut state = self.coord.state.lock().expect("coord lock");
                if let Some(unit) = state.agents.get_mut(&self.target_agent_id)
                    && unit.active_operation_id.as_deref() == Some(&self.operation_id)
                {
                    unit.active_operation_id = None;
                }
                state.active_tasks.remove(&self.operation_id);
            }
            self.coord.wakeup_notify.notify_one();
            self.coord.state_changed.notify_waiters();
        }
    }
}

enum AdmittedOutcome {
    Immediate(SpawnAgentOutcome),
    WaitSync(String),
}

/// Event-driven multi-agent coordinator owned per active session instance.
pub(crate) struct SessionCoordinator {
    pub(super) session_id: String,
    pub(super) entry_agent_id: String,
    pub(super) persistent: bool,
    pub(super) host: Arc<HostInner>,
    pub(super) entry: Weak<Entry>,
    pub(super) bus: Arc<SessionBus>,
    pub(super) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(super) approvals: Arc<ApprovalsHub>,
    pub(super) writer_gate: Arc<tokio::sync::Mutex<()>>,
    pub(super) wakeup_notify: Arc<Notify>,
    pub(crate) state_changed: Arc<Notify>,
    pub(super) shutdown: AtomicBool,
    pub(super) state: Mutex<CoordinatorState>,
}

#[derive(Serialize)]
struct OperationOutcomeJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct CanonicalSpawnFingerprint<'a> {
    kind: &'static str,
    caller_agent_id: &'a str,
    name: &'a str,
    preset_name: Option<&'a str>,
    role_prompt: Option<&'a str>,
    prompt: &'a str,
    narrowing: CanonicalNarrowing<'a>,
    interaction: AgentInteraction,
    execution_mode: OperationExecutionMode,
    on_parent_complete: OnParentComplete,
    timeout_secs: Option<u64>,
    max_rounds: Option<u32>,
    coalesce_key: Option<&'a str>,
}

#[derive(Serialize)]
struct CanonicalNarrowing<'a> {
    model_override: Option<CanonicalModelOverride<'a>>,
    workspace_root: Option<Option<String>>,
    tools: Option<&'a [crate::definition::WorkspaceTool]>,
    tool_limits: Option<&'a crate::definition::WorkspaceToolLimits>,
    write_limits: Option<Option<crate::definition::WriteToolLimits>>,
    command: Option<Option<crate::definition::CommandSettings>>,
    code_execution: Option<Option<crate::definition::CodeExecutionSettings>>,
    mcp_server_ids: Option<&'a [String]>,
    max_model_rounds: Option<u32>,
    max_tool_calls: Option<u32>,
    work_timeout_secs: Option<u64>,
    permission_mode: Option<PermissionMode>,
}

#[derive(Serialize)]
struct CanonicalModelOverride<'a> {
    provider: Option<crate::definition::Provider>,
    model: Option<&'a str>,
    endpoint: Option<Option<&'a str>>,
    credential: Option<&'a crate::definition::CredentialRef>,
    max_tokens: Option<Option<u32>>,
    reasoning_effort: Option<Option<&'a str>>,
    temperature: Option<Option<crate::definition::Temperature>>,
    extra_body: Option<&'a serde_json::Map<String, serde_json::Value>>,
    context_window: Option<Option<u64>>,
    max_model_attempts: Option<Option<u32>>,
    render_options: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

#[derive(Serialize)]
struct CanonicalMessageFingerprint<'a> {
    kind: &'static str,
    caller_agent_id: &'a str,
    target_agent_id: &'a str,
    prompt: &'a str,
    execution_mode: OperationExecutionMode,
    on_parent_complete: OnParentComplete,
    timeout_secs: Option<u64>,
    max_rounds: Option<u32>,
    coalesce_key: Option<&'a str>,
}

impl SessionCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::application) fn new(
        session_id: String,
        config: CoordinatorConfig,
        persistent: bool,
        host: Arc<HostInner>,
        entry: Weak<Entry>,
        approvals: Arc<ApprovalsHub>,
        writer_gate: Arc<tokio::sync::Mutex<()>>,
        entry_unit: AgentUnit,
        restored_agents: Vec<StoredAgentRow>,
        restored_operations: Vec<StoredOperationRow>,
        restored_pending_events: HashMap<String, Vec<InboxEvent>>,
        command_rules: CommandRuleSet,
        workspace_rules_backend: Option<WorkspaceRulesBackend>,
    ) -> Arc<Self> {
        let store_root = persistent
            .then(|| host.store.as_ref().map(|s| s.root().to_path_buf()))
            .flatten();
        let mut agents = HashMap::new();
        let mut agent_order = Vec::new();
        let entry_agent_id = entry_unit.descriptor.agent_id.clone();
        agent_order.push(entry_agent_id.clone());
        agents.insert(entry_agent_id.clone(), entry_unit);

        let mut op_count_by_agent: HashMap<String, usize> = HashMap::new();
        let mut max_depth_by_agent: HashMap<String, u32> = HashMap::new();
        let mut operations = HashMap::new();
        let mut dedup_index = HashMap::new();

        for row in restored_operations {
            *op_count_by_agent
                .entry(row.target_agent_id.clone())
                .or_insert(0) += 1;
            let max_depth = max_depth_by_agent
                .entry(row.target_agent_id.clone())
                .or_insert(1);
            *max_depth = (*max_depth).max(row.depth);

            if let Some(source_block_id) = &row.source_block_id {
                dedup_index.insert(
                    (row.parent_operation_id.clone(), source_block_id.clone()),
                    row.operation_id.clone(),
                );
            }
            let error = parse_error_from_outcome_json(row.outcome_json.as_deref());
            let record = OperationRecord {
                operation_id: row.operation_id.clone(),
                target_agent_id: row.target_agent_id,
                parent_operation_id: row.parent_operation_id,
                parent_agent_id: row.parent_agent_id,
                root_turn_id: None,
                dedup_fingerprint: row.dedup_fingerprint,
                execution_mode: row.execution_mode,
                on_parent_complete: row.on_parent_complete,
                depth: row.depth,
                status: row.status,
                result_text: row.result_text,
                error,
                finished_at_ms: row.finished_at_ms,
                cancel_token: CancellationToken::new(),
                suppress_wake: false,
                completion_notify: Arc::new(Notify::new()),
            };
            operations.insert(row.operation_id, record);
        }

        for row in restored_agents {
            if row.agent_id == entry_agent_id {
                continue;
            }
            let definition = match serde_json::from_str::<SessionDefinition>(&row.definition_json) {
                Ok(def) => def,
                Err(_) => continue,
            };
            let context =
                crate::material::decode_stored(row.context_json.as_bytes()).unwrap_or_default();
            let history: Vec<HistoryRecordEntry> =
                serde_json::from_str(&row.history_index_json).unwrap_or_default();
            let usage = row
                .usage_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<crate::store::WireSessionUsage>(raw).ok())
                .map(|w| w.to_session_usage())
                .unwrap_or_default();
            let role_prompt = extract_role_prompt(&context);
            let pending_events = restored_pending_events
                .get(&row.agent_id)
                .cloned()
                .unwrap_or_default();
            let confirmed_work_count = op_count_by_agent.get(&row.agent_id).copied().unwrap_or(0);
            let depth = max_depth_by_agent.get(&row.agent_id).copied().unwrap_or(1);

            let descriptor = AgentDescriptor {
                agent_id: row.agent_id.clone(),
                name: row.name,
                preset_name: row.preset_name,
                interaction: row.interaction,
                definition,
                role_prompt,
                created_at_ms: row.created_at_ms,
            };
            let unit = AgentUnit::new(
                &session_id,
                descriptor,
                depth,
                row.material_revision,
                context,
                history,
                usage,
                pending_events,
                confirmed_work_count,
                store_root.as_deref(),
            );
            agent_order.push(row.agent_id.clone());
            agents.insert(row.agent_id, unit);
        }

        Arc::new(Self {
            session_id,
            entry_agent_id,
            persistent,
            host,
            entry,
            bus: Arc::new(SessionBus::default()),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            approvals,
            writer_gate,
            wakeup_notify: Arc::new(Notify::new()),
            state_changed: Arc::new(Notify::new()),
            shutdown: AtomicBool::new(false),
            state: Mutex::new(CoordinatorState {
                config,
                agents,
                agent_order,
                operations,
                dedup_index,
                active_root_turn: None,
                auto_wakeup_turns: HashSet::new(),
                command_rules,
                workspace_rules_backend,
                save_blocked: None,
                pending_admits: VecDeque::new(),
                pending_outboxes: VecDeque::new(),
                active_tasks: HashMap::new(),
                utility_tasks: Vec::new(),
                scheduler_task: None,
                child_saves_in_flight: 0,
                admissions_in_flight: 0,
            }),
        })
    }

    pub(crate) fn start_scheduler(self: &Arc<Self>, runtime: &tokio::runtime::Handle) {
        let coord = self.clone();
        let handle = runtime.spawn(async move {
            coord.run_scheduler().await;
        });
        self.state.lock().expect("coord lock").scheduler_task = Some(handle);
    }

    pub(crate) fn subscribe_events(&self) -> broadcast::Receiver<Arc<SessionEvent>> {
        self.bus.subscribe()
    }

    pub(crate) fn publish_lifecycle(&self, lifecycle: Lifecycle) {
        let label = match lifecycle {
            Lifecycle::Opening => "opening",
            Lifecycle::InitializationBlocked => "initialization_blocked",
            Lifecycle::Open => "open",
            Lifecycle::Closing => "closing",
            Lifecycle::Closed => "closed",
            Lifecycle::Faulted => "faulted",
            Lifecycle::Failed => "failed",
        };
        self.bus.publish(SessionEvent::SessionLifecycleChanged {
            session_id: self.session_id.clone(),
            lifecycle: label.to_owned(),
        });
    }

    #[cfg(test)]
    pub(crate) fn set_config(&self, config: CoordinatorConfig) {
        self.state.lock().expect("coord lock").config = config;
    }

    pub(crate) fn entry_inbox(&self) -> Arc<InboxDrainHandleSnapshot> {
        let state = self.state.lock().expect("coord lock");
        state
            .agents
            .get(&self.entry_agent_id)
            .expect("entry agent unit")
            .inbox
            .clone()
    }

    pub(crate) fn list_agents(&self) -> Vec<AgentSnapshot> {
        let entry_live = self.entry.upgrade().map(|entry| {
            let s = entry.lock();
            EntryLiveSummary::from_state(&s)
        });
        let state = self.state.lock().expect("coord lock");
        state
            .agent_order
            .iter()
            .filter_map(|id| state.agents.get(id))
            .map(|unit| {
                let mut snap = unit.snapshot();
                if unit.descriptor.agent_id == self.entry_agent_id
                    && let Some(live) = &entry_live
                {
                    live.apply_to_snapshot(&mut snap);
                }
                snap
            })
            .collect()
    }

    pub(crate) fn agent_snapshot(&self, agent_id: &str) -> Option<AgentSnapshot> {
        let entry_live = if agent_id == self.entry_agent_id {
            self.entry.upgrade().map(|entry| {
                let s = entry.lock();
                EntryLiveSummary::from_state(&s)
            })
        } else {
            None
        };
        let state = self.state.lock().expect("coord lock");
        let mut snap = state.agents.get(agent_id)?.snapshot();
        if let Some(live) = entry_live {
            live.apply_to_snapshot(&mut snap);
        }
        Some(snap)
    }

    pub(crate) fn active_finite_background_tasks(&self) -> usize {
        self.background_tasks.active_finite_count()
    }

    pub(crate) fn background_task_snapshot(&self) -> Vec<BackgroundTaskSnapshot> {
        self.background_tasks.snapshot()
    }

    pub(crate) async fn wait_background_tasks_settled(&self) {
        self.background_tasks.wait_all_finite_settled().await;
    }

    pub(crate) fn has_pending_child_saves(&self) -> bool {
        let state = self.state.lock().expect("coord lock");
        state.child_saves_in_flight > 0
            || !state.pending_admits.is_empty()
            || !state.pending_outboxes.is_empty()
    }

    pub(crate) fn save_blocked_error(&self) -> Option<Arc<StoreError>> {
        self.state.lock().expect("coord lock").save_blocked.clone()
    }

    pub(crate) fn take_child_mcps(&self) -> Vec<Arc<crate::mcp::McpRuntime>> {
        let mut state = self.state.lock().expect("coord lock");
        let entry_id = self.entry_agent_id.clone();
        state
            .agents
            .iter_mut()
            .filter(|(id, _)| **id != entry_id)
            .filter_map(|(_, unit)| unit.owner.as_mut().and_then(|o| o.take_mcp()))
            .collect()
    }

    pub(crate) fn update_command_rules(&self, rules: CommandRuleSet) {
        self.state.lock().expect("coord lock").command_rules = rules;
    }

    pub(crate) fn update_workspace_rules_backend(&self, backend: Option<WorkspaceRulesBackend>) {
        self.state
            .lock()
            .expect("coord lock")
            .workspace_rules_backend = backend;
    }

    pub(crate) fn note_entry_user_submit(&self, work: &WorkRef) {
        let mut state = self.state.lock().expect("coord lock");
        state.active_root_turn = Some(work.turn_id.clone());
        if let Some(unit) = state.agents.get_mut(&self.entry_agent_id) {
            unit.auto_wakeups_in_cycle = 0;
            unit.active_work = Some(work.clone());
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sync_entry_after_work_finished(
        &self,
        work: &WorkRef,
        result: &WorkResult,
        context: Context,
        history: Vec<HistoryRecordEntry>,
        usage: SessionUsage,
        generation: u64,
        material_revision: Option<u64>,
        consumed: &ConsumedInboxEvents,
    ) {
        let turn_completed = matches!(result, WorkResult::Run(TurnResult::Completed { .. }));
        let (to_cancel_tokens, to_suppress_ids, to_cancel_ops) = {
            let mut state = self.state.lock().expect("coord lock");
            if let Some(unit) = state.agents.get_mut(&self.entry_agent_id) {
                unit.context = context;
                unit.history = history;
                unit.usage = usage;
                unit.generation = generation;
                if let Some(rev) = material_revision {
                    unit.material_revision = rev;
                }
                if unit.active_work.as_ref() == Some(work) {
                    unit.active_work = None;
                }
                remove_consumed_from_pending(&mut unit.pending_events, consumed);
                if turn_completed {
                    unit.confirmed_work_count = unit.confirmed_work_count.saturating_add(1);
                }
            }
            if state.active_root_turn.as_ref() == Some(&work.turn_id) {
                state.active_root_turn = None;
            }
            collect_root_turn_cascade(&mut state, &work.turn_id, turn_completed)
        };

        for token in to_cancel_tokens {
            token.cancel();
        }
        self.suppress_events_sync_best_effort(&to_suppress_ids);
        let _ = to_cancel_ops;
        let status = work_result_to_terminal_status(result, false);
        self.bus.publish(SessionEvent::OperationStateChanged {
            session_id: self.session_id.clone(),
            operation_id: work.turn_id.0.clone(),
            target_agent_id: self.entry_agent_id.clone(),
            parent_operation_id: None,
            status,
        });
        self.wakeup_notify.notify_one();
        self.state_changed.notify_waiters();
    }

    pub(crate) fn sync_entry_materials_after_save(
        &self,
        material_revision: Option<u64>,
        generation: u64,
        context: Option<Context>,
        history: Option<Vec<HistoryRecordEntry>>,
        usage: Option<SessionUsage>,
        consumed: &ConsumedInboxEvents,
    ) {
        let mut state = self.state.lock().expect("coord lock");
        if let Some(unit) = state.agents.get_mut(&self.entry_agent_id) {
            if let Some(rev) = material_revision {
                unit.material_revision = rev;
            }
            unit.generation = generation;
            if let Some(ctx) = context {
                unit.context = ctx;
            }
            if let Some(hist) = history {
                unit.history = hist;
            }
            if let Some(u) = usage {
                unit.usage = u;
            }
            unit.saving = false;
            remove_consumed_from_pending(&mut unit.pending_events, consumed);
        }
        self.wakeup_notify.notify_one();
        self.state_changed.notify_waiters();
    }

    /// Synchronously signal cooperative cancellation for active children associated
    /// with an entry-agent root work (for example when `ManagedSession::cancel` is called).
    pub(crate) fn cancel_children_of_root_work(&self, work: &WorkRef) {
        let (tokens, suppress_ids, _) = {
            let mut state = self.state.lock().expect("coord lock");
            collect_root_turn_cascade(&mut state, &work.turn_id, false)
        };
        for token in tokens {
            token.cancel();
        }
        self.suppress_events_sync_best_effort(&suppress_ids);
        self.wakeup_notify.notify_one();
        self.state_changed.notify_waiters();
    }

    /// Collect active operation cancel tokens and wake-suppression IDs under `Coordinator::state`.
    ///
    /// Callers holding `Entry::state` may invoke this while keeping the `Entry -> Coordinator`
    /// lock order intact, then drop both locks before calling [`Self::apply_cancel_all_active`].
    pub(crate) fn collect_cancel_all_active(&self) -> (Vec<CancellationToken>, Vec<String>) {
        let mut state = self.state.lock().expect("coord lock");
        let mut tokens = Vec::new();
        let mut suppress_ids = Vec::new();
        for op in state.operations.values_mut() {
            if op.status.is_active() {
                op.suppress_wake = true;
                tokens.push(op.cancel_token.clone());
                suppress_ids.push(op.operation_id.clone());
            }
        }
        for unit in state.agents.values_mut() {
            for ev in &mut unit.pending_events {
                if ev.wake_policy == WakePolicy::WakeIfIdle {
                    ev.wake_policy = WakePolicy::SuppressWake;
                    if ev.source_kind == EventSourceKind::AgentOperation
                        && !suppress_ids.contains(&ev.source_id)
                    {
                        suppress_ids.push(ev.source_id.clone());
                    }
                }
            }
        }
        (tokens, suppress_ids)
    }

    /// Execute cooperative cancellation, background-task cancellation, wake suppression,
    /// and waiter notifications after releasing locks.
    pub(crate) fn apply_cancel_all_active(
        &self,
        tokens: Vec<CancellationToken>,
        suppress_ids: Vec<String>,
    ) {
        let _ = self.background_tasks.cancel_all();
        for token in tokens {
            token.cancel();
        }
        self.suppress_events_sync_best_effort(&suppress_ids);
        self.wakeup_notify.notify_waiters();
        self.state_changed.notify_waiters();
    }

    /// Cancel all active child operations and suppress pending idle wakeups without
    /// shutting down the session coordinator.
    #[allow(dead_code)]
    pub(crate) fn cancel_all_active_operations(&self) {
        let (tokens, suppress_ids) = self.collect_cancel_all_active();
        self.apply_cancel_all_active(tokens, suppress_ids);
    }

    /// Return `true` if any agent currently has a `WakeIfIdle` pending event and passes
    /// all admission guards (meaning the scheduler is about to wake or downgrade it).
    fn has_ready_wakeup_candidate_locked(
        &self,
        state: &CoordinatorState,
        entry_state: &mut crate::application::entry::State,
    ) -> bool {
        if entry_state.lifecycle != Lifecycle::Open
            || entry_state.closing.is_some()
            || entry_state.forking > 0
            || matches!(entry_state.save, SaveStatus::SaveBlocked { .. })
            || entry_state.fault.is_some()
            || state.save_blocked.is_some()
        {
            return false;
        }
        let entry_ref = self.entry.upgrade();
        let entry_media = entry_ref.as_deref().map(|e| &*e.media_vault);
        for agent_id in &state.agent_order {
            let Some(unit) = state.agents.get(agent_id) else {
                continue;
            };
            if !unit
                .pending_events
                .iter()
                .any(|e| e.wake_policy == WakePolicy::WakeIfIdle)
            {
                continue;
            }
            if passes_agent_guards(
                agent_id,
                &self.entry_agent_id,
                unit,
                entry_state,
                entry_media,
            ) {
                return true;
            }
        }
        false
    }

    /// Check coordinator settlement conditions while the caller already holds `Entry::state`.
    ///
    /// Returns `Err(ApplicationError::SaveBlocked(..))` if a child save/outbox is blocked,
    /// or `Ok(true)` when all finite background tasks, child saves, outboxes, admissions,
    /// active operation tasks, and ready idle-wakeup candidates have settled.
    pub(crate) fn check_settled_with_entry(
        &self,
        entry_state: &mut crate::application::entry::State,
    ) -> Result<bool, ApplicationError> {
        let state = self.state.lock().expect("coord lock");
        if let Some(error) = &state.save_blocked {
            return Err(ApplicationError::SaveBlocked(error.clone()));
        }
        Ok(self.active_finite_background_tasks() == 0
            && state.child_saves_in_flight == 0
            && state.pending_admits.is_empty()
            && state.pending_outboxes.is_empty()
            && state.admissions_in_flight == 0
            && state.active_tasks.is_empty()
            && !state.operations.values().any(|op| op.status.is_active())
            && !self.has_ready_wakeup_candidate_locked(&state, entry_state))
    }

    /// Return `true` when any finite background task, in-flight admission, active
    /// operation task, or eligible idle wakeup still requires execution (excluding pure
    /// persistence/saving waits).
    pub(crate) fn has_active_execution_with_entry(
        &self,
        entry_state: &mut crate::application::entry::State,
    ) -> bool {
        let state = self.state.lock().expect("coord lock");
        self.active_finite_background_tasks() > 0
            || state.admissions_in_flight > 0
            || !state.active_tasks.is_empty()
            || state.operations.values().any(|op| op.status.is_active())
            || self.has_ready_wakeup_candidate_locked(&state, entry_state)
    }

    /// Cancel all active operations and background tasks for session shutdown.
    pub(crate) fn begin_session_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let tokens = {
            let mut state = self.state.lock().expect("coord lock");
            let mut tokens = Vec::new();
            for op in state.operations.values_mut() {
                op.suppress_wake = true;
                if op.status.is_active() {
                    tokens.push(op.cancel_token.clone());
                }
            }
            for unit in state.agents.values_mut() {
                for event in &mut unit.pending_events {
                    event.wake_policy = WakePolicy::SuppressWake;
                }
            }
            tokens
        };
        let _ = self.background_tasks.cancel_all();
        for token in tokens {
            token.cancel();
        }
        self.wakeup_notify.notify_waiters();
        self.state_changed.notify_waiters();
    }

    /// Await all supervised background operation tasks, scheduler loop, and pending child saves.
    pub(crate) async fn wait_all_tasks_settled(&self) {
        self.begin_session_shutdown();
        let (scheduler, tasks) = {
            let mut state = self.state.lock().expect("coord lock");
            let scheduler = state.scheduler_task.take();
            let tasks: Vec<JoinHandle<()>> = state.active_tasks.drain().map(|(_, h)| h).collect();
            (scheduler, tasks)
        };
        if let Some(scheduler) = scheduler {
            let _ = scheduler.await;
        }
        for task in tasks {
            let _ = task.await;
        }
        loop {
            self.begin_session_shutdown();
            let notified = self.state_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let (more, utility, child_saves, admissions) = {
                let mut state = self.state.lock().expect("coord lock");
                let more: Vec<JoinHandle<()>> =
                    state.active_tasks.drain().map(|(_, h)| h).collect();
                let utility: Vec<JoinHandle<()>> = std::mem::take(&mut state.utility_tasks);
                (
                    more,
                    utility,
                    state.child_saves_in_flight,
                    state.admissions_in_flight,
                )
            };
            let had_more = !more.is_empty() || !utility.is_empty();
            for task in more {
                let _ = task.await;
            }
            for task in utility {
                let _ = task.await;
            }
            if !had_more && child_saves == 0 && admissions == 0 {
                break;
            }
            if !had_more {
                notified.await;
            }
        }
    }

    pub(crate) async fn retry_pending_outboxes(
        self: &Arc<Self>,
        entry: &Arc<Entry>,
    ) -> Result<(), ApplicationError> {
        let writer = {
            let state = entry.lock();
            state.writer.clone()
        };
        let Some(writer) = writer else {
            return Ok(());
        };
        let _writer_guard = self.writer_gate.lock().await;
        loop {
            let next_admit = {
                let mut state = self.state.lock().expect("coord lock");
                state.pending_admits.pop_front()
            };
            if let Some(pending) = next_admit {
                let w = writer.clone();
                let req_clone = pending.admit_req.clone();
                match saving::io(&self.host, move || w.admit_operation(&req_clone)).await {
                    Ok(AdmitOperationOutcome::Admitted {
                        session_revision, ..
                    }) => {
                        entry.lock().revision = Some(session_revision);
                        let op_id = pending.admit_req.operation_id;
                        let target_agent_id = pending.admit_req.target_agent_id;
                        self.bus.publish(SessionEvent::OperationStateChanged {
                            session_id: self.session_id.clone(),
                            operation_id: op_id.clone(),
                            target_agent_id,
                            parent_operation_id: None,
                            status: OperationStatus::Creating,
                        });
                        self.spawn_operation_task(
                            op_id,
                            pending.prompt,
                            pending.timeout_secs,
                            None,
                            None,
                            Some(pending.drained),
                        );
                        self.state_changed.notify_waiters();
                        continue;
                    }
                    Ok(AdmitOperationOutcome::Replayed(_))
                    | Ok(AdmitOperationOutcome::DedupConflict { .. }) => {
                        let mut state = self.state.lock().expect("coord lock");
                        rollback_child_reactive_wakeup(
                            &mut state,
                            &pending.admit_req.operation_id,
                            &pending.admit_req.target_agent_id,
                            &pending.drained,
                        );
                        drop(state);
                        self.state_changed.notify_waiters();
                        continue;
                    }
                    Err(ApplicationError::Store(err)) => {
                        let retain_admit =
                            writer.pending_kind() == crate::store::PendingWriterKind::Admit;
                        {
                            let mut es = entry.lock();
                            es.save = SaveStatus::SaveBlocked {
                                error: err.clone(),
                                commit_id: pending.admit_req.commit_id.clone(),
                                material_version: entry.version(&es),
                            };
                            es.error = Some(ApplicationError::SaveBlocked(err.clone()));
                            entry.changed();
                            let mut state = self.state.lock().expect("coord lock");
                            if retain_admit {
                                state.pending_admits.push_front(pending);
                            } else {
                                rollback_child_reactive_wakeup(
                                    &mut state,
                                    &pending.admit_req.operation_id,
                                    &pending.admit_req.target_agent_id,
                                    &pending.drained,
                                );
                            }
                            state.save_blocked = Some(err.clone());
                        }
                        self.state_changed.notify_waiters();
                        return Err(ApplicationError::SaveBlocked(err));
                    }
                    Err(other) => {
                        let mut state = self.state.lock().expect("coord lock");
                        if writer.pending_kind() == crate::store::PendingWriterKind::Admit {
                            state.pending_admits.push_front(pending);
                        } else {
                            rollback_child_reactive_wakeup(
                                &mut state,
                                &pending.admit_req.operation_id,
                                &pending.admit_req.target_agent_id,
                                &pending.drained,
                            );
                        }
                        drop(state);
                        self.state_changed.notify_waiters();
                        return Err(other);
                    }
                }
            }

            // Read entry revision BEFORE acquiring Coordinator::state to preserve
            // the global Entry::state -> Coordinator::state lock ordering.
            let current_rev = entry.lock().revision.unwrap_or(0);
            let next_item = {
                let mut state = self.state.lock().expect("coord lock");
                let Some(mut item) = state.pending_outboxes.pop_front() else {
                    state.save_blocked = None;
                    drop(state);
                    let mut es = entry.lock();
                    if es.pending.is_none() && matches!(es.save, SaveStatus::SaveBlocked { .. }) {
                        if let Some(rev) = es.revision {
                            es.save = SaveStatus::Saved {
                                revision: rev,
                                material_version: entry.version(&es),
                            };
                        } else {
                            es.save = SaveStatus::NotTracked;
                        }
                        es.error = None;
                        entry.changed();
                    }
                    drop(es);
                    self.wakeup_notify.notify_one();
                    self.state_changed.notify_waiters();
                    return Ok(());
                };
                if item.deferred_before_write && item.req.expected_session_revision < current_rev {
                    item.req.expected_session_revision = current_rev;
                }
                item
            };
            let w = writer.clone();
            let req_clone = next_item.req.clone();
            match saving::io(&self.host, move || w.commit_outbox(&req_clone)).await {
                Ok(res) => {
                    entry.lock().revision = Some(res.session_revision);
                    let mut state = self.state.lock().expect("coord lock");
                    if let Some(upd) = &next_item.req.agent_update
                        && let Some(unit) = state.agents.get_mut(&upd.agent_id)
                        && let Some(mat_rev) = res.agent_material_revision
                    {
                        unit.material_revision = mat_rev;
                    }
                }
                Err(ApplicationError::Store(err)) => {
                    let requeued = QueuedOutbox {
                        deferred_before_write: matches!(
                            &*err,
                            crate::store::StoreError::PendingCommit
                        ),
                        req: next_item.req,
                    };
                    let mut state = self.state.lock().expect("coord lock");
                    state.pending_outboxes.push_front(requeued);
                    state.save_blocked = Some(err.clone());
                    drop(state);
                    self.state_changed.notify_waiters();
                    return Err(ApplicationError::SaveBlocked(err));
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Spawn a new child agent and admit its initial operation.
    pub(crate) async fn spawn_agent_operation(
        self: &Arc<Self>,
        req: SpawnAgentRequest,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.run_supervised_admission_with_cancel(None, move |coord| async move {
            coord.spawn_agent_admit_and_launch(req).await
        })
        .await
    }

    /// Spawn a new child agent with cooperative cancellation bound to `cancel` for sync waits.
    pub(crate) async fn spawn_agent_with_cancel(
        self: &Arc<Self>,
        req: SpawnAgentRequest,
        cancel: CancellationToken,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.run_supervised_admission_with_cancel(Some(cancel), move |coord| async move {
            coord.spawn_agent_admit_and_launch(req).await
        })
        .await
    }

    /// Look up the caller agent's frozen definition and current live permission mode.
    pub(crate) fn caller_definition_and_mode(
        &self,
        caller_agent_id: &str,
    ) -> Result<(SessionDefinition, PermissionMode), CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        let entry_state = entry.lock();
        let entry_live_mode = entry_state
            .permissions
            .as_ref()
            .map(|p| p.get().0)
            .or_else(|| {
                entry_state
                    .definition
                    .as_ref()
                    .map(|d| d.initial_permission_mode())
            })
            .unwrap_or(PermissionMode::Ask);
        let state = self.state.lock().expect("coord lock");
        let caller_unit = state
            .agents
            .get(caller_agent_id)
            .ok_or_else(|| CoordinatorError::UnknownAgent(caller_agent_id.to_owned()))?;
        let caller_def = if caller_agent_id == self.entry_agent_id {
            entry_state
                .definition
                .clone()
                .unwrap_or_else(|| caller_unit.descriptor.definition.clone())
        } else {
            caller_unit.descriptor.definition.clone()
        };
        let caller_live_mode = if caller_agent_id == self.entry_agent_id {
            entry_live_mode
        } else {
            caller_unit
                .permissions
                .as_ref()
                .map(|p| p.get().0)
                .unwrap_or_else(|| caller_def.initial_permission_mode())
        };
        Ok((caller_def, caller_live_mode))
    }

    async fn run_supervised_admission_with_cancel<F, Fut>(
        self: &Arc<Self>,
        cancel: Option<CancellationToken>,
        op: F,
    ) -> Result<SpawnAgentOutcome, CoordinatorError>
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<AdmittedOutcome, CoordinatorError>>
            + Send
            + 'static,
    {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        {
            let mut state = self.state.lock().expect("coord lock");
            state.admissions_in_flight += 1;
        }
        let coord = self.clone();
        tokio::spawn(async move {
            let _guard = AdmissionInFlightGuard(coord.clone());
            let res = op(coord).await;
            let _ = tx.send(res);
        });
        let admitted = match cancel.as_ref() {
            Some(token) => {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        let coord = self.clone();
                        tokio::spawn(async move {
                            if let Ok(Ok(AdmittedOutcome::WaitSync(op_id))) = rx.await {
                                let _ = coord.cancel_agent(&op_id).await;
                            }
                        });
                        return Err(CoordinatorError::Cancelled);
                    }
                    res = &mut rx => res.map_err(|_| CoordinatorError::Application(ApplicationError::Closing))??,
                }
            }
            None => rx
                .await
                .map_err(|_| CoordinatorError::Application(ApplicationError::Closing))??,
        };
        match admitted {
            AdmittedOutcome::Immediate(outcome) => Ok(outcome),
            AdmittedOutcome::WaitSync(operation_id) => match cancel {
                Some(token) => {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            let _ = self.cancel_agent(&operation_id).await;
                            Err(CoordinatorError::Cancelled)
                        }
                        res = self.wait_operation_outcome(&operation_id) => {
                            Ok(SpawnAgentOutcome::Completed(res?))
                        }
                    }
                }
                None => {
                    let outcome = self.wait_operation_outcome(&operation_id).await?;
                    Ok(SpawnAgentOutcome::Completed(outcome))
                }
            },
        }
    }

    async fn spawn_agent_admit_and_launch(
        self: &Arc<Self>,
        req: SpawnAgentRequest,
    ) -> Result<AdmittedOutcome, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        self.check_open_and_unblocked(&entry)?;

        // 1. Check prompt and input size bounds before touching storage.
        if req.prompt.len() > MAX_PROMPT_BYTES {
            return Err(CoordinatorError::PromptTooLarge {
                bytes: req.prompt.len(),
                limit: MAX_PROMPT_BYTES,
            });
        }
        let fingerprint = fingerprint_spawn(&req)?;

        // 2. Dedup check BEFORE capacity / busy / repair checks (AC03).
        if let Some(source_block_id) = &req.source_block_id
            && let Some(outcome) = self
                .try_replay_dedup(
                    req.caller_operation_id.as_deref(),
                    source_block_id,
                    &fingerprint,
                    req.execution_mode,
                )
                .await?
        {
            return Ok(AdmittedOutcome::Immediate(outcome));
        }

        // 3. Validate caller, depth, concurrency, and narrowing (`child <= parent`).
        let (
            caller_def,
            caller_live_mode,
            depth,
            root_turn_id,
            child_def,
            effective_timeout_secs,
            created_at_ms,
        ) = {
            let entry_state = entry.lock();
            let entry_live_mode = entry_state
                .permissions
                .as_ref()
                .map(|p| p.get().0)
                .or_else(|| {
                    entry_state
                        .definition
                        .as_ref()
                        .map(|d| d.initial_permission_mode())
                })
                .unwrap_or(PermissionMode::Ask);
            let state = self.state.lock().expect("coord lock");
            let caller_unit = state
                .agents
                .get(&req.caller_agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(req.caller_agent_id.clone()))?;

            let (depth, root_turn_id) = match &req.caller_operation_id {
                Some(parent_op_id) => {
                    let parent_op = state
                        .operations
                        .get(parent_op_id)
                        .ok_or_else(|| CoordinatorError::UnknownOperation(parent_op_id.clone()))?;
                    if parent_op.target_agent_id != req.caller_agent_id {
                        return Err(CoordinatorError::CrossAgentOperation {
                            expected_agent_id: parent_op.target_agent_id.clone(),
                            actual_agent_id: req.caller_agent_id.clone(),
                        });
                    }
                    (parent_op.depth + 1, parent_op.root_turn_id.clone())
                }
                None => {
                    let root_turn_id = if req.caller_agent_id == self.entry_agent_id {
                        state.active_root_turn.clone()
                    } else {
                        None
                    };
                    (caller_unit.depth + 1, root_turn_id)
                }
            };

            if depth > state.config.max_delegation_depth {
                return Err(CoordinatorError::DepthExceeded {
                    depth,
                    max_depth: state.config.max_delegation_depth,
                });
            }

            check_concurrency_limits(&state, &req.caller_agent_id, &self.entry_agent_id)?;

            let caller_def = if req.caller_agent_id == self.entry_agent_id {
                entry_state
                    .definition
                    .clone()
                    .unwrap_or_else(|| caller_unit.descriptor.definition.clone())
            } else {
                caller_unit.descriptor.definition.clone()
            };
            let caller_live_mode = if req.caller_agent_id == self.entry_agent_id {
                entry_live_mode
            } else {
                caller_unit
                    .permissions
                    .as_ref()
                    .map(|p| p.get().0)
                    .unwrap_or_else(|| caller_def.initial_permission_mode())
            };

            let mut effective_narrowing = req.narrowing.clone();
            if let Some(max_rounds) = req.max_rounds {
                if let Some(narrowed_rounds) = effective_narrowing.max_model_rounds {
                    if max_rounds > narrowed_rounds {
                        return Err(CoordinatorError::CapabilityMismatch(
                            "requested max_rounds exceeds narrowing.max_model_rounds".into(),
                        ));
                    }
                } else {
                    effective_narrowing.max_model_rounds = Some(max_rounds);
                }
            }
            if let (Some(req_t), Some(narrow_t)) =
                (req.timeout_secs, effective_narrowing.work_timeout_secs)
                && req_t > narrow_t
            {
                return Err(CoordinatorError::CapabilityMismatch(
                    "requested timeout_secs exceeds narrowing.work_timeout_secs".into(),
                ));
            }
            let effective_timeout = match (req.timeout_secs, effective_narrowing.work_timeout_secs)
            {
                (Some(t), Some(nt)) => Some(t.min(nt)),
                (Some(t), None) => Some(t),
                (None, Some(t)) => Some(t),
                (None, None) => Some(match caller_def.work_timeout_secs {
                    Some(parent_t) => state.config.default_operation_timeout_secs.min(parent_t),
                    None => state.config.default_operation_timeout_secs,
                }),
            };
            if effective_narrowing.work_timeout_secs.is_none() {
                effective_narrowing.work_timeout_secs = effective_timeout;
            }

            let child_def = narrow_child_definition(&caller_def, &effective_narrowing)?;
            check_permission_mode_narrowing(caller_live_mode, child_def.initial_permission_mode())?;

            (
                caller_def,
                caller_live_mode,
                depth,
                root_turn_id,
                child_def,
                effective_timeout.unwrap_or(state.config.default_operation_timeout_secs),
                crate::time::current_time_ms(),
            )
        };
        let _ = (caller_def, caller_live_mode);

        let initial_context = build_initial_child_context(req.role_prompt.as_deref());
        let input_json = serialize_spawn_input(&req)?;
        if input_json.len() > MAX_OPERATION_INPUT_BYTES {
            return Err(CoordinatorError::InputTooLarge {
                bytes: input_json.len(),
                limit: MAX_OPERATION_INPUT_BYTES,
            });
        }

        let agent_id = uuid::Uuid::now_v7().to_string();
        let operation_id = uuid::Uuid::now_v7().to_string();
        let commit_id = uuid::Uuid::now_v7().to_string();
        let definition_json = serde_json::to_string(&child_def)
            .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;
        let context_json = serde_json::to_string(&initial_context)
            .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;

        let descriptor = AgentDescriptor {
            agent_id: agent_id.clone(),
            name: req.name.clone(),
            preset_name: req.preset_name.clone(),
            interaction: req.interaction,
            definition: child_def,
            role_prompt: req.role_prompt.clone(),
            created_at_ms,
        };

        let store_root = self
            .persistent
            .then(|| self.host.store.as_ref().map(|s| s.root().to_path_buf()))
            .flatten();

        // 4. Persist admission (`status = 'creating'`) under `writer_gate` if persistent.
        let writer = if self.persistent {
            entry.lock().writer.clone()
        } else {
            None
        };

        if let Some(writer) = writer {
            let _writer_guard = self.writer_gate.lock().await;
            self.check_open_and_unblocked(&entry)?;
            if let Some(source_block_id) = &req.source_block_id
                && let Some(outcome) = self
                    .try_replay_dedup(
                        req.caller_operation_id.as_deref(),
                        source_block_id,
                        &fingerprint,
                        req.execution_mode,
                    )
                    .await?
            {
                return Ok(AdmittedOutcome::Immediate(outcome));
            }

            let admit_req = AdmitOperationRequest {
                commit_id,
                operation_id: operation_id.clone(),
                target_agent_id: agent_id.clone(),
                parent_operation_id: req.caller_operation_id.clone(),
                parent_agent_id: Some(req.caller_agent_id.clone()),
                source_block_id: req.source_block_id.clone(),
                dedup_fingerprint: Some(fingerprint.clone()),
                execution_mode: req.execution_mode,
                on_parent_complete: req.on_parent_complete,
                depth,
                input_json,
                created_at_ms,
                new_agent: Some(NewAgentRow {
                    agent_id: agent_id.clone(),
                    name: req.name.clone(),
                    preset_name: req.preset_name.clone(),
                    interaction: req.interaction,
                    definition_json,
                    context_json,
                    history_index_json: "[]".to_owned(),
                    usage_json: None,
                    created_at_ms,
                    updated_at_ms: created_at_ms,
                }),
            };

            let admit_res = saving::io(&self.host, move || writer.admit_operation(&admit_req))
                .await
                .map_err(CoordinatorError::from)?;
            match admit_res {
                AdmitOperationOutcome::DedupConflict { existing } => {
                    return Err(CoordinatorError::DedupConflict {
                        existing_operation_id: existing.operation_id,
                    });
                }
                AdmitOperationOutcome::Replayed(existing) => {
                    drop(_writer_guard);
                    let outcome = self
                        .handle_replayed_row(existing, req.execution_mode)
                        .await?;
                    return Ok(AdmittedOutcome::Immediate(outcome));
                }
                AdmitOperationOutcome::Admitted {
                    session_revision, ..
                } => {
                    entry.lock().revision = Some(session_revision);
                }
            }
        }

        // 5. Register in memory, BackgroundTaskRegistry, and publish Plane 2 events.
        let mut unit = AgentUnit::new(
            &self.session_id,
            descriptor,
            depth,
            0,
            initial_context,
            Vec::new(),
            SessionUsage::default(),
            Vec::new(),
            0,
            store_root.as_deref(),
        );
        unit.active_operation_id = Some(operation_id.clone());

        let cancel_token = CancellationToken::new();
        let completion_notify = Arc::new(Notify::new());
        let op_record = OperationRecord {
            operation_id: operation_id.clone(),
            target_agent_id: agent_id.clone(),
            parent_operation_id: req.caller_operation_id.clone(),
            parent_agent_id: Some(req.caller_agent_id.clone()),
            root_turn_id,
            dedup_fingerprint: Some(fingerprint),
            execution_mode: req.execution_mode,
            on_parent_complete: req.on_parent_complete,
            depth,
            status: OperationStatus::Creating,
            result_text: None,
            error: None,
            finished_at_ms: None,
            cancel_token: cancel_token.clone(),
            suppress_wake: false,
            completion_notify: completion_notify.clone(),
        };
        let receipt = op_record.to_receipt(false);

        {
            let mut state = self.state.lock().expect("coord lock");
            if let Some(source_block_id) = &req.source_block_id {
                state.dedup_index.insert(
                    (req.caller_operation_id.clone(), source_block_id.clone()),
                    operation_id.clone(),
                );
            }
            state.agent_order.push(agent_id.clone());
            state.agents.insert(agent_id.clone(), unit);
            state.operations.insert(operation_id.clone(), op_record);
        }

        if req.execution_mode == OperationExecutionMode::Background {
            let _ = self.background_tasks.register(BackgroundTaskRegistration {
                task_id: operation_id.clone(),
                kind: BackgroundTaskKind::AgentOperation,
                owner_agent_id: req.caller_agent_id.clone(),
                parent_operation_id: req.caller_operation_id.clone(),
                policy: TaskLifecyclePolicy::Finite {
                    on_parent_complete: req.on_parent_complete,
                },
                status: OperationStatus::Creating,
                cancel_token,
                created_at_ms,
            });
            self.bus.publish(SessionEvent::BackgroundTaskChanged {
                session_id: self.session_id.clone(),
                task_id: operation_id.clone(),
                kind: BackgroundTaskKind::AgentOperation,
                status: OperationStatus::Creating,
            });
        }

        self.bus.publish(SessionEvent::AgentRegistered {
            session_id: self.session_id.clone(),
            agent_id: agent_id.clone(),
            name: req.name,
            preset_name: req.preset_name,
            interaction: req.interaction,
        });
        self.bus.publish(SessionEvent::OperationStateChanged {
            session_id: self.session_id.clone(),
            operation_id: operation_id.clone(),
            target_agent_id: agent_id,
            parent_operation_id: req.caller_operation_id,
            status: OperationStatus::Creating,
        });

        // 6. Spawn supervised execution task. Dropping this caller's future never aborts it.
        self.spawn_operation_task(
            operation_id.clone(),
            req.prompt,
            effective_timeout_secs,
            req.max_rounds,
            req.coalesce_key,
            None,
        );

        match req.execution_mode {
            OperationExecutionMode::Background => Ok(AdmittedOutcome::Immediate(
                SpawnAgentOutcome::Background(receipt),
            )),
            OperationExecutionMode::Sync => Ok(AdmittedOutcome::WaitSync(operation_id)),
        }
    }

    /// Send follow-up instructions to an existing `Reusable` idle agent.
    pub(crate) async fn message_agent_operation(
        self: &Arc<Self>,
        req: MessageAgentRequest,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.run_supervised_admission_with_cancel(None, move |coord| async move {
            coord.message_agent_admit_and_launch(req).await
        })
        .await
    }

    /// Send follow-up instructions with cooperative cancellation bound to `cancel` for sync waits.
    pub(crate) async fn message_agent_with_cancel(
        self: &Arc<Self>,
        req: MessageAgentRequest,
        cancel: CancellationToken,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.run_supervised_admission_with_cancel(Some(cancel), move |coord| async move {
            coord.message_agent_admit_and_launch(req).await
        })
        .await
    }

    async fn message_agent_admit_and_launch(
        self: &Arc<Self>,
        req: MessageAgentRequest,
    ) -> Result<AdmittedOutcome, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        self.check_open_and_unblocked(&entry)?;

        if req.prompt.len() > MAX_PROMPT_BYTES {
            return Err(CoordinatorError::PromptTooLarge {
                bytes: req.prompt.len(),
                limit: MAX_PROMPT_BYTES,
            });
        }
        let fingerprint = fingerprint_message(&req)?;

        // 1. Dedup check BEFORE capacity / busy / SingleWork / repair checks (AC03).
        if let Some(source_block_id) = &req.source_block_id
            && let Some(outcome) = self
                .try_replay_dedup(
                    req.caller_operation_id.as_deref(),
                    source_block_id,
                    &fingerprint,
                    req.execution_mode,
                )
                .await?
        {
            return Ok(AdmittedOutcome::Immediate(outcome));
        }

        // 2. Validate isolation, depth, concurrency, reusability, idle, repair, and capability coverage.
        if req.caller_agent_id == req.target_agent_id {
            return Err(CoordinatorError::SelfDelegation);
        }

        let operation_id = uuid::Uuid::now_v7().to_string();
        let (depth, root_turn_id, effective_timeout_secs, created_at_ms) = {
            let entry_state = entry.lock();
            let entry_live_mode = entry_state
                .permissions
                .as_ref()
                .map(|p| p.get().0)
                .or_else(|| {
                    entry_state
                        .definition
                        .as_ref()
                        .map(|d| d.initial_permission_mode())
                })
                .unwrap_or(PermissionMode::Ask);
            let mut state = self.state.lock().expect("coord lock");

            // Check concurrency limits before borrowing mutable target_unit.
            check_concurrency_limits(&state, &req.caller_agent_id, &self.entry_agent_id)?;

            // Ancestor cycle detection.
            if req.target_agent_id == self.entry_agent_id {
                return Err(CoordinatorError::DelegationCycle);
            }
            if has_ancestor_agent(
                &state.operations,
                req.caller_operation_id.as_deref(),
                &req.target_agent_id,
            ) {
                return Err(CoordinatorError::DelegationCycle);
            }

            let caller_unit = state
                .agents
                .get(&req.caller_agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(req.caller_agent_id.clone()))?;
            let target_unit = state
                .agents
                .get(&req.target_agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(req.target_agent_id.clone()))?;

            // Depth check (`message_agent` never resets depth).
            let (depth, root_turn_id) = match &req.caller_operation_id {
                Some(parent_op_id) => {
                    let parent_op = state
                        .operations
                        .get(parent_op_id)
                        .ok_or_else(|| CoordinatorError::UnknownOperation(parent_op_id.clone()))?;
                    if parent_op.target_agent_id != req.caller_agent_id {
                        return Err(CoordinatorError::CrossAgentOperation {
                            expected_agent_id: parent_op.target_agent_id.clone(),
                            actual_agent_id: req.caller_agent_id.clone(),
                        });
                    }
                    (parent_op.depth + 1, parent_op.root_turn_id.clone())
                }
                None => {
                    let root_turn_id = if req.caller_agent_id == self.entry_agent_id {
                        state.active_root_turn.clone()
                    } else {
                        None
                    };
                    (caller_unit.depth + 1, root_turn_id)
                }
            };

            if depth > state.config.max_delegation_depth {
                return Err(CoordinatorError::DepthExceeded {
                    depth,
                    max_depth: state.config.max_delegation_depth,
                });
            }

            // Reusability check (`SingleWork` rejects after initial work or while active).
            if target_unit.descriptor.interaction == AgentInteraction::SingleWork {
                let any_prior_op = state
                    .operations
                    .values()
                    .any(|op| op.target_agent_id == req.target_agent_id);
                if target_unit.confirmed_work_count > 0
                    || target_unit.active_operation_id.is_some()
                    || any_prior_op
                {
                    return Err(CoordinatorError::NotReusable(req.target_agent_id.clone()));
                }
            }

            // Idle check (no implicit queueing).
            if let Some(active_op) = &target_unit.active_operation_id {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: req.target_agent_id.clone(),
                    active_operation_id: active_op.clone(),
                });
            }
            if !target_unit.is_idle_for_wakeup() {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: req.target_agent_id.clone(),
                    active_operation_id: "saving_or_compacting".to_owned(),
                });
            }

            // Repair check (must not mutate target Context).
            if target_unit.needs_repair() {
                return Err(CoordinatorError::AgentNeedsRepair(
                    req.target_agent_id.clone(),
                ));
            }

            // Capability compatibility check: caller's frozen capabilities must cover target's.
            let caller_def = if req.caller_agent_id == self.entry_agent_id {
                entry_state
                    .definition
                    .as_ref()
                    .unwrap_or(&caller_unit.descriptor.definition)
            } else {
                &caller_unit.descriptor.definition
            };
            let caller_live_mode = if req.caller_agent_id == self.entry_agent_id {
                entry_live_mode
            } else {
                caller_unit
                    .permissions
                    .as_ref()
                    .map(|p| p.get().0)
                    .unwrap_or_else(|| caller_def.initial_permission_mode())
            };
            let target_def = &target_unit.descriptor.definition;
            let target_live_mode = target_unit
                .permissions
                .as_ref()
                .map(|p| p.get().0)
                .unwrap_or_else(|| target_def.initial_permission_mode());

            verify_capability_coverage(
                caller_def,
                caller_live_mode,
                target_def,
                target_live_mode,
                req.timeout_secs,
                req.max_rounds,
            )?;

            let effective_timeout = match (req.timeout_secs, target_def.work_timeout_secs) {
                (Some(t), Some(tt)) => t.min(tt),
                (Some(t), None) => t,
                (None, Some(t)) => t,
                (None, None) => match caller_def.work_timeout_secs {
                    Some(pt) => state.config.default_operation_timeout_secs.min(pt),
                    None => state.config.default_operation_timeout_secs,
                },
            };
            let effective_timeout = caller_def
                .work_timeout_secs
                .map_or(effective_timeout, |pt| effective_timeout.min(pt));

            // Atomically reserve the target agent before releasing Coordinator::state so
            // concurrent callers cannot both enter `admit_operation` for the same agent.
            let target_unit_mut = state
                .agents
                .get_mut(&req.target_agent_id)
                .expect("checked target agent exists");
            target_unit_mut.active_operation_id = Some(operation_id.clone());

            (
                depth,
                root_turn_id,
                effective_timeout,
                crate::time::current_time_ms(),
            )
        };

        let mut reservation = TargetAgentReservation {
            coord: self.clone(),
            target_agent_id: req.target_agent_id.clone(),
            operation_id: operation_id.clone(),
            committed: false,
        };

        let input_json = serialize_message_input(&req)?;
        if input_json.len() > MAX_OPERATION_INPUT_BYTES {
            return Err(CoordinatorError::InputTooLarge {
                bytes: input_json.len(),
                limit: MAX_OPERATION_INPUT_BYTES,
            });
        }

        let commit_id = uuid::Uuid::now_v7().to_string();
        let writer = if self.persistent {
            entry.lock().writer.clone()
        } else {
            None
        };

        if let Some(writer) = writer {
            let _writer_guard = self.writer_gate.lock().await;
            self.check_open_and_unblocked(&entry)?;
            if let Some(source_block_id) = &req.source_block_id
                && let Some(outcome) = self
                    .try_replay_dedup(
                        req.caller_operation_id.as_deref(),
                        source_block_id,
                        &fingerprint,
                        req.execution_mode,
                    )
                    .await?
            {
                return Ok(AdmittedOutcome::Immediate(outcome));
            }

            let admit_req = AdmitOperationRequest {
                commit_id,
                operation_id: operation_id.clone(),
                target_agent_id: req.target_agent_id.clone(),
                parent_operation_id: req.caller_operation_id.clone(),
                parent_agent_id: Some(req.caller_agent_id.clone()),
                source_block_id: req.source_block_id.clone(),
                dedup_fingerprint: Some(fingerprint.clone()),
                execution_mode: req.execution_mode,
                on_parent_complete: req.on_parent_complete,
                depth,
                input_json,
                created_at_ms,
                new_agent: None,
            };

            let admit_res = saving::io(&self.host, move || writer.admit_operation(&admit_req))
                .await
                .map_err(CoordinatorError::from)?;
            match admit_res {
                AdmitOperationOutcome::DedupConflict { existing } => {
                    return Err(CoordinatorError::DedupConflict {
                        existing_operation_id: existing.operation_id,
                    });
                }
                AdmitOperationOutcome::Replayed(existing) => {
                    drop(_writer_guard);
                    let outcome = self
                        .handle_replayed_row(existing, req.execution_mode)
                        .await?;
                    return Ok(AdmittedOutcome::Immediate(outcome));
                }
                AdmitOperationOutcome::Admitted {
                    session_revision, ..
                } => {
                    entry.lock().revision = Some(session_revision);
                }
            }
        }

        let cancel_token = CancellationToken::new();
        let completion_notify = Arc::new(Notify::new());
        let op_record = OperationRecord {
            operation_id: operation_id.clone(),
            target_agent_id: req.target_agent_id.clone(),
            parent_operation_id: req.caller_operation_id.clone(),
            parent_agent_id: Some(req.caller_agent_id.clone()),
            root_turn_id,
            dedup_fingerprint: Some(fingerprint),
            execution_mode: req.execution_mode,
            on_parent_complete: req.on_parent_complete,
            depth,
            status: OperationStatus::Creating,
            result_text: None,
            error: None,
            finished_at_ms: None,
            cancel_token: cancel_token.clone(),
            suppress_wake: false,
            completion_notify: completion_notify.clone(),
        };
        let receipt = op_record.to_receipt(false);

        {
            let mut state = self.state.lock().expect("coord lock");
            let target_unit = state
                .agents
                .get_mut(&req.target_agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(req.target_agent_id.clone()))?;
            if target_unit.active_operation_id.as_deref() != Some(operation_id.as_str()) {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: req.target_agent_id.clone(),
                    active_operation_id: target_unit
                        .active_operation_id
                        .clone()
                        .unwrap_or_default(),
                });
            }
            if let Some(source_block_id) = &req.source_block_id {
                state.dedup_index.insert(
                    (req.caller_operation_id.clone(), source_block_id.clone()),
                    operation_id.clone(),
                );
            }
            state.operations.insert(operation_id.clone(), op_record);
            reservation.committed = true;
        }

        if req.execution_mode == OperationExecutionMode::Background {
            let _ = self.background_tasks.register(BackgroundTaskRegistration {
                task_id: operation_id.clone(),
                kind: BackgroundTaskKind::AgentOperation,
                owner_agent_id: req.caller_agent_id.clone(),
                parent_operation_id: req.caller_operation_id.clone(),
                policy: TaskLifecyclePolicy::Finite {
                    on_parent_complete: req.on_parent_complete,
                },
                status: OperationStatus::Creating,
                cancel_token,
                created_at_ms,
            });
            self.bus.publish(SessionEvent::BackgroundTaskChanged {
                session_id: self.session_id.clone(),
                task_id: operation_id.clone(),
                kind: BackgroundTaskKind::AgentOperation,
                status: OperationStatus::Creating,
            });
        }

        self.bus.publish(SessionEvent::OperationStateChanged {
            session_id: self.session_id.clone(),
            operation_id: operation_id.clone(),
            target_agent_id: req.target_agent_id,
            parent_operation_id: req.caller_operation_id,
            status: OperationStatus::Creating,
        });

        self.spawn_operation_task(
            operation_id.clone(),
            req.prompt,
            effective_timeout_secs,
            req.max_rounds,
            req.coalesce_key,
            None,
        );

        match req.execution_mode {
            OperationExecutionMode::Background => Ok(AdmittedOutcome::Immediate(
                SpawnAgentOutcome::Background(receipt),
            )),
            OperationExecutionMode::Sync => Ok(AdmittedOutcome::WaitSync(operation_id)),
        }
    }

    pub(crate) async fn read_agent(
        &self,
        operation_id: &str,
        offset_bytes: usize,
        limit_bytes: usize,
    ) -> Result<ReadAgentResult, CoordinatorError> {
        if let Some(entry) = self.entry.upgrade() {
            let lifecycle = entry.lock().lifecycle;
            if lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed.into());
            }
        }

        // Check in-memory first for active/queued operations (including SaveBlocked settled ops).
        let in_mem = {
            let state = self.state.lock().expect("coord lock");
            state.operations.get(operation_id).map(|op| {
                (
                    op.target_agent_id.clone(),
                    op.status,
                    op.result_text.clone(),
                    op.error.clone(),
                )
            })
        };

        if self.persistent
            && let Some(store) = self.host.store.clone()
        {
            let session_id = self.session_id.clone();
            let op_id = operation_id.to_owned();
            let page_res = saving::io_semaphore(&self.entry_io_or_host(), move || {
                store.read_agent_output_page(&session_id, &op_id, offset_bytes, limit_bytes)
            })
            .await
            .map_err(CoordinatorError::from)?;

            if let Some(read_page) = page_res {
                // If SQLite still shows active because an outbox commit is queued behind SaveBlocked,
                // prefer the settled in-memory terminal result if available.
                if read_page.operation.status.is_active()
                    && let Some((ref target_agent_id, status, ref result_text, ref error)) = in_mem
                    && status.is_terminal()
                {
                    let page = slice_utf8_page(
                        result_text.as_deref().unwrap_or(""),
                        offset_bytes,
                        limit_bytes,
                    );
                    return Ok(ReadAgentResult {
                        operation_id: operation_id.to_owned(),
                        target_agent_id: target_agent_id.clone(),
                        status,
                        page,
                        error: error.clone(),
                    });
                }
                let error =
                    parse_error_from_outcome_json(read_page.operation.outcome_json.as_deref())
                        .or_else(|| in_mem.and_then(|(_, _, _, e)| e));
                return Ok(ReadAgentResult {
                    operation_id: read_page.operation.operation_id,
                    target_agent_id: read_page.operation.target_agent_id,
                    status: read_page.operation.status,
                    page: read_page.page,
                    error,
                });
            }
        }

        let Some((target_agent_id, status, result_text, error)) = in_mem else {
            return Err(CoordinatorError::UnknownOperation(operation_id.to_owned()));
        };
        let page = slice_utf8_page(
            result_text.as_deref().unwrap_or(""),
            offset_bytes,
            limit_bytes,
        );
        Ok(ReadAgentResult {
            operation_id: operation_id.to_owned(),
            target_agent_id,
            status,
            page,
            error,
        })
    }

    /// Initiate cooperative cancellation of `operation_id` and its entire descendant subtree,
    /// awaiting true settlement while preserving any already-won terminal status.
    pub(crate) async fn cancel_agent(
        &self,
        operation_id: &str,
    ) -> Result<OperationOutcome, CoordinatorError> {
        let (outcome, _) = self.cancel_operation_inner(operation_id).await?;
        Ok(outcome)
    }

    /// Cooperatively cancel `operation_id` (and any active descendants), returning `Ok(true)` if
    /// `operation_id` was active when cancellation was initiated or `Ok(false)` if it was already
    /// terminal.
    pub(crate) async fn cancel_operation(
        &self,
        operation_id: &str,
    ) -> Result<bool, CoordinatorError> {
        let (_, was_active) = self.cancel_operation_inner(operation_id).await?;
        Ok(was_active)
    }

    async fn cancel_operation_inner(
        &self,
        operation_id: &str,
    ) -> Result<(OperationOutcome, bool), CoordinatorError> {
        let (was_active, wait_ids, tokens, suppress_ids) = {
            let mut state = self.state.lock().expect("coord lock");
            let was_active = state
                .operations
                .get(operation_id)
                .ok_or_else(|| CoordinatorError::UnknownOperation(operation_id.to_owned()))?
                .status
                .is_active();
            let subtree = collect_operation_subtree(&state.operations, operation_id);
            let mut tokens = Vec::new();
            let mut wait_ids = Vec::new();
            for id in &subtree {
                if let Some(op) = state.operations.get_mut(id) {
                    op.suppress_wake = true;
                    if op.status.is_active() {
                        tokens.push(op.cancel_token.clone());
                        wait_ids.push(id.clone());
                    }
                }
            }
            suppress_in_memory_events_for_sources(&mut state.agents, &subtree);
            (was_active, wait_ids, tokens, subtree)
        };

        for token in tokens {
            token.cancel();
        }
        if !suppress_ids.is_empty() {
            self.suppress_events_sync_best_effort(&suppress_ids);
            self.wakeup_notify.notify_one();
        }

        for id in wait_ids {
            let _ = self.wait_operation_outcome(&id).await;
        }
        let state = self.state.lock().expect("coord lock");
        let op = state
            .operations
            .get(operation_id)
            .ok_or_else(|| CoordinatorError::UnknownOperation(operation_id.to_owned()))?;
        Ok((op.to_outcome(), was_active))
    }

    pub(crate) fn read_agent_materials(
        &self,
        agent_id: &str,
    ) -> Result<AgentMaterialSnapshot, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        if agent_id == self.entry_agent_id {
            let managed = crate::application::ManagedSession { entry };
            let snap = managed.read_materials()?;
            return Ok(AgentMaterialSnapshot {
                version: AgentMaterialVersion {
                    target: snap.version.target,
                    agent_id: agent_id.to_owned(),
                    generation: snap.version.generation,
                },
                context: snap.context,
                history: snap.history,
            });
        }

        let entry_state = entry.lock();
        if entry_state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed.into());
        }
        let state = self.state.lock().expect("coord lock");
        let unit = state
            .agents
            .get(agent_id)
            .ok_or_else(|| CoordinatorError::UnknownAgent(agent_id.to_owned()))?;
        if let Some(active_op) = &unit.active_operation_id {
            return Err(CoordinatorError::AgentBusy {
                agent_id: agent_id.to_owned(),
                active_operation_id: active_op.clone(),
            });
        }
        Ok(AgentMaterialSnapshot {
            version: AgentMaterialVersion {
                target: entry.target.clone(),
                agent_id: agent_id.to_owned(),
                generation: unit.generation,
            },
            context: unit.context.clone(),
            history: unit.history.clone(),
        })
    }

    pub(crate) fn edit_agent_materials(
        self: &Arc<Self>,
        base: AgentMaterialVersion,
        blocks: Vec<ContextBlock>,
    ) -> Result<AgentMaterialVersion, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        if base.target != entry.target {
            return Err(ApplicationError::StaleInstance.into());
        }
        if base.agent_id == self.entry_agent_id {
            let managed = crate::application::ManagedSession { entry };
            let next = managed.edit(
                MaterialVersion {
                    target: base.target,
                    generation: base.generation,
                },
                blocks,
            )?;
            return Ok(AgentMaterialVersion {
                target: next.target,
                agent_id: base.agent_id,
                generation: next.generation,
            });
        }

        let (writer, agent_id, mat_rev, candidate, history_json, usage_json, next_gen) = {
            let entry_state = entry.lock();
            self.check_open_entry_state(&entry_state)?;
            let mut state = self.state.lock().expect("coord lock");
            if let Some(err) = &state.save_blocked {
                return Err(ApplicationError::SaveBlocked(err.clone()).into());
            }
            let unit = state
                .agents
                .get_mut(&base.agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(base.agent_id.clone()))?;
            if base.generation != unit.generation {
                return Err(ApplicationError::StaleMaterial.into());
            }
            if let Some(active_op) = &unit.active_operation_id {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: base.agent_id.clone(),
                    active_operation_id: active_op.clone(),
                });
            }
            if !unit.is_idle_for_wakeup() {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: base.agent_id.clone(),
                    active_operation_id: "saving_or_compacting".to_owned(),
                });
            }
            let mut candidate = unit.context.clone();
            let len = candidate.blocks().len();
            candidate
                .edit()
                .replace(0..len, blocks)
                .commit()
                .map_err(|failure| SessionError::ContextEdit(failure.reason))?;
            let next_gen = unit
                .generation
                .checked_add(1)
                .ok_or_else(|| ApplicationError::Faulted("material generation exhausted".into()))?;
            if let Some(owner) = &mut unit.owner {
                owner.with_context(|ctx| *ctx = candidate.clone())?;
            }
            unit.context = candidate.clone();
            unit.generation = next_gen;
            let history_json = serde_json::to_string(&unit.history)
                .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;
            let usage_json = crate::store::WireSessionUsage::from_session_usage(&unit.usage);
            let usage_json = (!usage_json.is_empty())
                .then(|| serde_json::to_string(&usage_json))
                .transpose()
                .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;
            let mat_rev = unit.material_revision;
            if self.persistent && entry_state.writer.is_some() {
                unit.saving = true;
                state.child_saves_in_flight += 1;
            }
            (
                entry_state.writer.clone(),
                base.agent_id.clone(),
                mat_rev,
                candidate,
                history_json,
                usage_json,
                next_gen,
            )
        };

        if self.persistent
            && let Some(writer) = writer
        {
            self.spawn_child_material_save(
                writer,
                agent_id.clone(),
                mat_rev,
                candidate,
                history_json,
                usage_json,
            );
        }
        Ok(AgentMaterialVersion {
            target: entry.target.clone(),
            agent_id,
            generation: next_gen,
        })
    }

    pub(crate) fn inspect_agent_repair(
        &self,
        agent_id: &str,
        work: Option<&WorkRef>,
    ) -> Result<AgentRepairInspection, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        if agent_id == self.entry_agent_id {
            let managed = crate::application::ManagedSession { entry };
            let inspection = managed.inspect_repair(work)?;
            return Ok(AgentRepairInspection {
                version: AgentMaterialVersion {
                    target: inspection.version.target,
                    agent_id: agent_id.to_owned(),
                    generation: inspection.version.generation,
                },
                diagnosis: inspection.diagnosis,
                original: inspection.original,
            });
        }

        let entry_state = entry.lock();
        if entry_state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed.into());
        }
        let state = self.state.lock().expect("coord lock");
        let unit = state
            .agents
            .get(agent_id)
            .ok_or_else(|| CoordinatorError::UnknownAgent(agent_id.to_owned()))?;
        if let Some(active_op) = &unit.active_operation_id {
            return Err(CoordinatorError::AgentBusy {
                agent_id: agent_id.to_owned(),
                active_operation_id: active_op.clone(),
            });
        }
        let version = AgentMaterialVersion {
            target: entry.target.clone(),
            agent_id: agent_id.to_owned(),
            generation: unit.generation,
        };
        let original = match (work, &unit.owner) {
            (Some(work), Some(owner)) => Some(repair::inspect_original_batch(owner, work)?),
            _ => None,
        };
        let owner_history = unit
            .owner
            .as_ref()
            .and_then(|o| o.compaction())
            .and_then(|c| c.history());
        let artifacts = owner_history.as_deref().or(unit.history_store.as_deref());
        let diagnosis = repair::diagnose(
            &unit.context,
            &unit.history,
            artifacts,
            &unit.descriptor.definition,
        );
        Ok(AgentRepairInspection {
            version,
            diagnosis,
            original,
        })
    }

    pub(crate) fn prepare_agent_repair(
        &self,
        agent_id: &str,
        mode: RepairMode,
    ) -> Result<AgentRepairPlan, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        if agent_id == self.entry_agent_id {
            let managed = crate::application::ManagedSession { entry };
            let plan = managed.plan_repair(mode)?;
            return Ok(AgentRepairPlan::new(
                AgentMaterialVersion {
                    target: plan.base().target.clone(),
                    agent_id: agent_id.to_owned(),
                    generation: plan.base().generation,
                },
                plan.claims_revision,
                plan.mode,
                plan.candidate,
                plan.summary,
            ));
        }

        let entry_state = entry.lock();
        self.check_open_entry_state(&entry_state)?;
        let state = self.state.lock().expect("coord lock");
        if let Some(err) = &state.save_blocked {
            return Err(ApplicationError::SaveBlocked(err.clone()).into());
        }
        let unit = state
            .agents
            .get(agent_id)
            .ok_or_else(|| CoordinatorError::UnknownAgent(agent_id.to_owned()))?;
        if let Some(active_op) = &unit.active_operation_id {
            return Err(CoordinatorError::AgentBusy {
                agent_id: agent_id.to_owned(),
                active_operation_id: active_op.clone(),
            });
        }
        if !unit.is_idle_for_wakeup() {
            return Err(CoordinatorError::AgentBusy {
                agent_id: agent_id.to_owned(),
                active_operation_id: "saving_or_compacting".to_owned(),
            });
        }
        let base = AgentMaterialVersion {
            target: entry.target.clone(),
            agent_id: agent_id.to_owned(),
            generation: unit.generation,
        };
        let claims_revision = unit.owner.as_ref().map_or(0, |o| o.claims_revision());
        let owner_history = unit
            .owner
            .as_ref()
            .and_then(|o| o.compaction())
            .and_then(|c| c.history());
        let artifacts = owner_history.as_deref().or(unit.history_store.as_deref());
        let built = repair::build_repair_plan(
            &unit.context,
            &unit.history,
            artifacts,
            None,
            &unit.descriptor.definition,
            unit.owner.as_ref(),
            &mode,
        )?;
        Ok(AgentRepairPlan::new(
            base,
            claims_revision,
            mode,
            built.candidate,
            built.summary,
        ))
    }

    pub(crate) fn apply_agent_repair(
        self: &Arc<Self>,
        plan: AgentRepairPlan,
    ) -> Result<AgentMaterialVersion, CoordinatorError> {
        let Some(entry) = self.entry.upgrade() else {
            return Err(ApplicationError::InstanceGone.into());
        };
        if plan.base.target != entry.target {
            return Err(ApplicationError::StaleInstance.into());
        }
        if plan.base.agent_id == self.entry_agent_id {
            let repair_plan = crate::application::RepairPlan {
                base: MaterialVersion {
                    target: plan.base.target,
                    generation: plan.base.generation,
                },
                claims_revision: plan.claims_revision,
                mode: plan.mode,
                candidate: plan.candidate,
                summary: plan.summary,
            };
            let managed = crate::application::ManagedSession { entry };
            let next = managed.apply_repair(&repair_plan)?;
            return Ok(AgentMaterialVersion {
                target: next.target,
                agent_id: self.entry_agent_id.clone(),
                generation: next.generation,
            });
        }

        let (writer, agent_id, mat_rev, candidate, history_json, usage_json, next_gen) = {
            let entry_state = entry.lock();
            self.check_open_entry_state(&entry_state)?;
            let mut state = self.state.lock().expect("coord lock");
            if let Some(err) = &state.save_blocked {
                return Err(ApplicationError::SaveBlocked(err.clone()).into());
            }
            let unit = state
                .agents
                .get_mut(&plan.base.agent_id)
                .ok_or_else(|| CoordinatorError::UnknownAgent(plan.base.agent_id.clone()))?;
            if plan.base.generation != unit.generation {
                return Err(ApplicationError::StaleMaterial.into());
            }
            if let Some(active_op) = &unit.active_operation_id {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: plan.base.agent_id.clone(),
                    active_operation_id: active_op.clone(),
                });
            }
            if !unit.is_idle_for_wakeup() {
                return Err(CoordinatorError::AgentBusy {
                    agent_id: plan.base.agent_id.clone(),
                    active_operation_id: "saving_or_compacting".to_owned(),
                });
            }
            let claims_rev = unit.owner.as_ref().map_or(0, |o| o.claims_revision());
            if plan.claims_revision != claims_rev {
                return Err(crate::application::RepairError::StaleEvidence.into());
            }
            if let RepairMode::AdoptOriginal(work) = &plan.mode {
                let owner = unit
                    .owner
                    .as_ref()
                    .ok_or(crate::application::RepairError::OriginalUnavailable)?;
                owner.inspect_uncommitted(work, repair::source_check)??;
            }
            let next_gen = unit
                .generation
                .checked_add(1)
                .ok_or_else(|| ApplicationError::Faulted("material generation exhausted".into()))?;
            let candidate = plan.candidate;
            if let Some(owner) = &mut unit.owner {
                owner.with_context(|ctx| *ctx = candidate.clone())?;
            }
            unit.context = candidate.clone();
            unit.generation = next_gen;
            let history_json = serde_json::to_string(&unit.history)
                .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;
            let usage_json = crate::store::WireSessionUsage::from_session_usage(&unit.usage);
            let usage_json = (!usage_json.is_empty())
                .then(|| serde_json::to_string(&usage_json))
                .transpose()
                .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))?;
            let mat_rev = unit.material_revision;
            if self.persistent && entry_state.writer.is_some() {
                unit.saving = true;
                state.child_saves_in_flight += 1;
            }
            (
                entry_state.writer.clone(),
                plan.base.agent_id,
                mat_rev,
                candidate,
                history_json,
                usage_json,
                next_gen,
            )
        };

        if self.persistent
            && let Some(writer) = writer
        {
            self.spawn_child_material_save(
                writer,
                agent_id.clone(),
                mat_rev,
                candidate,
                history_json,
                usage_json,
            );
        }
        Ok(AgentMaterialVersion {
            target: entry.target.clone(),
            agent_id,
            generation: next_gen,
        })
    }

    // -----------------------------------------------------------------------
    // Internal scheduling, execution, outbox, and wakeup engine
    // -----------------------------------------------------------------------

    fn entry_io_or_host(&self) -> Arc<tokio::sync::Semaphore> {
        self.host.io.clone()
    }

    fn check_open_and_unblocked(&self, entry: &Entry) -> Result<(), CoordinatorError> {
        let entry_state = entry.lock();
        self.check_open_entry_state(&entry_state)?;
        drop(entry_state);
        if let Some(err) = self.save_blocked_error() {
            return Err(ApplicationError::SaveBlocked(err).into());
        }
        Ok(())
    }

    fn check_open_entry_state(
        &self,
        state: &crate::application::entry::State,
    ) -> Result<(), CoordinatorError> {
        match state.lifecycle {
            Lifecycle::Closing => return Err(ApplicationError::Closing.into()),
            Lifecycle::Closed => return Err(ApplicationError::Closed.into()),
            Lifecycle::Faulted => {
                return Err(state
                    .error
                    .clone()
                    .unwrap_or_else(|| ApplicationError::Faulted("worker lost materials".into()))
                    .into());
            }
            Lifecycle::Failed => {
                return Err(state
                    .error
                    .clone()
                    .unwrap_or(ApplicationError::InstanceGone)
                    .into());
            }
            Lifecycle::Opening | Lifecycle::InitializationBlocked => {
                return Err(ApplicationError::Saving.into());
            }
            Lifecycle::Open => {}
        }
        if let SaveStatus::SaveBlocked { error, .. } = &state.save {
            return Err(ApplicationError::SaveBlocked(error.clone()).into());
        }
        if state.forking > 0 {
            return Err(
                ApplicationError::ForkNotReady("session is currently being forked".into()).into(),
            );
        }
        Ok(())
    }

    async fn try_replay_dedup(
        &self,
        parent_operation_id: Option<&str>,
        source_block_id: &str,
        fingerprint: &str,
        execution_mode: OperationExecutionMode,
    ) -> Result<Option<SpawnAgentOutcome>, CoordinatorError> {
        let key = (
            parent_operation_id.map(str::to_owned),
            source_block_id.to_owned(),
        );
        let existing = {
            let state = self.state.lock().expect("coord lock");
            state
                .dedup_index
                .get(&key)
                .and_then(|op_id| state.operations.get(op_id))
                .map(|op| {
                    (
                        op.dedup_fingerprint.clone(),
                        op.operation_id.clone(),
                        op.to_receipt(true),
                    )
                })
        };
        let Some((existing_fp, op_id, receipt)) = existing else {
            return Ok(None);
        };
        if existing_fp.as_deref() != Some(fingerprint) {
            return Err(CoordinatorError::DedupConflict {
                existing_operation_id: op_id,
            });
        }
        match execution_mode {
            OperationExecutionMode::Background => Ok(Some(SpawnAgentOutcome::Background(receipt))),
            OperationExecutionMode::Sync => {
                let outcome = self.wait_operation_outcome(&op_id).await?;
                Ok(Some(SpawnAgentOutcome::Completed(outcome)))
            }
        }
    }

    async fn handle_replayed_row(
        &self,
        row: StoredOperationRow,
        execution_mode: OperationExecutionMode,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        match execution_mode {
            OperationExecutionMode::Background => {
                Ok(SpawnAgentOutcome::Background(OperationReceipt {
                    agent_id: row.target_agent_id,
                    operation_id: row.operation_id,
                    status: row.status,
                    replayed: true,
                }))
            }
            OperationExecutionMode::Sync => {
                let outcome = self.wait_operation_outcome(&row.operation_id).await?;
                Ok(SpawnAgentOutcome::Completed(outcome))
            }
        }
    }

    async fn wait_operation_outcome(
        &self,
        operation_id: &str,
    ) -> Result<OperationOutcome, CoordinatorError> {
        let notify = {
            let state = self.state.lock().expect("coord lock");
            let op = state
                .operations
                .get(operation_id)
                .ok_or_else(|| CoordinatorError::UnknownOperation(operation_id.to_owned()))?;
            if op.status.is_terminal() {
                return Ok(op.to_outcome());
            }
            op.completion_notify.clone()
        };
        loop {
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.state.lock().expect("coord lock");
                let op = state
                    .operations
                    .get(operation_id)
                    .ok_or_else(|| CoordinatorError::UnknownOperation(operation_id.to_owned()))?;
                if op.status.is_terminal() {
                    return Ok(op.to_outcome());
                }
            }
            notified.await;
        }
    }

    fn spawn_operation_task(
        self: &Arc<Self>,
        operation_id: String,
        prompt: String,
        timeout_secs: u64,
        max_rounds_override: Option<u32>,
        coalesce_key: Option<String>,
        pre_drained: Option<DrainedNotification>,
    ) {
        let coord = self.clone();
        let op_id_for_map = operation_id.clone();
        let handle = tokio::spawn(async move {
            coord
                .run_operation(
                    operation_id,
                    prompt,
                    timeout_secs,
                    max_rounds_override,
                    coalesce_key,
                    pre_drained,
                )
                .await;
        });
        let mut state = self.state.lock().expect("coord lock");
        // Always track already-admitted operations rather than aborting so shutdown settles
        // them cleanly as `Cancelled` in SQLite and in-memory state.
        state.active_tasks.insert(op_id_for_map, handle);
    }

    fn spawn_child_material_save(
        self: &Arc<Self>,
        writer: Arc<crate::store::Writer>,
        agent_id: String,
        expected_material_revision: u64,
        candidate: Context,
        history_index_json: String,
        usage_json: Option<String>,
    ) {
        let coord = self.clone();
        tokio::spawn(async move {
            let context_json = match serde_json::to_string(&candidate) {
                Ok(json) => json,
                Err(_) => {
                    let mut state = coord.state.lock().expect("coord lock");
                    if let Some(unit) = state.agents.get_mut(&agent_id) {
                        unit.saving = false;
                    }
                    state.child_saves_in_flight = state.child_saves_in_flight.saturating_sub(1);
                    coord.state_changed.notify_waiters();
                    return;
                }
            };
            let _writer_guard = coord.writer_gate.lock().await;
            let expected_session_revision = coord
                .entry
                .upgrade()
                .and_then(|e| e.lock().revision)
                .unwrap_or(0);
            let req = OutboxCommitRequest {
                commit_id: uuid::Uuid::now_v7().to_string(),
                expected_session_revision,
                updated_at_ms: crate::time::current_time_ms(),
                agent_update: Some(AgentMaterialUpdate {
                    agent_id: agent_id.clone(),
                    expected_material_revision,
                    context_json,
                    history_index_json,
                    usage_json,
                }),
                operation_update: None,
                new_events: Vec::new(),
                delivered_event_ids: Vec::new(),
                dropped_event_ids: Vec::new(),
            };
            let w = writer.clone();
            let req_clone = req.clone();
            let res = saving::io(&coord.host, move || w.commit_outbox(&req_clone)).await;
            match res {
                Ok(out) => {
                    if let Some(entry) = coord.entry.upgrade() {
                        entry.lock().revision = Some(out.session_revision);
                    }
                    let mut state = coord.state.lock().expect("coord lock");
                    if let Some(unit) = state.agents.get_mut(&agent_id) {
                        if let Some(mat_rev) = out.agent_material_revision {
                            unit.material_revision = mat_rev;
                        }
                        unit.saving = false;
                    }
                    state.child_saves_in_flight = state.child_saves_in_flight.saturating_sub(1);
                }
                Err(ApplicationError::Store(err)) => {
                    let deferred_before_write =
                        matches!(&*err, crate::store::StoreError::PendingCommit);
                    if let Some(entry) = coord.entry.upgrade() {
                        let mut es = entry.lock();
                        es.save = SaveStatus::SaveBlocked {
                            error: err.clone(),
                            commit_id: req.commit_id.clone(),
                            material_version: entry.version(&es),
                        };
                        es.error = Some(ApplicationError::SaveBlocked(err.clone()));
                        entry.changed();
                    }
                    let mut state = coord.state.lock().expect("coord lock");
                    state.save_blocked = Some(err);
                    state.pending_outboxes.push_back(QueuedOutbox {
                        req,
                        deferred_before_write,
                    });
                    if let Some(unit) = state.agents.get_mut(&agent_id) {
                        unit.saving = false;
                    }
                    state.child_saves_in_flight = state.child_saves_in_flight.saturating_sub(1);
                }
                Err(_) => {
                    let mut state = coord.state.lock().expect("coord lock");
                    if let Some(unit) = state.agents.get_mut(&agent_id) {
                        unit.saving = false;
                    }
                    state.child_saves_in_flight = state.child_saves_in_flight.saturating_sub(1);
                }
            }
            coord.wakeup_notify.notify_one();
            coord.state_changed.notify_waiters();
        });
    }

    fn restore_pre_drained(
        &self,
        target_agent_id: &str,
        pre_drained: Option<&DrainedNotification>,
    ) {
        if let Some(drained) = pre_drained {
            let state = self.state.lock().expect("coord lock");
            if let Some(unit) = state.agents.get(target_agent_id) {
                unit.inbox.restore_drained(drained);
            }
        }
    }

    async fn run_operation(
        self: &Arc<Self>,
        operation_id: String,
        prompt: String,
        timeout_secs: u64,
        max_rounds_override: Option<u32>,
        coalesce_key: Option<String>,
        pre_drained: Option<DrainedNotification>,
    ) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };

        // Step A: Pre-run cancellation check while in `creating`.
        let (pre_cancelled, target_agent_id, parent_agent_id, execution_mode, cancel_token) = {
            let state = self.state.lock().expect("coord lock");
            let Some(op) = state.operations.get(&operation_id) else {
                return;
            };
            let parent_cancelled = match &op.parent_operation_id {
                Some(pid) => state.operations.get(pid).is_some_and(|p| {
                    matches!(
                        p.status,
                        OperationStatus::Cancelled
                            | OperationStatus::Failed
                            | OperationStatus::Deadline
                            | OperationStatus::Faulted
                            | OperationStatus::Interrupted
                    ) || (p.status == OperationStatus::Completed
                        && op.on_parent_complete == OnParentComplete::Cancel)
                }),
                None => false,
            };
            let cancelled = op.cancel_token.is_cancelled()
                || self.shutdown.load(Ordering::SeqCst)
                || parent_cancelled;
            (
                cancelled,
                op.target_agent_id.clone(),
                op.parent_agent_id.clone(),
                op.execution_mode,
                op.cancel_token.clone(),
            )
        };

        if pre_cancelled {
            self.restore_pre_drained(&target_agent_id, pre_drained.as_ref());
            self.settle_operation(
                &entry,
                &operation_id,
                &target_agent_id,
                parent_agent_id.as_deref(),
                execution_mode,
                OperationStatus::Cancelled,
                None,
                None,
                coalesce_key,
                None,
                ConsumedInboxEvents::default(),
                true,
            )
            .await;
            return;
        }

        // Step B: Transition `creating -> running`.
        let writer = if self.persistent {
            entry.lock().writer.clone()
        } else {
            None
        };
        if let Some(writer) = writer {
            let _writer_guard = self.writer_gate.lock().await;
            let op_id_clone = operation_id.clone();
            let now_ms = crate::time::current_time_ms();
            match saving::io(&self.host, move || {
                writer.transition_operation_running(&op_id_clone, now_ms)
            })
            .await
            {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    drop(_writer_guard);
                    self.restore_pre_drained(&target_agent_id, pre_drained.as_ref());
                    self.settle_operation(
                        &entry,
                        &operation_id,
                        &target_agent_id,
                        parent_agent_id.as_deref(),
                        execution_mode,
                        OperationStatus::Cancelled,
                        None,
                        None,
                        coalesce_key,
                        None,
                        ConsumedInboxEvents::default(),
                        true,
                    )
                    .await;
                    return;
                }
            }
        }

        let (cancelled_during_transition, parent_op_id) = {
            let mut state = self.state.lock().expect("coord lock");
            let mut parent_op_id = None;
            if let Some(op) = state.operations.get_mut(&operation_id) {
                op.status = OperationStatus::Running;
                parent_op_id = op.parent_operation_id.clone();
            }
            (
                cancel_token.is_cancelled() || self.shutdown.load(Ordering::SeqCst),
                parent_op_id,
            )
        };
        if execution_mode == OperationExecutionMode::Background {
            let _ = self
                .background_tasks
                .transition(&operation_id, OperationStatus::Running);
            self.bus.publish(SessionEvent::BackgroundTaskChanged {
                session_id: self.session_id.clone(),
                task_id: operation_id.clone(),
                kind: BackgroundTaskKind::AgentOperation,
                status: OperationStatus::Running,
            });
        }
        self.bus.publish(SessionEvent::OperationStateChanged {
            session_id: self.session_id.clone(),
            operation_id: operation_id.clone(),
            target_agent_id: target_agent_id.clone(),
            parent_operation_id: parent_op_id,
            status: OperationStatus::Running,
        });

        if cancelled_during_transition {
            self.restore_pre_drained(&target_agent_id, pre_drained.as_ref());
            self.settle_operation(
                &entry,
                &operation_id,
                &target_agent_id,
                parent_agent_id.as_deref(),
                execution_mode,
                OperationStatus::Cancelled,
                None,
                None,
                coalesce_key,
                None,
                ConsumedInboxEvents::default(),
                true,
            )
            .await;
            return;
        }

        // Step C: Lazy assembly for child agent if `owner` is `None`.
        let needs_assembly = {
            let state = self.state.lock().expect("coord lock");
            state
                .agents
                .get(&target_agent_id)
                .is_some_and(|u| u.owner.is_none())
        };
        if needs_assembly
            && let Err(err_msg) = self
                .lazy_assemble_child(&entry, &target_agent_id, &operation_id)
                .await
        {
            self.restore_pre_drained(&target_agent_id, pre_drained.as_ref());
            self.settle_operation(
                &entry,
                &operation_id,
                &target_agent_id,
                parent_agent_id.as_deref(),
                execution_mode,
                OperationStatus::Failed,
                None,
                Some(err_msg),
                coalesce_key,
                None,
                ConsumedInboxEvents::default(),
                false,
            )
            .await;
            return;
        }

        // Step D: Submit work to the agent's `Session` and bridge progress.
        let submit_res: Result<
            (
                crate::session::SessionHandle,
                WorkRef,
                usize,
                ActiveOperationSlotGuard,
            ),
            String,
        > = {
            let mut state = self.state.lock().expect("coord lock");
            let Some(unit) = state.agents.get_mut(&target_agent_id) else {
                return;
            };
            unit.set_approval_origin(&operation_id);
            let Some(handle) = unit.handle.clone() else {
                if let Some(drained) = &pre_drained {
                    unit.inbox.restore_drained(drained);
                }
                return;
            };
            let start_block_len = unit.context.blocks().len();
            let block = if pre_drained.is_none() {
                ContextBlock::new(
                    new_block_id(),
                    BlockContent::text(Role::User, prompt.clone()),
                    BlockMeta::from_source("user"),
                )
            } else {
                ContextBlock::new(
                    new_block_id(),
                    BlockContent::text(Role::User, prompt.clone()),
                    BlockMeta::from_source(TASK_NOTIFICATION_SOURCE),
                )
            };
            let max_rounds =
                max_rounds_override.map(|r| r.min(unit.descriptor.definition.max_model_rounds));
            match handle.submit_with_max_rounds(
                SubmitRequest {
                    request_key: operation_id.clone(),
                    blocks: vec![block],
                },
                max_rounds,
            ) {
                Ok(receipt) => {
                    unit.active_work = Some(receipt.work.clone());
                    *unit.active_op_slot.lock().expect("active op slot lock") =
                        Some(operation_id.clone());
                    let slot_guard = ActiveOperationSlotGuard(unit.active_op_slot.clone());
                    Ok((handle, receipt.work, start_block_len, slot_guard))
                }
                Err(rejection) => {
                    if let Some(drained) = &pre_drained {
                        unit.inbox.restore_drained(drained);
                    }
                    Err(rejection.reason.to_string())
                }
            }
        };
        let (handle, work_ref, start_block_len, _op_slot_guard) = match submit_res {
            Ok(res) => res,
            Err(err_msg) => {
                self.settle_operation(
                    &entry,
                    &operation_id,
                    &target_agent_id,
                    parent_agent_id.as_deref(),
                    execution_mode,
                    OperationStatus::Failed,
                    None,
                    Some(err_msg),
                    coalesce_key,
                    None,
                    ConsumedInboxEvents::default(),
                    false,
                )
                .await;
                return;
            }
        };

        let mut progress_rx = handle.subscribe_progress();
        let bus = self.bus.clone();
        let session_id = self.session_id.clone();
        let agent_id_for_prog = target_agent_id.clone();
        let op_id_for_prog = operation_id.clone();
        let work_for_prog = work_ref.clone();
        let progress_task = tokio::spawn(async move {
            loop {
                match progress_rx.recv().await {
                    Ok(progress) => {
                        if progress.work == work_for_prog {
                            bus.publish(SessionEvent::AgentProgress {
                                session_id: session_id.clone(),
                                agent_id: agent_id_for_prog.clone(),
                                operation_id: op_id_for_prog.clone(),
                                progress: (*progress).clone(),
                            });
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Step E: Wait for work completion, cooperative cancellation, or deadline.
        let timeout = Duration::from_secs(timeout_secs.max(1));
        let deadline_sleep = tokio::time::sleep(timeout);
        tokio::pin!(deadline_sleep);

        let mut timed_out = false;
        let wait_fut = handle.wait(&work_ref, timeout + Duration::from_secs(5));
        tokio::pin!(wait_fut);

        tokio::select! {
            _ = &mut wait_fut => {}
            _ = cancel_token.cancelled() => {
                let _ = handle.cancel(&work_ref);
                let _ = handle.wait_finished(&work_ref).await;
            }
            _ = &mut deadline_sleep => {
                timed_out = true;
                let _ = handle.cancel(&work_ref);
                let _ = handle.wait_finished(&work_ref).await;
            }
        }
        progress_task.abort();

        // Step F: Capture returned materials, result text, and consumed inbox events.
        let observation = handle.observe(&work_ref).ok();
        let (status, result_text, error, updated_materials, total_consumed) = {
            let mut state = self.state.lock().expect("coord lock");
            let Some(unit) = state.agents.get_mut(&target_agent_id) else {
                return;
            };
            let mut consumed = pre_drained
                .as_ref()
                .map_or_else(ConsumedInboxEvents::default, |d| d.consumed.clone());
            let extra = unit.inbox.take_consumed();
            consumed.delivered_ids.extend(extra.delivered_ids);
            consumed.dropped_ids.extend(extra.dropped_ids);

            let owner = unit.owner.as_mut().expect("assembled child owner");
            let captured = owner.with_context(|ctx| {
                (
                    ctx.clone(),
                    extract_turn_assistant_text(ctx, start_block_len),
                )
            });
            let history = owner.history_entries();
            let usage = owner.session_usage();
            let work_res = observation.and_then(|obs| match obs.state {
                WorkState::Finished(res) => Some(res),
                _ => None,
            });
            let (status, mut result_text, error) = match (work_res.as_deref(), timed_out) {
                (Some(WorkResult::Run(TurnResult::Completed { final_output })), _) => {
                    let final_text = final_output.response.text().to_owned();
                    let text = if !final_text.is_empty() {
                        Some(final_text)
                    } else {
                        None
                    };
                    (OperationStatus::Completed, text, None)
                }
                (
                    Some(WorkResult::Run(TurnResult::Interrupted {
                        cause: TurnInterruption::DeadlineExceeded { .. },
                    }))
                    | Some(WorkResult::StoppedBeforeRun(ControlError::TimedOut)),
                    _,
                ) => (
                    OperationStatus::Deadline,
                    None,
                    Some("operation deadline exceeded".to_owned()),
                ),
                (
                    Some(WorkResult::Run(TurnResult::Interrupted {
                        cause: TurnInterruption::Cancelled { .. },
                    }))
                    | Some(WorkResult::StoppedBeforeRun(ControlError::Cancelled)),
                    true,
                ) => (
                    OperationStatus::Deadline,
                    None,
                    Some("operation deadline exceeded".to_owned()),
                ),
                (
                    Some(WorkResult::Run(TurnResult::Interrupted {
                        cause: TurnInterruption::Cancelled { .. },
                    }))
                    | Some(WorkResult::StoppedBeforeRun(ControlError::Cancelled)),
                    false,
                ) => (OperationStatus::Cancelled, None, None),
                (Some(res @ WorkResult::Run(TurnResult::Interrupted { .. })), _) => {
                    let out = crate::application::WorkOutcome::of(res);
                    (OperationStatus::Failed, None, Some(out.summary))
                }
                (Some(WorkResult::AssemblyFailed(err)), _) => {
                    (OperationStatus::Failed, None, Some(err.to_string()))
                }
                (Some(WorkResult::Faulted(fault)), _) => {
                    (OperationStatus::Faulted, None, Some(fault.to_string()))
                }
                (None, true) => (
                    OperationStatus::Deadline,
                    None,
                    Some("operation deadline exceeded".to_owned()),
                ),
                (None, false) => (
                    OperationStatus::Faulted,
                    None,
                    Some("worker finished without observation".to_owned()),
                ),
            };

            let updated_materials = match captured {
                Ok((ctx, fallback_text)) => {
                    if result_text.is_none() && !fallback_text.is_empty() {
                        result_text = Some(fallback_text);
                    }
                    unit.confirmed_work_count = unit.confirmed_work_count.saturating_add(1);
                    Some((ctx, history, usage))
                }
                Err(_) => None,
            };
            // Evict oldest read finished work if retained sheet reached capacity.
            let capacity = unit.descriptor.definition.retained_work_capacity;
            while owner.retained_count() >= capacity {
                let candidate = owner
                    .retained()
                    .into_iter()
                    .find(|w| w.finished && w.read && !w.uncommitted)
                    .map(|w| w.work);
                let Some(candidate) = candidate else {
                    break;
                };
                if owner.forget(&candidate).is_err() {
                    break;
                }
            }
            unit.active_work = None;

            (status, result_text, error, updated_materials, consumed)
        };

        self.settle_operation(
            &entry,
            &operation_id,
            &target_agent_id,
            parent_agent_id.as_deref(),
            execution_mode,
            status,
            result_text,
            error,
            coalesce_key,
            updated_materials,
            total_consumed,
            false,
        )
        .await;
    }

    async fn lazy_assemble_child(
        &self,
        entry: &Arc<Entry>,
        agent_id: &str,
        operation_id: &str,
    ) -> Result<(), String> {
        let (
            definition,
            agent_name,
            context,
            history,
            usage,
            inbox,
            command_rules,
            workspace_rules_backend,
            effective_mode,
            can_delegate,
            active_op_slot,
        ) = {
            let entry_state = entry.lock();
            let entry_mode = entry_state
                .permissions
                .as_ref()
                .map(|p| p.get().0)
                .unwrap_or(PermissionMode::Ask);
            let state = self.state.lock().expect("coord lock");
            let unit = state
                .agents
                .get(agent_id)
                .ok_or_else(|| format!("unknown agent {agent_id}"))?;
            let def = unit.descriptor.definition.clone();
            let mode = if permission_mode_rank(def.initial_permission_mode())
                <= permission_mode_rank(entry_mode)
            {
                def.initial_permission_mode()
            } else {
                entry_mode
            };
            let can_delegate =
                unit.depth < state.config.max_delegation_depth && def.prelude.agents_enabled;
            (
                def,
                unit.descriptor.name.clone(),
                unit.context.clone(),
                unit.history.clone(),
                unit.usage,
                unit.inbox.clone(),
                state.command_rules.clone(),
                state.workspace_rules_backend.clone(),
                mode,
                can_delegate,
                unit.active_op_slot.clone(),
            )
        };

        let mcp = if self.host.options.connect_mcp_servers && !definition.mcp_servers.is_empty() {
            match crate::mcp::McpRuntime::start(&definition).await {
                Ok(mcp) => mcp,
                Err(err) => {
                    let msg = err.to_string();
                    let mut state = self.state.lock().expect("coord lock");
                    if let Some(unit) = state.agents.get_mut(agent_id) {
                        unit.assembly_error = Some(msg.clone());
                    }
                    return Err(msg);
                }
            }
        } else {
            None
        };

        let storage = self.host.store.as_ref().map(|s| s.root().to_path_buf());
        let mut resources = SessionResources::new(self.host.tool_resources.clone())
            .with_agent_id(agent_id.to_owned())
            .with_inbox(inbox)
            .with_approval_origin(ApprovalOrigin::new(agent_id, &agent_name, operation_id))
            .with_shared_approvals(self.approvals.clone())
            .with_storage(storage)
            .with_compaction(history, None, None)
            .with_usage(Some(usage))
            .with_command_rules(command_rules)
            .with_workspace_rules_backend(workspace_rules_backend)
            .persistent(self.persistent);
        if let Some(tools) = self.host.external_tools.clone() {
            resources = resources.with_external_tools(tools);
        }
        if can_delegate {
            let extra = crate::collaboration::collaboration_tools(
                crate::collaboration::CollaborationContext::for_child(
                    self.entry.clone(),
                    agent_id.to_owned(),
                    active_op_slot,
                ),
            );
            resources = resources.with_extra_tools(extra);
        }
        #[cfg(test)]
        {
            let per_agent_gw = self
                .host
                .agent_gateway_override
                .lock()
                .expect("test agent gateway")
                .as_ref()
                .and_then(|f| f(&agent_name, &definition));
            let gw = per_agent_gw.or_else(|| {
                self.host
                    .gateway_override
                    .lock()
                    .expect("test gateway")
                    .clone()
            });
            if let Some(gw) = gw {
                resources = resources.with_test_gateway(gw);
            }
        }

        let conv_id: ConversationId = self.session_id.clone().into();
        match assemble(
            &definition,
            conv_id,
            context,
            resources,
            effective_mode,
            mcp.clone(),
            true,
        ) {
            Ok(assembled) => {
                let mut state = self.state.lock().expect("coord lock");
                if let Some(unit) = state.agents.get_mut(agent_id) {
                    unit.handle = Some(assembled.session.handle());
                    unit.owner = Some(assembled.session);
                    unit.permissions = Some(assembled.permissions);
                    unit.assembly_error = None;
                }
                Ok(())
            }
            Err(err) => {
                if let Some(mcp) = mcp {
                    mcp.close().await;
                }
                let msg = err.to_string();
                let mut state = self.state.lock().expect("coord lock");
                if let Some(unit) = state.agents.get_mut(agent_id) {
                    unit.assembly_error = Some(msg.clone());
                }
                Err(msg)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_operation(
        &self,
        entry: &Arc<Entry>,
        operation_id: &str,
        target_agent_id: &str,
        parent_agent_id: Option<&str>,
        execution_mode: OperationExecutionMode,
        status: OperationStatus,
        result_text: Option<String>,
        error: Option<String>,
        coalesce_key: Option<String>,
        updated_materials: Option<(Context, Vec<HistoryRecordEntry>, SessionUsage)>,
        consumed: ConsumedInboxEvents,
        force_suppress_wake: bool,
    ) {
        let finished_at_ms = crate::time::current_time_ms();

        // Determine wake policy and prepare NewInboxEvent if Background.
        let (new_event, agent_name) = {
            let entry_state = entry.lock();
            let entry_closing = matches!(
                entry_state.lifecycle,
                Lifecycle::Closing | Lifecycle::Closed | Lifecycle::Faulted
            ) || entry_state.closing.is_some();
            drop(entry_state);

            let state = self.state.lock().expect("coord lock");
            let op = state.operations.get(operation_id);
            let op_suppress = op.is_some_and(|o| o.suppress_wake);
            let parent_dead = op
                .and_then(|o| o.parent_operation_id.as_deref())
                .and_then(|pid| state.operations.get(pid))
                .is_some_and(|p| p.status.is_terminal() && p.status != OperationStatus::Completed);
            let agent_name = state
                .agents
                .get(target_agent_id)
                .map_or_else(|| target_agent_id.to_owned(), |u| u.descriptor.name.clone());

            let wake_policy = if force_suppress_wake
                || op_suppress
                || parent_dead
                || entry_closing
                || self.shutdown.load(Ordering::SeqCst)
                || execution_mode == OperationExecutionMode::Sync
            {
                WakePolicy::SuppressWake
            } else {
                WakePolicy::WakeIfIdle
            };

            let new_event = if execution_mode == OperationExecutionMode::Background
                && let Some(parent_id) = parent_agent_id
            {
                let full_text = result_text.as_deref().unwrap_or("");
                let (summary_prefix, truncated) =
                    truncate_utf8_boundary(full_text, DEFAULT_NOTIFICATION_INLINE_MAX_BYTES);
                let payload = AgentNotificationPayload {
                    operation_id: operation_id.to_owned(),
                    agent_id: target_agent_id.to_owned(),
                    agent_name: agent_name.clone(),
                    status,
                    summary_text: summary_prefix.to_owned(),
                    total_bytes: full_text.len(),
                    truncated,
                    error: error.clone(),
                };
                let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
                Some(NewInboxEvent {
                    target_agent_id: parent_id.to_owned(),
                    priority: EventPriority::Normal,
                    source_kind: EventSourceKind::AgentOperation,
                    source_id: operation_id.to_owned(),
                    coalesce_key,
                    wake_policy,
                    payload_json,
                    created_at_ms: finished_at_ms,
                })
            } else {
                None
            };
            (new_event, agent_name)
        };
        let _ = agent_name;

        let outcome_json = serde_json::to_string(&OperationOutcomeJson {
            error: error.clone(),
        })
        .ok();

        let mut inserted_event: Option<InboxEvent> = None;
        let mut new_material_revision: Option<u64> = None;

        let writer_for_outbox = if self.persistent {
            entry.lock().writer.clone()
        } else {
            None
        };
        if let Some(writer) = writer_for_outbox {
            let _writer_guard = self.writer_gate.lock().await;
            let expected_session_revision = entry.lock().revision.unwrap_or(0);
            let expected_material_revision = {
                let state = self.state.lock().expect("coord lock");
                state
                    .agents
                    .get(target_agent_id)
                    .map_or(0, |u| u.material_revision)
            };
            let agent_update = if let Some((ctx, hist, usage)) = &updated_materials {
                let context_json = serde_json::to_string(ctx).unwrap_or_else(|_| "{}".into());
                let history_index_json =
                    serde_json::to_string(hist).unwrap_or_else(|_| "[]".into());
                let wire_usage = crate::store::WireSessionUsage::from_session_usage(usage);
                let usage_json = (!wire_usage.is_empty())
                    .then(|| serde_json::to_string(&wire_usage).ok())
                    .flatten();
                Some(AgentMaterialUpdate {
                    agent_id: target_agent_id.to_owned(),
                    expected_material_revision,
                    context_json,
                    history_index_json,
                    usage_json,
                })
            } else {
                None
            };

            let (delivered_event_ids, dropped_event_ids) = if agent_update.is_some() {
                (consumed.delivered_ids.clone(), consumed.dropped_ids.clone())
            } else {
                (Vec::new(), Vec::new())
            };

            let outbox_req = OutboxCommitRequest {
                commit_id: uuid::Uuid::now_v7().to_string(),
                expected_session_revision,
                updated_at_ms: finished_at_ms,
                agent_update,
                operation_update: Some(OperationTerminalUpdate {
                    operation_id: operation_id.to_owned(),
                    status,
                    outcome_json,
                    result_text: result_text.clone(),
                    finished_at_ms,
                }),
                new_events: new_event.iter().cloned().collect(),
                delivered_event_ids,
                dropped_event_ids,
            };

            let w = writer.clone();
            let req_clone = outbox_req.clone();
            match saving::io(&self.host, move || w.commit_outbox(&req_clone)).await {
                Ok(out) => {
                    entry.lock().revision = Some(out.session_revision);
                    new_material_revision = out.agent_material_revision;
                    if let Some(&event_id) = out.inserted_event_ids.first()
                        && let Some(ev) = &new_event
                    {
                        inserted_event = Some(InboxEvent {
                            event_id,
                            session_id: self.session_id.clone(),
                            target_agent_id: ev.target_agent_id.clone(),
                            priority: ev.priority,
                            source_kind: ev.source_kind,
                            source_id: ev.source_id.clone(),
                            coalesce_key: ev.coalesce_key.clone(),
                            wake_policy: ev.wake_policy,
                            payload_json: ev.payload_json.clone(),
                            state: EventDeliveryState::Pending,
                            claimed_op_id: None,
                            created_at_ms: ev.created_at_ms,
                            delivered_at_ms: None,
                        });
                    }
                }
                Err(ApplicationError::Store(store_err)) => {
                    // AC11: Enter SaveBlocked WITHOUT killing other running child agents!
                    let deferred_before_write =
                        matches!(&*store_err, crate::store::StoreError::PendingCommit);
                    {
                        let mut es = entry.lock();
                        es.save = SaveStatus::SaveBlocked {
                            error: store_err.clone(),
                            commit_id: outbox_req.commit_id.clone(),
                            material_version: entry.version(&es),
                        };
                        es.error = Some(ApplicationError::SaveBlocked(store_err.clone()));
                        entry.changed();
                    }
                    let mut state = self.state.lock().expect("coord lock");
                    state.save_blocked = Some(store_err);
                    state.pending_outboxes.push_back(QueuedOutbox {
                        req: outbox_req,
                        deferred_before_write,
                    });
                }
                Err(_) => {}
            }
        } else if let Some(ev) = &new_event {
            // Transient session: synthesize a positive local event_id.
            let next_id = i64::try_from(finished_at_ms.max(1)).unwrap_or(1);
            inserted_event = Some(InboxEvent {
                event_id: next_id,
                session_id: self.session_id.clone(),
                target_agent_id: ev.target_agent_id.clone(),
                priority: ev.priority,
                source_kind: ev.source_kind,
                source_id: ev.source_id.clone(),
                coalesce_key: ev.coalesce_key.clone(),
                wake_policy: ev.wake_policy,
                payload_json: ev.payload_json.clone(),
                state: EventDeliveryState::Pending,
                claimed_op_id: None,
                created_at_ms: ev.created_at_ms,
                delivered_at_ms: None,
            });
        }

        // Update in-memory agent & operation state, push parent inbox event, and collect child cascade.
        let (mut cascade_tokens, mut cascade_suppress_ids, op_notify, parent_op_id) = {
            let mut state = self.state.lock().expect("coord lock");
            if let Some(unit) = state.agents.get_mut(target_agent_id) {
                let has_updated_materials = updated_materials.is_some();
                if let Some((ctx, hist, usage)) = updated_materials {
                    unit.context = ctx;
                    unit.history = hist;
                    unit.usage = usage;
                    unit.generation = unit.generation.saturating_add(1);
                }
                if let Some(mat_rev) = new_material_revision {
                    unit.material_revision = mat_rev;
                }
                if unit.active_operation_id.as_deref() == Some(operation_id) {
                    unit.active_operation_id = None;
                }
                unit.active_work = None;
                unit.saving = false;
                if has_updated_materials {
                    remove_consumed_from_pending(&mut unit.pending_events, &consumed);
                }
            }

            let mut op_notify = None;
            let mut parent_op_id = None;
            if let Some(op) = state.operations.get_mut(operation_id) {
                if !op.status.is_terminal() {
                    op.status = status;
                    op.result_text = result_text;
                    op.error = error;
                    op.finished_at_ms = Some(finished_at_ms);
                }
                op_notify = Some(op.completion_notify.clone());
                parent_op_id = op.parent_operation_id.clone();
            }

            // Re-check suppression after awaiting commit_outbox in case cancel_all_active,
            // cancel_agent, or session shutdown raced with the outbox write.
            let should_suppress_wake = force_suppress_wake
                || self.shutdown.load(Ordering::SeqCst)
                || state.operations.get(operation_id).is_some_and(|op| {
                    op.suppress_wake
                        || op
                            .parent_operation_id
                            .as_deref()
                            .and_then(|pid| state.operations.get(pid))
                            .is_some_and(|p| {
                                p.suppress_wake
                                    || (p.status.is_terminal()
                                        && p.status != OperationStatus::Completed)
                            })
                });
            let mut late_suppressed_source = None;
            if let Some(inbox_ev) = inserted_event.as_mut() {
                if should_suppress_wake && inbox_ev.wake_policy == WakePolicy::WakeIfIdle {
                    inbox_ev.wake_policy = WakePolicy::SuppressWake;
                    late_suppressed_source = Some(operation_id.to_owned());
                }
                if let Some(parent_unit) = state.agents.get_mut(&inbox_ev.target_agent_id) {
                    parent_unit.inbox.enqueue(inbox_ev.clone());
                    parent_unit.pending_events.push(inbox_ev.clone());
                }
            }

            // Child cascade (AC07).
            let (cascade_tokens, mut cascade_suppress_ids) = if status == OperationStatus::Completed
            {
                let state_ref = &mut *state;
                collect_normal_completion_cascade(
                    &mut state_ref.operations,
                    &mut state_ref.agents,
                    operation_id,
                )
            } else {
                let subtree = collect_operation_subtree(&state.operations, operation_id);
                let mut tokens = Vec::new();
                for id in &subtree {
                    if id == operation_id {
                        continue;
                    }
                    if let Some(child_op) = state.operations.get_mut(id) {
                        child_op.suppress_wake = true;
                        if child_op.status.is_active() {
                            tokens.push(child_op.cancel_token.clone());
                        }
                    }
                }
                suppress_in_memory_events_for_sources(&mut state.agents, &subtree);
                (tokens, subtree)
            };
            if let Some(source_id) = late_suppressed_source
                && !cascade_suppress_ids.contains(&source_id)
            {
                cascade_suppress_ids.push(source_id);
            }

            state.active_tasks.remove(operation_id);
            (
                cascade_tokens,
                cascade_suppress_ids,
                op_notify,
                parent_op_id,
            )
        };
        let _ = (&mut cascade_tokens, &mut cascade_suppress_ids);

        if execution_mode == OperationExecutionMode::Background {
            let _ = self.background_tasks.transition(operation_id, status);
            self.bus.publish(SessionEvent::BackgroundTaskChanged {
                session_id: self.session_id.clone(),
                task_id: operation_id.to_owned(),
                kind: BackgroundTaskKind::AgentOperation,
                status,
            });
        }
        self.bus.publish(SessionEvent::OperationStateChanged {
            session_id: self.session_id.clone(),
            operation_id: operation_id.to_owned(),
            target_agent_id: target_agent_id.to_owned(),
            parent_operation_id: parent_op_id,
            status,
        });
        if let Some(ev) = inserted_event {
            self.bus.publish(SessionEvent::InboxEventChanged {
                session_id: self.session_id.clone(),
                target_agent_id: ev.target_agent_id,
                event_id: ev.event_id,
                state: ev.state,
                wake_policy: ev.wake_policy,
            });
        }

        for token in cascade_tokens {
            token.cancel();
        }
        self.suppress_events_sync_best_effort(&cascade_suppress_ids);

        if let Some(notify) = op_notify {
            notify.notify_waiters();
        }
        self.state_changed.notify_waiters();
        self.wakeup_notify.notify_one();
    }

    fn suppress_events_sync_best_effort(&self, source_ids: &[String]) {
        if !self.persistent || source_ids.is_empty() {
            return;
        }
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let Some(writer) = entry.lock().writer.clone() else {
            return;
        };
        // Non-blocking best-effort SQLite wake suppression tracked by utility_tasks so
        // `wait_all_tasks_settled` awaits completion before releasing the writer OS lock.
        let ids = source_ids.to_vec();
        let gate = self.writer_gate.clone();
        let host = self.host.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let task = handle.spawn(async move {
                let _g = gate.lock().await;
                let _ = saving::io(&host, move || {
                    for id in &ids {
                        let _ = writer.suppress_pending_wakeups_for_source(
                            EventSourceKind::AgentOperation,
                            id,
                        );
                    }
                    Ok(())
                })
                .await;
            });
            let mut state = self.state.lock().expect("coord lock");
            state.utility_tasks.retain(|h| !h.is_finished());
            state.utility_tasks.push(task);
        }
    }

    // -----------------------------------------------------------------------
    // Plane 1: 7-Guard Idle Reactive Wakeup Scheduler + 0ms / 50ms Debounce
    // -----------------------------------------------------------------------

    async fn run_scheduler(self: Arc<Self>) {
        loop {
            let notified = self.wakeup_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }

            let candidate = self.find_wakeup_candidate().await;
            let Some(target_agent_id) = candidate else {
                notified.await;
                continue;
            };

            // Sibling debounce: 0 ms if no active finite sibling tasks exist; otherwise up to sibling_debounce (50 ms).
            let debounce = self
                .state
                .lock()
                .expect("coord lock")
                .config
                .sibling_debounce;
            if self
                .background_tasks
                .active_sibling_count(&target_agent_id, None)
                > 0
                && !debounce.is_zero()
            {
                let deadline = tokio::time::Instant::now() + debounce;
                loop {
                    if self.shutdown.load(Ordering::SeqCst)
                        || self
                            .background_tasks
                            .active_sibling_count(&target_agent_id, None)
                            == 0
                    {
                        break;
                    }
                    let woken = self.wakeup_notify.notified();
                    tokio::pin!(woken);
                    woken.as_mut().enable();
                    if self.shutdown.load(Ordering::SeqCst)
                        || self
                            .background_tasks
                            .active_sibling_count(&target_agent_id, None)
                            == 0
                    {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        _ = woken => {}
                    }
                }
            }

            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }

            // Re-evaluate all 7 guards after debounce, then submit reactive turn atomically!
            self.try_trigger_wakeup(&target_agent_id).await;
        }
    }

    async fn find_wakeup_candidate(&self) -> Option<String> {
        let entry = self.entry.upgrade()?;
        let overflowed = {
            let mut entry_state = entry.lock();
            if entry_state.lifecycle != Lifecycle::Open
                || entry_state.closing.is_some()
                || matches!(entry_state.save, SaveStatus::SaveBlocked { .. })
                || entry_state.fault.is_some()
            {
                return None;
            }

            let state = self.state.lock().expect("coord lock");
            if state.save_blocked.is_some() {
                return None;
            }
            let max_auto = state.config.max_auto_wakeups;
            let mut chosen = None;
            let mut overflow_agent = None;

            for agent_id in &state.agent_order {
                let Some(unit) = state.agents.get(agent_id) else {
                    continue;
                };
                if !unit
                    .pending_events
                    .iter()
                    .any(|e| e.wake_policy == WakePolicy::WakeIfIdle)
                {
                    continue;
                }
                if !passes_agent_guards(
                    agent_id,
                    &self.entry_agent_id,
                    unit,
                    &mut entry_state,
                    Some(&entry.media_vault),
                ) {
                    continue;
                }
                if unit.auto_wakeups_in_cycle >= max_auto {
                    overflow_agent = Some((agent_id.clone(), max_auto));
                    break;
                }
                chosen = Some(agent_id.clone());
                break;
            }
            match (chosen, overflow_agent) {
                (Some(id), _) => return Some(id),
                (None, Some((id, max))) => Some((id, max)),
                (None, None) => return None,
            }
        };

        if let Some((agent_id, max_auto)) = overflowed {
            self.downgrade_overflow_wakeups(&entry, &agent_id, max_auto)
                .await;
        }
        None
    }

    async fn downgrade_overflow_wakeups(
        &self,
        entry: &Arc<Entry>,
        agent_id: &str,
        max_auto_wakeups: usize,
    ) {
        let mut downgraded_ids = Vec::new();
        let writer_for_outbox = if self.persistent {
            entry.lock().writer.clone()
        } else {
            None
        };
        if let Some(writer) = writer_for_outbox {
            let _g = self.writer_gate.lock().await;
            let aid = agent_id.to_owned();
            if let Ok(ids) =
                saving::io(&self.host, move || writer.downgrade_agent_wakeups(&aid)).await
            {
                downgraded_ids = ids;
            }
        }
        {
            let mut state = self.state.lock().expect("coord lock");
            if let Some(unit) = state.agents.get_mut(agent_id) {
                for ev in &mut unit.pending_events {
                    if ev.wake_policy == WakePolicy::WakeIfIdle {
                        ev.wake_policy = WakePolicy::DeliverOnNextTurn;
                        if !self.persistent {
                            downgraded_ids.push(ev.event_id);
                        }
                    }
                }
            }
        }
        for event_id in downgraded_ids {
            self.bus.publish(SessionEvent::InboxEventChanged {
                session_id: self.session_id.clone(),
                target_agent_id: agent_id.to_owned(),
                event_id,
                state: EventDeliveryState::Pending,
                wake_policy: WakePolicy::DeliverOnNextTurn,
            });
        }
        self.bus.publish(SessionEvent::AutoWakeupLimitReached {
            session_id: self.session_id.clone(),
            agent_id: agent_id.to_owned(),
            max_auto_wakeups,
        });
        self.state_changed.notify_waiters();
    }

    async fn try_trigger_wakeup(self: &Arc<Self>, target_agent_id: &str) {
        let Some(entry) = self.entry.upgrade() else {
            return;
        };

        let mut overflow_max = None;
        let mut submitted_root_op_id: Option<String> = None;
        let child_reactive_work: Option<(
            String,
            DrainedNotification,
            u64,
            u32,
            CancellationToken,
        )> = {
            let mut entry_state = entry.lock();
            if entry_state.lifecycle != Lifecycle::Open
                || entry_state.closing.is_some()
                || entry_state.forking > 0
                || matches!(entry_state.save, SaveStatus::SaveBlocked { .. })
                || entry_state.fault.is_some()
            {
                return;
            }

            let mut state = self.state.lock().expect("coord lock");
            if state.save_blocked.is_some() {
                return;
            }
            let max_auto = state.config.max_auto_wakeups;
            let timeout_secs = state.config.default_operation_timeout_secs;
            let is_overflow = {
                let Some(unit) = state.agents.get(target_agent_id) else {
                    return;
                };
                if !unit
                    .pending_events
                    .iter()
                    .any(|e| e.wake_policy == WakePolicy::WakeIfIdle)
                {
                    return;
                }
                if !passes_agent_guards(
                    target_agent_id,
                    &self.entry_agent_id,
                    unit,
                    &mut entry_state,
                    Some(&entry.media_vault),
                ) {
                    return;
                }
                unit.auto_wakeups_in_cycle >= max_auto
            };
            if is_overflow {
                overflow_max = Some(max_auto);
                None
            } else if target_agent_id == self.entry_agent_id {
                let Some(handle) = entry_state.handle.clone() else {
                    return;
                };
                let capacity = entry_state
                    .definition
                    .as_ref()
                    .map(|d| d.retained_work_capacity);
                if let (Some(capacity), Some(owner)) = (capacity, entry_state.owner.as_mut()) {
                    while owner.retained_count() >= capacity {
                        if let Some(candidate) = owner
                            .retained()
                            .into_iter()
                            .find(|w| w.finished && w.read && !w.uncommitted)
                            .map(|w| w.work)
                        {
                            if owner.forget(&candidate).is_err() {
                                break;
                            }
                            state.auto_wakeup_turns.remove(&candidate.turn_id);
                            continue;
                        }
                        if let Some(wakeup_candidate) = owner
                            .retained()
                            .into_iter()
                            .find(|w| {
                                w.finished
                                    && !w.uncommitted
                                    && state.auto_wakeup_turns.contains(&w.work.turn_id)
                            })
                            .map(|w| w.work)
                        {
                            let _ = handle.observe(&wakeup_candidate);
                            if owner.forget(&wakeup_candidate).is_err() {
                                break;
                            }
                            state.auto_wakeup_turns.remove(&wakeup_candidate.turn_id);
                            continue;
                        }
                        break;
                    }
                }
                let drained = {
                    let Some(unit) = state.agents.get_mut(target_agent_id) else {
                        return;
                    };
                    let Some(drained) = unit.inbox.drain_notification_detailed() else {
                        return;
                    };
                    unit.auto_wakeups_in_cycle += 1;
                    drained
                };
                let block = ContextBlock::new(
                    new_block_id(),
                    BlockContent::text(Role::User, drained.rendered.clone()),
                    BlockMeta::from_source(TASK_NOTIFICATION_SOURCE),
                );
                match handle.submit(SubmitRequest {
                    request_key: uuid::Uuid::now_v7().to_string(),
                    blocks: vec![block],
                }) {
                    Ok(receipt) => {
                        submitted_root_op_id = Some(receipt.work.turn_id.0.clone());
                        if let Some(unit) = state.agents.get_mut(target_agent_id) {
                            unit.active_work = Some(receipt.work.clone());
                        }
                        state.active_root_turn = Some(receipt.work.turn_id.clone());
                        state.auto_wakeup_turns.insert(receipt.work.turn_id.clone());
                        entry_state.active = Some(receipt.work.clone());
                        entry_state.root_work_seq = entry_state.root_work_seq.saturating_add(1);
                        entry_state.last_root_work = Some(receipt.work);
                        if entry_state.persistent {
                            entry_state.save = SaveStatus::Dirty {
                                base_revision: entry_state.revision,
                                material_version: entry.version(&entry_state),
                            };
                        }
                        entry.notify();
                    }
                    Err(_) => {
                        // Rollback counter and restore mailbox so failed wakeup never loses events.
                        if let Some(unit) = state.agents.get_mut(target_agent_id) {
                            unit.auto_wakeups_in_cycle =
                                unit.auto_wakeups_in_cycle.saturating_sub(1);
                            unit.inbox.restore_drained(&drained);
                        }
                    }
                }
                None
            } else {
                let Some(unit) = state.agents.get_mut(target_agent_id) else {
                    return;
                };
                let Some(drained) = unit.inbox.drain_notification_detailed() else {
                    return;
                };
                let _ = unit.inbox.take_consumed();
                unit.auto_wakeups_in_cycle += 1;
                let op_id = uuid::Uuid::now_v7().to_string();
                unit.active_operation_id = Some(op_id.clone());
                let timeout = unit
                    .descriptor
                    .definition
                    .work_timeout_secs
                    .unwrap_or(timeout_secs);
                let depth = unit.depth;
                let cancel_token = CancellationToken::new();
                state.operations.insert(
                    op_id.clone(),
                    OperationRecord {
                        operation_id: op_id.clone(),
                        target_agent_id: target_agent_id.to_owned(),
                        parent_operation_id: None,
                        parent_agent_id: None,
                        root_turn_id: None,
                        dedup_fingerprint: None,
                        execution_mode: OperationExecutionMode::Background,
                        on_parent_complete: OnParentComplete::Continue,
                        depth,
                        status: OperationStatus::Creating,
                        result_text: None,
                        error: None,
                        finished_at_ms: None,
                        cancel_token: cancel_token.clone(),
                        suppress_wake: false,
                        completion_notify: Arc::new(Notify::new()),
                    },
                );
                Some((op_id, drained, timeout, depth, cancel_token))
            }
        };

        if let Some(operation_id) = submitted_root_op_id {
            self.bus.publish(SessionEvent::OperationStateChanged {
                session_id: self.session_id.clone(),
                operation_id,
                target_agent_id: self.entry_agent_id.clone(),
                parent_operation_id: None,
                status: OperationStatus::Running,
            });
            self.state_changed.notify_waiters();
        }

        if let Some(max_auto) = overflow_max {
            self.downgrade_overflow_wakeups(&entry, target_agent_id, max_auto)
                .await;
            return;
        }

        if let Some((op_id, drained, timeout, depth, cancel_token)) = child_reactive_work {
            let prompt = drained.rendered.clone();
            let writer = if self.persistent {
                entry.lock().writer.clone()
            } else {
                None
            };
            if let Some(writer) = writer {
                let _writer_guard = self.writer_gate.lock().await;
                let aborted = self.check_open_and_unblocked(&entry).is_err()
                    || cancel_token.is_cancelled()
                    || self.shutdown.load(Ordering::SeqCst);
                if aborted {
                    let mut state = self.state.lock().expect("coord lock");
                    rollback_child_reactive_wakeup(&mut state, &op_id, target_agent_id, &drained);
                    drop(state);
                    self.state_changed.notify_waiters();
                    return;
                }
                let input_json = serde_json::to_string(&serde_json::json!({
                    "kind": "reactive_wakeup",
                    "prompt": &prompt,
                }))
                .unwrap_or_else(|_| "{}".into());
                let admit_req = AdmitOperationRequest {
                    commit_id: uuid::Uuid::now_v7().to_string(),
                    operation_id: op_id.clone(),
                    target_agent_id: target_agent_id.to_owned(),
                    parent_operation_id: None,
                    parent_agent_id: None,
                    source_block_id: None,
                    dedup_fingerprint: None,
                    execution_mode: OperationExecutionMode::Background,
                    on_parent_complete: OnParentComplete::Continue,
                    depth,
                    input_json,
                    created_at_ms: crate::time::current_time_ms(),
                    new_agent: None,
                };
                let w = writer.clone();
                let req_clone = admit_req.clone();
                match saving::io(&self.host, move || w.admit_operation(&req_clone)).await {
                    Ok(AdmitOperationOutcome::Admitted {
                        session_revision, ..
                    }) => {
                        entry.lock().revision = Some(session_revision);
                    }
                    Ok(AdmitOperationOutcome::Replayed(_))
                    | Ok(AdmitOperationOutcome::DedupConflict { .. }) => {
                        let mut state = self.state.lock().expect("coord lock");
                        rollback_child_reactive_wakeup(
                            &mut state,
                            &op_id,
                            target_agent_id,
                            &drained,
                        );
                        drop(state);
                        self.state_changed.notify_waiters();
                        return;
                    }
                    Err(ApplicationError::Store(store_err)) => {
                        self.handle_child_reactive_admit_failure(
                            &entry, &writer, store_err, admit_req, prompt, timeout, drained,
                        );
                        return;
                    }
                    Err(other) => {
                        let store_err =
                            Arc::new(crate::store::StoreError::InvalidDatabase(other.to_string()));
                        self.handle_child_reactive_admit_failure(
                            &entry, &writer, store_err, admit_req, prompt, timeout, drained,
                        );
                        return;
                    }
                }
            }
            self.bus.publish(SessionEvent::OperationStateChanged {
                session_id: self.session_id.clone(),
                operation_id: op_id.clone(),
                target_agent_id: target_agent_id.to_owned(),
                parent_operation_id: None,
                status: OperationStatus::Creating,
            });
            self.spawn_operation_task(op_id, prompt, timeout, None, None, Some(drained));
            self.state_changed.notify_waiters();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_child_reactive_admit_failure(
        &self,
        entry: &Arc<Entry>,
        writer: &crate::store::Writer,
        store_err: Arc<crate::store::StoreError>,
        admit_req: AdmitOperationRequest,
        prompt: String,
        timeout_secs: u64,
        drained: DrainedNotification,
    ) {
        let is_pending_commit = matches!(&*store_err, crate::store::StoreError::PendingCommit);
        let retain_pending_admit =
            !is_pending_commit && writer.pending_kind() == crate::store::PendingWriterKind::Admit;
        // Strictly maintain Entry::state -> Coordinator::state lock ordering.
        {
            let mut es = entry.lock();
            if !is_pending_commit || !matches!(es.save, SaveStatus::SaveBlocked { .. }) {
                es.save = SaveStatus::SaveBlocked {
                    error: store_err.clone(),
                    commit_id: admit_req.commit_id.clone(),
                    material_version: entry.version(&es),
                };
                es.error = Some(ApplicationError::SaveBlocked(store_err.clone()));
                entry.changed();
            }
            let mut state = self.state.lock().expect("coord lock");
            if state.save_blocked.is_none() || !is_pending_commit {
                state.save_blocked = Some(store_err);
            }
            if retain_pending_admit {
                state.pending_admits.push_back(PendingReactiveAdmit {
                    admit_req,
                    prompt,
                    timeout_secs,
                    drained,
                });
            } else {
                let op_id = admit_req.operation_id;
                let target_agent_id = admit_req.target_agent_id;
                rollback_child_reactive_wakeup(&mut state, &op_id, &target_agent_id, &drained);
            }
        }
        self.state_changed.notify_waiters();
    }
}

fn rollback_child_reactive_wakeup(
    state: &mut CoordinatorState,
    op_id: &str,
    target_agent_id: &str,
    drained: &DrainedNotification,
) {
    state.operations.remove(op_id);
    if let Some(unit) = state.agents.get_mut(target_agent_id) {
        if unit.active_operation_id.as_deref() == Some(op_id) {
            unit.active_operation_id = None;
        }
        unit.auto_wakeups_in_cycle = unit.auto_wakeups_in_cycle.saturating_sub(1);
        unit.inbox.restore_drained(drained);
    }
}

fn work_result_to_terminal_status(result: &WorkResult, timed_out: bool) -> OperationStatus {
    match (result, timed_out) {
        (WorkResult::Run(TurnResult::Completed { .. }), _) => OperationStatus::Completed,
        (
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::DeadlineExceeded { .. },
            })
            | WorkResult::StoppedBeforeRun(ControlError::TimedOut),
            _,
        ) => OperationStatus::Deadline,
        (
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. },
            })
            | WorkResult::StoppedBeforeRun(ControlError::Cancelled),
            true,
        ) => OperationStatus::Deadline,
        (
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. },
            })
            | WorkResult::StoppedBeforeRun(ControlError::Cancelled),
            false,
        ) => OperationStatus::Cancelled,
        (WorkResult::Run(TurnResult::Interrupted { .. }), _)
        | (WorkResult::AssemblyFailed(_), _) => OperationStatus::Failed,
        (WorkResult::Faulted(_), _) => OperationStatus::Faulted,
    }
}

type InboxDrainHandleSnapshot = crate::events::InboxDrainHandle;

struct EntryLiveSummary {
    generation: u64,
    material_revision: Option<u64>,
    active_operation_id: Option<String>,
    usage: Option<SessionUsage>,
}

impl EntryLiveSummary {
    fn from_state(state: &crate::application::entry::State) -> Self {
        Self {
            generation: state.generation,
            material_revision: state.revision,
            active_operation_id: state.active.as_ref().map(|w| w.turn_id.0.clone()),
            usage: state.owner.as_ref().map(|o| o.session_usage()),
        }
    }

    fn apply_to_snapshot(&self, snap: &mut AgentSnapshot) {
        snap.generation = self.generation;
        if let Some(rev) = self.material_revision {
            snap.material_revision = rev;
        }
        if let Some(active_id) = &self.active_operation_id {
            snap.active_operation_id = Some(active_id.clone());
        }
        if let Some(usage) = &self.usage {
            snap.usage = *usage;
        }
    }
}

fn passes_agent_guards(
    agent_id: &str,
    entry_agent_id: &str,
    unit: &AgentUnit,
    entry_state: &mut crate::application::entry::State,
    entry_media_vault: Option<&crate::media::MediaVault>,
) -> bool {
    if !unit.is_idle_for_wakeup() || unit.assembly_error.is_some() {
        return false;
    }
    if agent_id == entry_agent_id {
        if entry_state.active.is_some()
            || entry_state.pending.is_some()
            || entry_state.save_requested
            || entry_state.compacting
            || entry_state.forking > 0
        {
            return false;
        }
        let Some(owner) = entry_state.owner.as_mut() else {
            return false;
        };
        let history = owner.history_entries();
        let artifacts = owner.compaction().and_then(|c| c.history());
        let ready = owner
            .with_context(|ctx| {
                repair::admission_check_with_media(
                    ctx,
                    &history,
                    artifacts.as_deref(),
                    entry_media_vault,
                )
            })
            .is_ok_and(|res| res.is_ok());
        return ready;
    }
    !unit.needs_repair()
}

fn remove_consumed_from_pending(pending: &mut Vec<InboxEvent>, consumed: &ConsumedInboxEvents) {
    if consumed.delivered_ids.is_empty() && consumed.dropped_ids.is_empty() {
        return;
    }
    pending.retain(|ev| {
        !consumed.delivered_ids.contains(&ev.event_id)
            && !consumed.dropped_ids.contains(&ev.event_id)
    });
}

fn collect_root_turn_cascade(
    state: &mut CoordinatorState,
    turn_id: &TurnId,
    parent_completed_normally: bool,
) -> (Vec<CancellationToken>, Vec<String>, Vec<String>) {
    let direct_children: Vec<String> = state
        .operations
        .values()
        .filter(|op| op.root_turn_id.as_ref() == Some(turn_id) && op.parent_operation_id.is_none())
        .map(|op| op.operation_id.clone())
        .collect();

    if direct_children.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let mut tokens = Vec::new();
    let mut suppress_ids = Vec::new();
    let mut cancelled_ops = Vec::new();

    if parent_completed_normally {
        for child_id in direct_children {
            let should_cancel = state.operations.get(&child_id).is_some_and(|op| {
                op.status.is_active() && op.on_parent_complete == OnParentComplete::Cancel
            });
            if should_cancel {
                let subtree = collect_operation_subtree(&state.operations, &child_id);
                for id in subtree {
                    if let Some(op) = state.operations.get_mut(&id) {
                        op.suppress_wake = true;
                        if op.status.is_active() {
                            tokens.push(op.cancel_token.clone());
                            cancelled_ops.push(id.clone());
                        }
                    }
                    suppress_ids.push(id);
                }
            }
        }
    } else {
        for child_id in direct_children {
            let subtree = collect_operation_subtree(&state.operations, &child_id);
            for id in subtree {
                if let Some(op) = state.operations.get_mut(&id) {
                    op.suppress_wake = true;
                    if op.status.is_active() {
                        tokens.push(op.cancel_token.clone());
                        cancelled_ops.push(id.clone());
                    }
                }
                suppress_ids.push(id);
            }
        }
    }

    suppress_in_memory_events_for_sources(&mut state.agents, &suppress_ids);
    (tokens, suppress_ids, cancelled_ops)
}

fn collect_normal_completion_cascade(
    operations: &mut HashMap<String, OperationRecord>,
    agents: &mut HashMap<String, AgentUnit>,
    parent_operation_id: &str,
) -> (Vec<CancellationToken>, Vec<String>) {
    let direct_cancel_children: Vec<String> = operations
        .values()
        .filter(|op| {
            op.parent_operation_id.as_deref() == Some(parent_operation_id)
                && op.status.is_active()
                && op.on_parent_complete == OnParentComplete::Cancel
        })
        .map(|op| op.operation_id.clone())
        .collect();

    if direct_cancel_children.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut tokens = Vec::new();
    let mut suppress_ids = Vec::new();
    for child_id in direct_cancel_children {
        let subtree = collect_operation_subtree(operations, &child_id);
        for id in subtree {
            if let Some(op) = operations.get_mut(&id) {
                op.suppress_wake = true;
                if op.status.is_active() {
                    tokens.push(op.cancel_token.clone());
                }
            }
            suppress_ids.push(id);
        }
    }
    suppress_in_memory_events_for_sources(agents, &suppress_ids);
    (tokens, suppress_ids)
}

fn collect_operation_subtree(
    operations: &HashMap<String, OperationRecord>,
    root_op_id: &str,
) -> Vec<String> {
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for op in operations.values() {
        if let Some(parent) = op.parent_operation_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(op.operation_id.as_str());
        }
    }

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut result = Vec::new();
    if operations.contains_key(root_op_id) {
        visited.insert(root_op_id.to_owned());
        queue.push_back(root_op_id.to_owned());
        result.push(root_op_id.to_owned());
    }
    while let Some(current) = queue.pop_front() {
        let Some(children) = children_by_parent.get(current.as_str()) else {
            continue;
        };
        for &child in children {
            if visited.insert(child.to_owned()) {
                result.push(child.to_owned());
                queue.push_back(child.to_owned());
            }
        }
    }
    result
}

fn suppress_in_memory_events_for_sources(
    agents: &mut HashMap<String, AgentUnit>,
    source_ids: &[String],
) {
    if source_ids.is_empty() {
        return;
    }
    for unit in agents.values_mut() {
        for ev in &mut unit.pending_events {
            if ev.source_kind == EventSourceKind::AgentOperation
                && source_ids.iter().any(|id| id == &ev.source_id)
            {
                ev.wake_policy = WakePolicy::SuppressWake;
            }
        }
    }
}

fn has_ancestor_agent<'a>(
    operations: &'a HashMap<String, OperationRecord>,
    mut current_op_id: Option<&'a str>,
    target_agent_id: &str,
) -> bool {
    let mut visited = HashSet::new();
    while let Some(op_id) = current_op_id {
        if !visited.insert(op_id) {
            break;
        }
        let Some(op) = operations.get(op_id) else {
            break;
        };
        if op.target_agent_id == target_agent_id {
            return true;
        }
        current_op_id = op.parent_operation_id.as_deref();
    }
    false
}

fn check_concurrency_limits(
    state: &CoordinatorState,
    caller_agent_id: &str,
    entry_agent_id: &str,
) -> Result<(), CoordinatorError> {
    let caller_active = state
        .operations
        .values()
        .filter(|op| {
            op.status.is_active() && op.parent_agent_id.as_deref() == Some(caller_agent_id)
        })
        .count();
    if caller_active >= state.config.max_active_ops_per_agent {
        return Err(CoordinatorError::AgentConcurrencyLimit {
            limit: state.config.max_active_ops_per_agent,
        });
    }
    let session_child_active = state
        .operations
        .values()
        .filter(|op| op.status.is_active() && op.target_agent_id != entry_agent_id)
        .count();
    if session_child_active >= state.config.max_active_child_ops_per_session {
        return Err(CoordinatorError::SessionConcurrencyLimit {
            limit: state.config.max_active_child_ops_per_session,
        });
    }
    Ok(())
}

fn clean_lexical_absolute(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => return None,
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                cleaned.push(component.as_os_str());
            }
        }
    }
    Some(cleaned)
}

fn verify_capability_coverage(
    caller: &SessionDefinition,
    caller_live_mode: PermissionMode,
    target: &SessionDefinition,
    target_live_mode: PermissionMode,
    requested_timeout_secs: Option<u64>,
    requested_max_rounds: Option<u32>,
) -> Result<(), CoordinatorError> {
    // 1. Workspace root
    if let Some(target_ws) = &target.workspace_root {
        let Some(caller_ws) = &caller.workspace_root else {
            return Err(CoordinatorError::CapabilityMismatch(
                "target requires workspace_root absent from caller".into(),
            ));
        };
        let (Some(cw), Some(tw)) = (
            clean_lexical_absolute(caller_ws),
            clean_lexical_absolute(target_ws),
        ) else {
            return Err(CoordinatorError::CapabilityMismatch(
                "invalid workspace_root".into(),
            ));
        };
        if !tw.starts_with(&cw) {
            return Err(CoordinatorError::CapabilityMismatch(
                "target workspace_root exceeds caller workspace_root".into(),
            ));
        }
    }

    // 2. Tools subset
    for tool in &target.tools {
        if !caller.tools.contains(tool) {
            return Err(CoordinatorError::CapabilityMismatch(format!(
                "target enables tool '{}' absent from caller",
                tool.name()
            )));
        }
    }

    // 3. Tool limits
    let tl = &target.tool_limits;
    let cl = &caller.tool_limits;
    if tl.max_concurrent_operations > cl.max_concurrent_operations
        || tl.call_timeout_secs > cl.call_timeout_secs
        || tl.max_output_bytes > cl.max_output_bytes
        || tl.max_directory_entries > cl.max_directory_entries
        || tl.max_file_scan_bytes > cl.max_file_scan_bytes
        || tl.max_search_files > cl.max_search_files
        || tl.max_search_depth > cl.max_search_depth
        || tl.max_search_scan_bytes > cl.max_search_scan_bytes
        || tl.max_search_matches > cl.max_search_matches
    {
        return Err(CoordinatorError::CapabilityMismatch(
            "target tool_limits exceed caller bounds".into(),
        ));
    }

    // 4. Write limits
    let target_has_write = target.tools.iter().any(|t| {
        matches!(
            t,
            crate::definition::WorkspaceTool::WriteFile
                | crate::definition::WorkspaceTool::EditFile
        )
    });
    let caller_has_write = caller.tools.iter().any(|t| {
        matches!(
            t,
            crate::definition::WorkspaceTool::WriteFile
                | crate::definition::WorkspaceTool::EditFile
        )
    });
    if target_has_write || target.write_limits.is_some() {
        let caller_wl = caller
            .write_limits
            .or_else(|| caller_has_write.then(crate::definition::WriteToolLimits::default))
            .ok_or_else(|| {
                CoordinatorError::CapabilityMismatch("target write_limits exceed caller".into())
            })?;
        let target_wl = target.write_limits.unwrap_or_default();
        if target_wl.max_edit_file_bytes > caller_wl.max_edit_file_bytes
            || target_wl.max_write_bytes > caller_wl.max_write_bytes
            || target_wl.max_edit_segments > caller_wl.max_edit_segments
        {
            return Err(CoordinatorError::CapabilityMismatch(
                "target write_limits exceed caller bounds".into(),
            ));
        }
    }

    // 5. Command settings
    if let Some(target_cmd) = target.effective_command_settings() {
        let Some(caller_cmd) = caller.effective_command_settings() else {
            return Err(CoordinatorError::CapabilityMismatch(
                "target command settings absent from caller".into(),
            ));
        };
        if (caller_cmd.network == crate::definition::CommandNetwork::None
            && target_cmd.network == crate::definition::CommandNetwork::Open)
            || target_cmd.default_timeout_secs > caller_cmd.default_timeout_secs
            || target_cmd.max_timeout_secs > caller_cmd.max_timeout_secs
        {
            return Err(CoordinatorError::CapabilityMismatch(
                "target command settings exceed caller bounds".into(),
            ));
        }
    }

    // 6. Code execution
    if let Some(tce) = target.code_execution {
        let Some(cce) = caller.code_execution else {
            return Err(CoordinatorError::CapabilityMismatch(
                "target code_execution absent from caller".into(),
            ));
        };
        if tce.timeout_secs > cce.timeout_secs
            || tce.memory_limit_bytes > cce.memory_limit_bytes
            || tce.stack_limit_bytes > cce.stack_limit_bytes
            || tce.max_subcalls > cce.max_subcalls
            || tce.max_concurrency > cce.max_concurrency
            || tce.max_output_bytes > cce.max_output_bytes
        {
            return Err(CoordinatorError::CapabilityMismatch(
                "target code_execution exceeds caller bounds".into(),
            ));
        }
    }

    // 7. MCP servers subset
    for server in &target.mcp_servers {
        if !caller.mcp_servers.iter().any(|s| s == server) {
            return Err(CoordinatorError::CapabilityMismatch(format!(
                "target MCP server '{}' not covered by caller",
                server.id
            )));
        }
    }

    // 8. Permission mode
    if permission_mode_rank(target.initial_permission_mode())
        > permission_mode_rank(caller.initial_permission_mode())
        || permission_mode_rank(target_live_mode) > permission_mode_rank(caller_live_mode)
    {
        return Err(CoordinatorError::CapabilityMismatch(
            "target permission mode exceeds caller permission mode".into(),
        ));
    }

    // 9. Rounds, tool calls, and timeouts
    if target.max_model_rounds > caller.max_model_rounds {
        return Err(CoordinatorError::CapabilityMismatch(
            "target max_model_rounds exceeds caller".into(),
        ));
    }
    if let Some(req_rounds) = requested_max_rounds
        && (req_rounds > caller.max_model_rounds || req_rounds > target.max_model_rounds)
    {
        return Err(CoordinatorError::CapabilityMismatch(
            "requested max_rounds exceeds caller or target ceiling".into(),
        ));
    }
    if target.max_tool_calls > caller.max_tool_calls {
        return Err(CoordinatorError::CapabilityMismatch(
            "target max_tool_calls exceeds caller".into(),
        ));
    }
    if let Some(caller_timeout) = caller.work_timeout_secs {
        if target
            .work_timeout_secs
            .is_some_and(|tt| tt > caller_timeout)
        {
            return Err(CoordinatorError::CapabilityMismatch(
                "target work_timeout_secs exceeds caller".into(),
            ));
        }
        if requested_timeout_secs.is_some_and(|rt| rt > caller_timeout) {
            return Err(CoordinatorError::CapabilityMismatch(
                "requested timeout_secs exceeds caller".into(),
            ));
        }
    }
    if let Some(target_timeout) = target.work_timeout_secs
        && requested_timeout_secs.is_some_and(|rt| rt > target_timeout)
    {
        return Err(CoordinatorError::CapabilityMismatch(
            "requested timeout_secs exceeds target work_timeout_secs".into(),
        ));
    }

    Ok(())
}

fn build_initial_child_context(role_prompt: Option<&str>) -> Context {
    let Some(role) = role_prompt.filter(|r| !r.is_empty()) else {
        return Context::new();
    };
    Context::from_blocks(vec![ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::System, role),
        BlockMeta::from_source(ROLE_PROMPT_SOURCE),
    )])
    .expect("single fresh role prompt block has unique id")
}

fn extract_role_prompt(context: &Context) -> Option<String> {
    context.blocks().iter().find_map(|block| {
        if block.meta().source.as_deref() == Some(ROLE_PROMPT_SOURCE)
            && let BlockContent::Message {
                role: Role::System,
                parts,
            } = block.content()
        {
            let mut text = String::new();
            for part in parts {
                if let causa::kernel::ContentPart::Text(payload) = part {
                    text.push_str(&payload.0);
                }
            }
            return Some(text);
        }
        None
    })
}

fn extract_turn_assistant_text(context: &Context, start_block_len: usize) -> String {
    let mut chunks = Vec::new();
    for block in context.blocks().iter().skip(start_block_len) {
        if let BlockContent::Message {
            role: Role::Assistant,
            parts,
        } = block.content()
        {
            let mut msg = String::new();
            for part in parts {
                if let causa::kernel::ContentPart::Text(payload) = part {
                    msg.push_str(&payload.0);
                }
            }
            if !msg.is_empty() {
                chunks.push(msg);
            }
        }
    }
    chunks.join("\n")
}

fn parse_error_from_outcome_json(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let val: serde_json::Value = serde_json::from_str(raw).ok()?;
    val.get("error").and_then(|v| v.as_str()).map(str::to_owned)
}

fn canonical_narrowing_view(req: &ChildNarrowingRequest) -> CanonicalNarrowing<'_> {
    CanonicalNarrowing {
        model_override: req.model_override.as_ref().map(|m| CanonicalModelOverride {
            provider: m.provider,
            model: m.model.as_deref(),
            endpoint: m.endpoint.as_ref().map(|e| e.as_deref()),
            credential: m.credential.as_ref(),
            max_tokens: m.max_tokens,
            reasoning_effort: m.reasoning_effort.as_ref().map(|r| r.as_deref()),
            temperature: m.temperature,
            extra_body: m.extra_body.as_ref(),
            context_window: m.context_window,
            max_model_attempts: m.max_model_attempts,
            render_options: m.render_options.as_ref(),
        }),
        workspace_root: req
            .workspace_root
            .as_ref()
            .map(|opt| opt.as_ref().map(|p| p.to_string_lossy().into_owned())),
        tools: req.tools.as_deref(),
        tool_limits: req.tool_limits.as_ref(),
        write_limits: req.write_limits,
        command: req.command,
        code_execution: req.code_execution,
        mcp_server_ids: req.mcp_server_ids.as_deref(),
        max_model_rounds: req.max_model_rounds,
        max_tool_calls: req.max_tool_calls,
        work_timeout_secs: req.work_timeout_secs,
        permission_mode: req.permission_mode,
    }
}

struct ActiveOperationSlotGuard(Arc<Mutex<Option<String>>>);

impl Drop for ActiveOperationSlotGuard {
    fn drop(&mut self) {
        *self.0.lock().expect("active op slot lock") = None;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn serialize_spawn_input(req: &SpawnAgentRequest) -> Result<String, CoordinatorError> {
    let payload = CanonicalSpawnFingerprint {
        kind: "spawn_agent",
        caller_agent_id: &req.caller_agent_id,
        name: &req.name,
        preset_name: req.preset_name.as_deref(),
        role_prompt: req.role_prompt.as_deref(),
        prompt: &req.prompt,
        narrowing: canonical_narrowing_view(&req.narrowing),
        interaction: req.interaction,
        execution_mode: req.execution_mode,
        on_parent_complete: req.on_parent_complete,
        timeout_secs: req.timeout_secs,
        max_rounds: req.max_rounds,
        coalesce_key: req.coalesce_key.as_deref(),
    };
    serde_json::to_string(&payload)
        .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))
}

fn serialize_message_input(req: &MessageAgentRequest) -> Result<String, CoordinatorError> {
    let payload = CanonicalMessageFingerprint {
        kind: "message_agent",
        caller_agent_id: &req.caller_agent_id,
        target_agent_id: &req.target_agent_id,
        prompt: &req.prompt,
        execution_mode: req.execution_mode,
        on_parent_complete: req.on_parent_complete,
        timeout_secs: req.timeout_secs,
        max_rounds: req.max_rounds,
        coalesce_key: req.coalesce_key.as_deref(),
    };
    serde_json::to_string(&payload)
        .map_err(|err| CoordinatorError::Store(Arc::new(StoreError::from(err))))
}

fn fingerprint_spawn(req: &SpawnAgentRequest) -> Result<String, CoordinatorError> {
    let json = serialize_spawn_input(req)?;
    Ok(sha256_hex(json.as_bytes()))
}

fn fingerprint_message(req: &MessageAgentRequest) -> Result<String, CoordinatorError> {
    let json = serialize_message_input(req)?;
    Ok(sha256_hex(json.as_bytes()))
}
