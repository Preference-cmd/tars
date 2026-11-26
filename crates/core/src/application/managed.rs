#[cfg(test)]
use super::coordinator::CoordinatorConfig;
use super::coordinator::{
    CoordinatorError, MessageAgentRequest, OperationOutcome, ReadAgentResult, SpawnAgentOutcome,
    SpawnAgentRequest,
};
use super::entry::Entry;
use super::saving;
use super::types::*;
use crate::agent::{
    AgentMaterialSnapshot, AgentMaterialVersion, AgentRepairInspection, AgentRepairPlan,
    AgentSnapshot,
};
use crate::compact::{CompactionOutcome, CompactionRuntime, HistoryRecordEntry};
use crate::definition::SessionDefinition;
use crate::events::{BackgroundTaskSnapshot, ConsumedInboxEvents, SessionEvent};
use crate::session::{
    SessionError, SubmitRequest, WorkObservation, WorkProgress, WorkReceipt, WorkRef,
};
use crate::store::{PreparedCommit, SavedSessionRecord};
use crate::usage::{SessionUsage, WorkUsage};
use causa::kernel::{CallControl, CancellationToken, Context, ContextBlock};
use std::sync::Arc;
use std::time::Duration;

/// Cloneable view permanently bound to one instance, without low-level mutation access.
#[derive(Clone)]
pub struct ManagedSession {
    pub(super) entry: Arc<Entry>,
}
impl ManagedSession {
    /// Read the instance's active permission mode and revision.
    ///
    /// For instances assembled without permission state (such as test fixtures),
    /// this returns `None`.
    pub fn permission_mode(&self) -> Option<(crate::permission::PermissionMode, u64)> {
        let state = self.entry.lock();
        state.permissions.as_ref().map(|p| p.get())
    }

    /// Snapshot of the instance's MCP servers, in definition order.
    ///
    /// Returns an empty list for instances without an MCP runtime and for an
    /// instance whose session is already closed.
    pub fn mcp_status(&self) -> Vec<crate::McpServerStatus> {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .map_or_else(Vec::new, |owner| owner.mcp_status())
    }

    /// The probed command sandbox for this instance, captured when it opened.
    ///
    /// Returns `None` for instances without the command tool, for an instance
    /// whose session is already closed, and for test fixtures. The status is
    /// frozen at assembly; it does not change while the instance stays open.
    pub fn command_status(&self) -> Option<crate::sandbox::SandboxStatus> {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.command_status())
    }

    /// Change the instance's active permission mode, incrementing revision on actual change.
    ///
    /// If the session is closing, this returns `Err(ApplicationError::Closing)`.
    /// If the session is already closed, this returns `Err(ApplicationError::Closed)`.
    /// For instances assembled without permission state (such as test fixtures),
    /// this returns `Err(ApplicationError::PermissionUnavailable)`.
    ///
    /// When switching modes in an idle persistent session, a save operation is initiated.
    /// If active work or another save is in progress, the latest mode will be included in the
    /// subsequent save.
    pub fn set_permission_mode(
        &self,
        mode: crate::permission::PermissionMode,
    ) -> Result<(crate::permission::PermissionMode, u64), ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        if state.owner.is_none() {
            return Err(ApplicationError::InstanceGone);
        }
        let permissions = state
            .permissions
            .as_ref()
            .ok_or(ApplicationError::PermissionUnavailable)?;
        let prev_mode = permissions.get().0;
        let (new_mode, rev) = permissions.set(mode);
        if prev_mode != new_mode
            && state.persistent
            && state.active.is_none()
            && state.pending.is_none()
            && state.lifecycle == Lifecycle::Open
        {
            saving::prepare(&self.entry, &mut state)?;
            self.entry.notify();
        }
        Ok((new_mode, rev))
    }

    /// Connect the instance's approval channel receiver.
    ///
    /// At most one receiver is connected at any time; calling this again replaces
    /// any previously connected receiver. For instances assembled without permission
    /// state (such as test fixtures), this returns an immediately closed receiver.
    pub fn connect_approvals(&self) -> crate::permission::ApprovalReceiver {
        let state = self.entry.lock();
        state.permissions.as_ref().map_or_else(
            || {
                let (_, rx) = tokio::sync::mpsc::channel(1);
                rx
            },
            |p| p.connect_approvals(),
        )
    }

    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn workspace_tools(&self) -> Option<Arc<crate::tools::WorkspaceTools>> {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.workspace_tools())
    }

    /// Replace command approval rules on the instance's permission hub during test runs.
    #[cfg(test)]
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    pub(crate) fn set_command_rules(&self, rules: crate::permission::CommandRuleSet) {
        let state = self.entry.lock();
        if let Some(permissions) = &state.permissions {
            permissions.set_rules(rules);
        }
    }

    /// Append an extra before processor after the permission processor during test runs.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn add_test_before_processor(
        &self,
        processor: Arc<dyn causa::kernel::ToolBatchProcessor>,
    ) {
        let state = self.entry.lock();
        if let Some(owner) = &state.owner {
            owner.add_test_before_processor(processor);
        }
    }

    /// Register a test hook invoked after `prepare` and before live re-decision in `ToolPolicyProcessor`.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set_test_after_prepare_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        let state = self.entry.lock();
        if let Some(owner) = &state.owner {
            owner.set_test_after_prepare_hook(hook);
        }
    }
    /// Subscribe to this instance's bounded, temporary model observations.
    /// Subscribe before submit; late subscriptions have no history. Capacity is
    /// 128 observations, each text fragment at most 16 KiB. `Lagged` does not
    /// affect reliable work results, cancellation, material return or saving.
    pub fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<Arc<WorkProgress>> {
        self.entry
            .lock()
            .handle
            .as_ref()
            .expect("published handle")
            .subscribe_progress()
    }

    /// Exact instance target for privileged host operations.
    pub fn target(&self) -> SessionTarget {
        self.entry.target.clone()
    }
    /// Frozen definition used by this instance's actual assembly.
    pub fn definition(&self) -> SessionDefinition {
        self.entry
            .lock()
            .definition
            .clone()
            .expect("published definition")
    }
    /// Read execution, lifecycle and save progress independently.
    pub fn status(&self) -> SessionStatus {
        self.entry.status()
    }
    /// Title of the last stored record, taken from the first user message.
    pub fn title(&self) -> Option<String> {
        self.entry.lock().saved.title.clone()
    }
    /// Creation timestamp in milliseconds since Unix epoch, if derived from UUIDv7 ID.
    pub fn created_at_ms(&self) -> Option<u64> {
        crate::time::created_at_from_id(self.entry.target.conversation_id.as_str())
    }
    /// Last-activity time of the last stored record, in Unix milliseconds;
    /// the creation time before the first record is stored.
    pub fn updated_at_ms(&self) -> u64 {
        self.entry.lock().saved.updated_at_ms
    }
    /// Facts learned while this instance was opened.
    pub fn open_report(&self) -> OpenReport {
        self.entry.lock().open_report.clone()
    }

    /// Whether the workspace `.tars/permissions.toml` file is trusted (also `true`
    /// when no workspace or no workspace rules file is present).
    pub fn workspace_rules_trusted(&self) -> bool {
        self.entry.lock().open_report.workspace_rules.trusted
    }

    /// Whether workspace `.tars/agents/*.md` presets are trusted (also `true`
    /// when no workspace or no workspace agent files are present).
    pub fn workspace_agents_trusted(&self) -> bool {
        self.entry.lock().open_report.workspace_agents.trusted
    }

    /// Point-in-time snapshot of the currently active trusted [`crate::agents::AgentRegistry`].
    pub fn agent_registry_snapshot(&self) -> crate::agents::AgentRegistry {
        self.entry
            .agent_registry
            .read()
            .expect("agent registry lock")
            .clone()
    }

    /// Snapshot of the currently active three-layer command permission rules.
    pub fn command_rules_snapshot(&self) -> crate::permission::CommandRuleSet {
        let state = self.entry.lock();
        state
            .permissions
            .as_ref()
            .map_or_else(crate::permission::CommandRuleSet::new, |p| {
                p.command_rules()
            })
    }

    /// Remove an in-memory session command rule by 0-based index.
    ///
    /// Returns `Ok(true)` if a rule at `index` existed and was removed, or
    /// `Ok(false)` when `index` is out of bounds.
    pub fn remove_session_rule(&self, index: usize) -> Result<bool, ApplicationError> {
        let state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        if state.owner.is_none() {
            return Err(ApplicationError::InstanceGone);
        }
        let permissions = state
            .permissions
            .as_ref()
            .ok_or(ApplicationError::PermissionUnavailable)?;
        let removed = permissions.remove_session_rule(index);
        let rules = permissions.command_rules();
        drop(state);
        if let Some(coord) = self.entry.coordinator.get() {
            coord.update_command_rules(rules);
        }
        Ok(removed)
    }

    /// Clear all in-memory session command rules, returning the number of rules cleared.
    pub fn clear_session_rules(&self) -> Result<usize, ApplicationError> {
        let state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        if state.owner.is_none() {
            return Err(ApplicationError::InstanceGone);
        }
        let permissions = state
            .permissions
            .as_ref()
            .ok_or(ApplicationError::PermissionUnavailable)?;
        let count = permissions.clear_session_rules();
        let rules = permissions.command_rules();
        drop(state);
        if let Some(coord) = self.entry.coordinator.get() {
            coord.update_command_rules(rules);
        }
        Ok(count)
    }

    /// Trust the workspace `.tars/permissions.toml` snapshot captured when this
    /// session opened, write it to the Store trust copy, and activate its
    /// `allow` rules in the live session.
    ///
    /// Never re-reads `.tars/permissions.toml` from disk, avoiding TOCTOU races.
    /// Returns the number of newly activated `allow` rules, or
    /// `Err(ApplicationError::StoreUnavailable)` when the host has no Store.
    pub async fn trust_workspace_rules(&self) -> Result<usize, ApplicationError> {
        let (store, io, snapshot) = {
            let state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            if state.lifecycle == Lifecycle::Closing {
                return Err(ApplicationError::Closing);
            }
            let store = self
                .entry
                .store
                .clone()
                .ok_or(ApplicationError::StoreUnavailable)?;
            if state.open_report.workspace_rules.trusted {
                return Ok(0);
            }
            let snapshot = state.workspace_rules_snapshot.clone();
            (store, self.entry.io.clone(), snapshot)
        };
        let Some(snapshot) = snapshot else {
            return Ok(0);
        };
        let workspace_root = snapshot.workspace_root.clone();
        let raw_text = snapshot.raw_text.clone();
        saving::io_semaphore(&io, move || {
            store.write_workspace_rules_trust(&workspace_root, &raw_text)
        })
        .await?;
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        if state.open_report.workspace_rules.trusted {
            return Ok(0);
        }
        if state.owner.is_none() {
            return Err(ApplicationError::InstanceGone);
        }
        let activated = snapshot
            .rules
            .iter()
            .filter(|rule| rule.decision == crate::permission::RuleDecision::Allow)
            .count();
        if let Some(permissions) = &state.permissions {
            permissions.trust_workspace_rules(snapshot.rules, snapshot.raw_text.clone());
            let rules = permissions.command_rules();
            let backend =
                self.entry
                    .store
                    .as_ref()
                    .map(|store| crate::permission::WorkspaceRulesBackend {
                        workspace_root: snapshot.workspace_root,
                        store: store.clone(),
                        trusted: true,
                        trusted_raw_text: Some(snapshot.raw_text),
                    });
            if let Some(coord) = self.entry.coordinator.get() {
                coord.update_command_rules(rules);
                coord.update_workspace_rules_backend(backend);
            }
        }
        state.open_report.workspace_rules.allow_rules.clear();
        state.open_report.workspace_rules.previous_trusted_text = None;
        state.open_report.workspace_rules.diff = None;
        state.open_report.workspace_rules.trusted = true;
        Ok(activated)
    }

    /// Trust the workspace `.tars/agents/*.md` snapshot captured when this session opened,
    /// write its manifest to the Store trust copy, and activate its presets in the live
    /// [`crate::agents::AgentRegistry`].
    ///
    /// Never re-reads `.tars/agents/*.md` from disk, avoiding TOCTOU races.
    /// Returns the number of newly activated workspace agent presets, or
    /// `Err(ApplicationError::StoreUnavailable)` when the host has no Store.
    pub async fn trust_workspace_agents(&self) -> Result<usize, ApplicationError> {
        let (store, io, snapshot) = {
            let state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            if state.lifecycle == Lifecycle::Closing {
                return Err(ApplicationError::Closing);
            }
            let store = self
                .entry
                .store
                .clone()
                .ok_or(ApplicationError::StoreUnavailable)?;
            if state.open_report.workspace_agents.trusted {
                return Ok(0);
            }
            let snapshot = state.workspace_agents_snapshot.clone();
            (store, self.entry.io.clone(), snapshot)
        };
        let Some(snapshot) = snapshot else {
            return Ok(0);
        };
        let workspace_root = snapshot.workspace_root.clone();
        let manifest_text = snapshot.manifest_text.clone();
        saving::io_semaphore(&io, move || {
            store.write_workspace_agents_trust(&workspace_root, &manifest_text)
        })
        .await?;
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        if state.open_report.workspace_agents.trusted {
            return Ok(0);
        }
        if state.owner.is_none() {
            return Err(ApplicationError::InstanceGone);
        }
        let activated = snapshot.presets.len();
        let def = state.definition.clone().expect("published definition");
        state.trusted_workspace_agents = snapshot.presets;
        let next_registry = if def.prelude.agents_enabled {
            crate::agents::AgentDiscovery::discover(
                def.prelude.config_dir.as_deref(),
                &def.prelude.agent_dirs,
                &state.trusted_workspace_agents,
            )
            .registry
        } else {
            crate::agents::AgentRegistry::new()
        };
        *self
            .entry
            .agent_registry
            .write()
            .expect("agent registry lock") = next_registry;
        state.open_report.workspace_agents.untrusted_agents.clear();
        state.open_report.workspace_agents.trusted = true;
        state.workspace_agents_snapshot = None;
        Ok(activated)
    }
    /// Non-fatal prelude diagnostics recorded during preparation or the most recent submit.
    pub fn prelude_diagnostics(&self) -> Vec<crate::prelude::PreludeDiagnostic> {
        self.entry.lock().prelude_diagnostics.clone()
    }
    /// Replay a retained key first; otherwise synchronously admit under the save barrier.
    pub fn submit(&self, request: SubmitRequest) -> Result<WorkReceipt, ManagedSubmitRejection> {
        let mut state = self.entry.lock();
        let handle = state.handle.as_ref().expect("published handle").clone();
        if let Some(result) = handle.inspect_request(&request) {
            return result.map_err(|reason| ManagedSubmitRejection {
                reason: reason.into(),
                request,
            });
        }
        if let Err(reason) = self.entry.new_mutation(&state) {
            return Err(ManagedSubmitRejection { reason, request });
        }
        let owner = state.owner.as_mut().expect("published owner");
        let history = owner.history_entries();
        let artifacts = owner.compaction().and_then(|c| c.history());
        let readiness = owner.with_context(|context| {
            super::repair::admission_check(context, &history, artifacts.as_deref())
        });
        let readiness = readiness
            .map_err(ApplicationError::from)
            .and_then(|result| result.map_err(Into::into));
        if let Err(reason) = readiness {
            return Err(ManagedSubmitRejection { reason, request });
        }
        // Retained works that a caller has already read keep the sheet full.
        // Release the oldest eligible ones before the low-level capacity gate;
        // unread results and unclaimed batches stay observable/repairable.
        let capacity = state
            .definition
            .as_ref()
            .expect("published definition")
            .retained_work_capacity;
        let owner = state.owner.as_mut().expect("published owner");
        loop {
            if owner.retained_count() < capacity {
                break;
            }
            let candidate = owner
                .retained()
                .into_iter()
                .find(|work| work.finished && work.read && !work.uncommitted)
                .map(|work| work.work);
            let Some(candidate) = candidate else {
                break;
            };
            if let Err(error) = owner.forget(&candidate) {
                return Err(ManagedSubmitRejection {
                    reason: error.into(),
                    request,
                });
            }
        }
        let persistent = state.persistent;
        let definition = state
            .definition
            .as_ref()
            .expect("published definition")
            .clone();
        let current_mode = state
            .permissions
            .as_ref()
            .map(|p| p.get().0)
            .unwrap_or_else(|| definition.initial_permission_mode());
        let trusted_workspace_agents = state.trusted_workspace_agents.clone();
        let model_profiles = self.entry.model_profiles.clone();
        let owner = state.owner.as_mut().expect("published owner");
        let skill_registry = owner.skill_registry();
        let loaded_skills = owner.loaded_skills();
        let prelude_evaluation = owner.with_context(|context| {
            let blocks = context.blocks();
            loaded_skills.sync_from_context(blocks);
            if !crate::prelude::has_prelude(blocks) {
                return None;
            }
            let runtime_state = crate::prelude::PreludeRuntimeState::from_definition(
                &definition,
                current_mode,
                persistent,
            );
            let (discovered_registry, mut diagnostics) =
                crate::prelude::discover_skills_for_prelude(&definition.prelude, &runtime_state);
            let agent_registry = if definition.prelude.agents_enabled {
                let discovery = crate::agents::AgentDiscovery::discover(
                    definition.prelude.config_dir.as_deref(),
                    &definition.prelude.agent_dirs,
                    &trusted_workspace_agents,
                );
                diagnostics.extend(
                    discovery
                        .diagnostics
                        .into_iter()
                        .map(crate::prelude::PreludeDiagnostic::from),
                );
                discovery.registry
            } else {
                crate::agents::AgentRegistry::new()
            };
            let include_load_skill =
                definition.prelude.skills_enabled && !discovered_registry.is_empty();
            let budget = prelude_budget_config(&definition, persistent, include_load_skill);
            Some(crate::prelude::evaluate_dynamic_updates_with_registries(
                blocks,
                &definition.prelude,
                &runtime_state,
                &budget,
                discovered_registry,
                agent_registry,
                &model_profiles,
                diagnostics,
            ))
        });
        let (prelude_updates, update_diagnostics, next_registry, next_agent_registry) =
            match prelude_evaluation {
                Ok(Some(update)) => (
                    update.blocks,
                    update.diagnostics,
                    Some(update.registry),
                    Some(update.agent_registry),
                ),
                Ok(None) => (Vec::new(), Vec::new(), None, None),
                Err(error) => {
                    return Err(ManagedSubmitRejection {
                        reason: error.into(),
                        request,
                    });
                }
            };
        if let Some(next_registry) = next_registry {
            *skill_registry.write().expect("skill registry lock") = next_registry;
        }
        if let Some(next_agent_registry) = next_agent_registry {
            *self
                .entry
                .agent_registry
                .write()
                .expect("agent registry lock") = next_agent_registry;
        }
        let receipt = handle
            .submit_with_prelude_updates(request, prelude_updates)
            .map_err(|failure| ManagedSubmitRejection {
                reason: failure.reason.into(),
                request: failure.request,
            })?;
        state.prelude_diagnostics = update_diagnostics;
        // The supervisor already exists. This synchronous registration holds the
        // application gate until it captures and confirms this work's materials.
        state.active = Some(receipt.work.clone());
        state.root_work_seq = state.root_work_seq.saturating_add(1);
        state.last_root_work = Some(receipt.work.clone());
        if state.persistent {
            state.save = SaveStatus::Dirty {
                base_revision: state.revision,
                material_version: self.entry.version(&state),
            };
        }
        if let Some(coord) = self.entry.coordinator.get() {
            coord.note_entry_user_submit(&receipt.work);
        }
        self.entry.notify();
        Ok(receipt)
    }
    /// Read the original low-level result without synthesizing persistence outcomes.
    pub fn observe(&self, work: &WorkRef) -> Result<WorkObservation, SessionError> {
        self.entry
            .lock()
            .handle
            .as_ref()
            .expect("published handle")
            .observe(work)
    }
    /// Wait a finite time for the original work; abandoning this wait cancels nothing.
    pub async fn wait(
        &self,
        work: &WorkRef,
        timeout: Duration,
    ) -> Result<WorkObservation, SessionError> {
        let handle = self
            .entry
            .lock()
            .handle
            .as_ref()
            .expect("published handle")
            .clone();
        handle.wait(work, timeout).await
    }
    /// Request cooperative cancellation without changing a finished result.
    pub fn cancel(&self, work: &WorkRef) -> Result<(), SessionError> {
        let handle = self
            .entry
            .lock()
            .handle
            .as_ref()
            .expect("published handle")
            .clone();
        handle.cancel(work)?;
        if let Some(coord) = self.entry.coordinator.get() {
            coord.cancel_children_of_root_work(work);
        }
        Ok(())
    }
    /// Copy actual idle materials, including while save is blocked, for recovery/export.
    pub fn read_materials(&self) -> Result<MaterialSnapshot, ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.active.is_some() && state.pending.is_none() && state.fault.is_none() {
            return Err(SessionError::Busy {
                work: state.active.clone().expect("active"),
            }
            .into());
        }
        let version = self.entry.version(&state);
        let owner = state.owner.as_mut().ok_or(ApplicationError::InstanceGone)?;
        // Read together under the entry lock: with no work running, no rewrite
        // can land between the Context and its index.
        let history = owner.history_entries();
        let context = owner.with_context(|context| {
            self.entry.cloned_snapshot();
            context.clone()
        })?;
        Ok(MaterialSnapshot {
            version,
            context,
            history,
        })
    }
    /// Atomically replace all blocks if the instance generation and admission gate match.
    /// Invalid identities leave the actual context unchanged; no arbitrary callbacks run.
    pub fn edit(
        &self,
        base: MaterialVersion,
        blocks: Vec<ContextBlock>,
    ) -> Result<MaterialVersion, ApplicationError> {
        let mut state = self.entry.lock();
        if base.target != self.entry.target {
            return Err(ApplicationError::StaleInstance);
        }
        if base != self.entry.version(&state) {
            return Err(ApplicationError::StaleMaterial);
        }
        self.entry.new_mutation(&state)?;
        // Validate the complete replacement and persistent snapshot before
        // mutating the owner. Every ordinary rejection leaves materials intact.
        let mut candidate = state
            .owner
            .as_mut()
            .expect("published owner")
            .with_context(|context| {
                self.entry.cloned_snapshot();
                context.clone()
            })?;
        let len = candidate.blocks().len();
        candidate
            .edit()
            .replace(0..len, blocks)
            .commit()
            .map_err(|failure| SessionError::ContextEdit(failure.reason))?;
        self.commit_materials(&mut state, candidate)
    }

    // Called only with the application gate held, after the caller validates
    // its baseline and the complete immutable candidate.
    pub(super) fn commit_materials(
        &self,
        state: &mut super::entry::State,
        candidate: Context,
    ) -> Result<MaterialVersion, ApplicationError> {
        let next = state
            .generation
            .checked_add(1)
            .ok_or_else(|| ApplicationError::Faulted("material generation exhausted".into()))?;
        let current_mode = state.permissions.as_ref().map(|p| p.get().0);
        let pending = if state.persistent {
            self.entry.cloned_snapshot();
            let (auto_compact, compact_threshold, history) = saving::compaction_record(state);
            let usage = state.owner.as_ref().map(|o| o.session_usage());
            let (title, updated_at_ms) = saving::next_metadata(state, &candidate);
            let record = SavedSessionRecord::new_with_compaction(
                self.entry.target.conversation_id.as_str().to_owned(),
                uuid::Uuid::now_v7().to_string(),
                state.revision,
                state.definition.clone().expect("definition"),
                candidate.clone(),
                current_mode,
                auto_compact,
                compact_threshold,
                history,
                usage,
                title,
                updated_at_ms,
            )?;
            Some(Arc::new(PreparedCommit::new(state.revision, record)?))
        } else {
            None
        };
        let persistent = state.persistent;
        let transient_sync = if !persistent {
            let owner = state.owner.as_ref().expect("published owner");
            Some((
                candidate.clone(),
                owner.history_entries(),
                owner.session_usage(),
            ))
        } else {
            None
        };
        state
            .owner
            .as_mut()
            .expect("published owner")
            .with_context(|context| *context = candidate)?;
        state.generation = next;
        if let Some(pending) = pending {
            state.pending = Some(pending);
            state.pending_attempted = false;
            state.save_requested = true;
            state.save = SaveStatus::Dirty {
                base_revision: state.revision,
                material_version: self.entry.version(state),
            };
        }
        self.entry.notify();
        let version = self.entry.version(state);
        if let Some((ctx, hist, usage)) = transient_sync
            && let Some(coord) = self.entry.coordinator.get()
        {
            coord.sync_entry_materials_after_save(
                None,
                next,
                Some(ctx),
                Some(hist),
                Some(usage),
                &ConsumedInboxEvents::default(),
            );
        }
        Ok(version)
    }
    /// Wait for the entry root commit/save barrier without checking child outboxes.
    async fn wait_entry_saved(&self) -> Result<SaveStatus, ApplicationError> {
        loop {
            let changed = self.entry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.entry.lock();
                if let Some(error) = &state.fault {
                    return Err(error.clone());
                }
                if !state.save_requested {
                    if let SaveStatus::SaveBlocked { error, .. } = &state.save {
                        return Err(ApplicationError::SaveBlocked(error.clone()));
                    }
                    if state.active.is_none()
                        && matches!(
                            state.save,
                            SaveStatus::Saved { .. } | SaveStatus::NotTracked
                        )
                    {
                        return Ok(state.save.clone());
                    }
                    if state.lifecycle == Lifecycle::Closed {
                        return Err(ApplicationError::Closed);
                    }
                }
            }
            changed.await;
        }
    }

    async fn wait_root_commit_settled(&self) -> Result<(), ApplicationError> {
        loop {
            let changed = self.entry.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.entry.lock();
                if let Some(error) = &state.fault {
                    return Err(error.clone());
                }
                if !state.save_requested && !state.save_in_flight {
                    if state.pending.is_none() {
                        return Ok(());
                    }
                    if let SaveStatus::SaveBlocked { error, .. } = &state.save {
                        return Err(ApplicationError::SaveBlocked(error.clone()));
                    }
                    if state.lifecycle == Lifecycle::Closed {
                        return Err(ApplicationError::Closed);
                    }
                }
            }
            changed.await;
        }
    }

    fn trigger_root_retry_save(&self, allow_close: bool) -> Result<(), ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.pending.is_some() && matches!(state.save, SaveStatus::SaveBlocked { .. }) {
            state.save_requested = true;
            state.error = None;
            if allow_close && state.closing.is_some() {
                state.close_in_flight = true;
            }
            self.entry.notify();
        } else if state.pending.is_none() && matches!(state.save, SaveStatus::SaveBlocked { .. }) {
            if let Some(rev) = state.revision {
                state.save = SaveStatus::Saved {
                    revision: rev,
                    material_version: self.entry.version(&state),
                };
            } else {
                state.save = SaveStatus::NotTracked;
            }
            state.error = None;
            if allow_close && state.closing.is_some() {
                state.close_in_flight = true;
                self.entry.notify();
            }
            self.entry.changed();
        } else if allow_close && state.closing.is_some() && !state.close_in_flight {
            state.close_in_flight = true;
            self.entry.notify();
        }
        Ok(())
    }

    /// Wait for this instance's current execution/save barrier to settle.
    pub async fn wait_saved(&self) -> Result<SaveStatus, ApplicationError> {
        let entry_status = self.wait_entry_saved().await?;
        if let Some(coord) = self.entry.coordinator.get() {
            loop {
                let notified = coord.state_changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(err) = coord.save_blocked_error() {
                    return Err(ApplicationError::SaveBlocked(err));
                }
                if !coord.has_pending_child_saves() {
                    break;
                }
                notified.await;
            }
        }
        Ok(entry_status)
    }
    /// Confirm or retry the same immutable pending commit; concurrent retries join.
    pub async fn retry_save(&self) -> Result<SaveStatus, ApplicationError> {
        let writer = self.entry.lock().writer.clone();
        let pending_kind = writer
            .as_ref()
            .map_or(crate::store::PendingWriterKind::None, |w| w.pending_kind());
        let coord = self.entry.coordinator.get().cloned();

        if pending_kind == crate::store::PendingWriterKind::RootCommit {
            // Order A: Writer holds a retained root commit; settle it first so
            // WriterState::pending clears before retrying any deferred outboxes.
            self.trigger_root_retry_save(false)?;
            self.wait_root_commit_settled().await?;
            if let Some(coord) = &coord
                && (coord.has_pending_child_saves() || coord.save_blocked_error().is_some())
            {
                coord.retry_pending_outboxes(&self.entry).await?;
            }
            self.trigger_root_retry_save(true)?;
            return self.wait_saved().await;
        }

        // Order B / outbox-first: drain pending outboxes first, then retry any pending root commit.
        if let Some(coord) = &coord
            && (coord.has_pending_child_saves() || coord.save_blocked_error().is_some())
        {
            coord.retry_pending_outboxes(&self.entry).await?;
        }
        self.trigger_root_retry_save(true)?;
        self.wait_saved().await
    }

    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set_writer_fault_hook(&self, hook: Option<crate::store::FaultHook>) {
        if let Some(writer) = &self.entry.lock().writer {
            writer.set_fault_hook(hook);
        }
    }

    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn pending_commit_id(&self) -> Option<String> {
        self.entry
            .lock()
            .pending
            .as_ref()
            .map(|p| p.record.last_commit_id.clone())
    }

    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn pending_commit_permission_mode(
        &self,
    ) -> Option<Option<crate::permission::PermissionMode>> {
        self.entry
            .lock()
            .pending
            .as_ref()
            .map(|p| p.record.permission_mode)
    }

    /// Retained-work capacity and current use, for status displays.
    pub fn work_capacity(&self) -> Option<WorkCapacity> {
        let state = self.entry.lock();
        let owner = state.owner.as_ref()?;
        let capacity = state
            .definition
            .as_ref()
            .map(|definition| definition.retained_work_capacity)?;
        Some(WorkCapacity {
            used: owner.retained_count(),
            capacity,
        })
    }

    /// Running compaction settings, when this session keeps history.
    pub fn compaction_settings(&self) -> Option<CompactionSettings> {
        let state = self.entry.lock();
        let runtime = state.owner.as_ref().and_then(|owner| owner.compaction())?;
        let (auto_compact, compact_threshold) = runtime.settings();
        Some(CompactionSettings {
            auto_compact,
            compact_threshold,
            context_window: runtime.window(),
        })
    }

    /// Replace the running automatic-compaction switch.
    ///
    /// Every new session starts from the frozen definition's value; the running
    /// value is stored only when it differs, and changing it in an idle
    /// persistent session starts a save.
    pub fn set_auto_compact(&self, enabled: bool) -> Result<(), ApplicationError> {
        self.update_compaction_settings(|runtime| {
            if runtime.settings().0 == enabled {
                return false;
            }
            runtime.set_auto_compact(enabled);
            true
        })
    }

    /// Replace the running trigger percentage; the supported range is 10..=95.
    pub fn set_compact_threshold(&self, threshold: u8) -> Result<(), ApplicationError> {
        if !(crate::definition::MIN_COMPACT_THRESHOLD..=crate::definition::MAX_COMPACT_THRESHOLD)
            .contains(&threshold)
        {
            return Err(crate::definition::DefinitionError::InvalidCompactThreshold.into());
        }
        self.update_compaction_settings(|runtime| {
            if runtime.settings().1 == threshold {
                return false;
            }
            runtime.set_threshold(threshold);
            true
        })
    }

    fn update_compaction_settings(
        &self,
        apply: impl FnOnce(&CompactionRuntime) -> bool,
    ) -> Result<(), ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.lifecycle == Lifecycle::Closing {
            return Err(ApplicationError::Closing);
        }
        let runtime = state
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .ok_or(ApplicationError::CompactionUnavailable)?;
        if !apply(&runtime) {
            return Ok(());
        }
        if state.persistent
            && state.active.is_none()
            && state.pending.is_none()
            && state.lifecycle == Lifecycle::Open
        {
            saving::prepare(&self.entry, &mut state)?;
            self.entry.notify();
        }
        Ok(())
    }

    /// Current request-size estimate for the status display.
    ///
    /// The estimate combines the last upstream prompt size with the material
    /// added since that request. Returns `None` for a session without
    /// compaction history or after it closed.
    pub fn context_usage(&self) -> Option<ContextUsage> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return None;
        }
        let owner = state.owner.as_mut()?;
        let runtime = owner.compaction()?;
        let bytes = owner
            .with_context(|context| {
                self.entry.cloned_snapshot();
                crate::compact::estimate::context_bytes(context)
            })
            .ok()?;
        let (auto_compact, compact_threshold) = runtime.settings();
        Some(ContextUsage {
            estimate_tokens: runtime.usage().estimate(bytes),
            context_window: runtime.window(),
            auto_compact,
            compact_threshold,
        })
    }

    /// Cumulative model usage across this open session instance.
    ///
    /// Returns `None` after the session is closed.
    pub fn session_usage(&self) -> Option<SessionUsage> {
        let state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return None;
        }
        let handle = state.handle.as_ref()?;
        Some(handle.session_usage())
    }

    /// Usage statistics for a specific retained work.
    ///
    /// Returns `None` if the work is unknown, already released when the
    /// retained-work sheet reached capacity, or after the session has closed.
    pub fn work_usage(&self, work: &WorkRef) -> Option<WorkUsage> {
        let state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return None;
        }
        let handle = state.handle.as_ref()?;
        handle.work_usage(work)
    }

    /// Index of the rewrite records, in creation order.
    pub fn history(&self) -> Vec<HistoryRecordEntry> {
        self.entry
            .lock()
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .map(|runtime| runtime.entries())
            .unwrap_or_default()
    }

    /// Read one history rewrite record for display; the current Context never includes it.
    pub fn read_history_record(
        &self,
        record_id: &str,
    ) -> Result<crate::compact::HistoryRecord, ApplicationError> {
        let (history, cached) = {
            let state = self.entry.lock();
            let runtime = state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .ok_or(ApplicationError::CompactionUnavailable)?;
            let history = runtime
                .history()
                .ok_or(ApplicationError::CompactionUnavailable)?;
            let cached = history.get_written_record(record_id);
            (history, cached)
        };
        if let Some(record) = cached {
            return Ok((*record).clone());
        }
        history
            .read(record_id)
            .map_err(|error| ApplicationError::Store(Arc::new(error)))
    }

    /// Artifact store backed by this session's rewrite records and `.artifacts/` directory.
    #[cfg(test)]
    pub(crate) fn artifacts(&self) -> Option<crate::compact::HistoryArtifacts> {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .and_then(|runtime| runtime.artifacts())
    }

    /// Read the UTF-8 text of one spilled tool-output artifact (`artifact:<id>` or `<id>`) for display.
    pub fn read_spilled_artifact(&self, id: &str) -> Result<String, ApplicationError> {
        let artifact_id = id.strip_prefix("artifact:").unwrap_or(id).to_owned();
        let history = {
            let state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            let runtime = state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .ok_or(ApplicationError::CompactionUnavailable)?;
            runtime
                .history()
                .ok_or(ApplicationError::CompactionUnavailable)?
        };
        let bytes = history
            .read_spilled_artifact(&artifact_id, None)
            .map_err(|error| ApplicationError::Store(Arc::new(error)))?;
        String::from_utf8(bytes).map_err(|_| {
            ApplicationError::Store(Arc::new(crate::store::StoreError::InvalidHistory(format!(
                "artifact {id} is not valid UTF-8 text"
            ))))
        })
    }

    /// Read the UTF-8 text of one tool-output artifact (`artifact:<id>` or
    /// `<record id>/<block id>`) for display.
    pub fn read_artifact(&self, id: &str) -> Result<String, ApplicationError> {
        let artifacts = {
            let state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            let runtime = state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .ok_or(ApplicationError::CompactionUnavailable)?;
            runtime
                .artifacts()
                .ok_or(ApplicationError::CompactionUnavailable)?
        };
        let bytes = artifacts
            .read_blocking(id, None)
            .map_err(|error| ApplicationError::Store(Arc::new(error)))?;
        String::from_utf8(bytes).map_err(|_| {
            ApplicationError::Store(Arc::new(crate::store::StoreError::InvalidHistory(format!(
                "artifact {id} is not valid UTF-8 text"
            ))))
        })
    }

    /// Override the compaction runtime's wall-clock source for deterministic tests.
    #[cfg(test)]
    pub(crate) fn set_compaction_clock_for_test(&self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) {
        let state = self.entry.lock();
        if let Some(runtime) = state.owner.as_ref().and_then(|owner| owner.compaction()) {
            runtime.set_clock(clock);
        }
    }

    /// History rewrite records written by this session in this process.
    pub fn written_history_records(&self) -> Vec<Arc<crate::compact::HistoryRecord>> {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .and_then(|runtime| runtime.history())
            .map(|history| history.written_records())
            .unwrap_or_default()
    }

    /// Number of record files this session has read.
    #[cfg(test)]
    pub(crate) fn record_reads(&self) -> usize {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .and_then(|runtime| runtime.history())
            .map(|history| history.read_count())
            .unwrap_or(0)
    }

    /// Number of spilled artifact files this session has read.
    #[cfg(test)]
    pub(crate) fn artifact_reads(&self) -> usize {
        let state = self.entry.lock();
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.compaction())
            .and_then(|runtime| runtime.history())
            .map(|history| history.artifact_read_count())
            .unwrap_or(0)
    }

    /// Expand the current Context into the full conversation.
    ///
    /// Record files are read only for the inserted runs the expansion reaches;
    /// with `last_n` only the newest N blocks are produced. Unreadable records
    /// are reported in the result, not as an error.
    pub fn expanded_conversation(
        &self,
        last_n: Option<usize>,
    ) -> Result<crate::compact::ExpandedConversation, ApplicationError> {
        let snapshot = self.read_materials()?;
        let history = {
            let state = self.entry.lock();
            state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .and_then(|runtime| runtime.history())
        };
        Ok(crate::compact::expand_conversation(
            snapshot.context.blocks(),
            &snapshot.history,
            last_n,
            |id| match &history {
                Some(history) => history.read(id),
                None => Err(crate::store::StoreError::InvalidHistory(
                    "this session keeps no history records".into(),
                )),
            },
        ))
    }

    /// Replace the current materials with a model-written brief and save it.
    ///
    /// When the brief chain is then over its budget it is merged too, as a
    /// second record.
    ///
    /// Refused while the session is busy or being saved, and while interrupted
    /// material still needs repair. A dropped future releases the compaction
    /// barrier; a record file written before the save failed is cleaned up on the next open.
    pub async fn compact(&self) -> Result<CompactionOutcome, ApplicationError> {
        let (runtime, version, context) = {
            let mut state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            if state.lifecycle == Lifecycle::Closing {
                return Err(ApplicationError::Closing);
            }
            self.entry.new_mutation(&state)?;
            let runtime = state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .ok_or(ApplicationError::CompactionUnavailable)?;
            let owner = state.owner.as_mut().expect("published owner");
            let history = owner.history_entries();
            let artifacts = owner.compaction().and_then(|c| c.history());
            let readiness = owner.with_context(|context| {
                self.entry.cloned_snapshot();
                super::repair::admission_check(context, &history, artifacts.as_deref())
            });
            match readiness {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error.into()),
                Err(error) => return Err(error.into()),
            }
            let version = self.entry.version(&state);
            let context = state
                .owner
                .as_mut()
                .expect("published owner")
                .with_context(|context| context.clone())?;
            state.compacting = true;
            self.entry.changed();
            (runtime, version, context)
        };
        let guard = CompactingGuard { entry: &self.entry };
        let control = CallControl::new(CancellationToken::new(), None);
        let rewrite = match runtime.manual_plan(&context, &control).await {
            Ok(rewrite) => rewrite,
            Err(reason) => {
                return Err(ApplicationError::Compaction(reason.name().to_owned()));
            }
        };
        let merge = runtime
            .manual_merge_plan(&rewrite, &context, &control)
            .await;
        let mut state = self.entry.lock();
        if state.lifecycle != Lifecycle::Open || state.closing.is_some() {
            return Err(if state.lifecycle == Lifecycle::Closed {
                ApplicationError::Closed
            } else {
                ApplicationError::Closing
            });
        }
        if self.entry.version(&state) != version {
            return Err(ApplicationError::StaleMaterial);
        }
        let (trigger, estimate_before, mut estimate_after) = (
            rewrite.trigger,
            rewrite.estimate_before,
            rewrite.estimate_after,
        );
        let mut candidate = context;
        let record = match runtime.commit_rewrite(&mut candidate, rewrite) {
            Ok(entry) => entry,
            Err(reason) => {
                return Err(ApplicationError::Compaction(reason.name().to_owned()));
            }
        };
        // A merge that cannot be committed leaves the summary on its own.
        let merged = merge.and_then(|merge| {
            let after = merge.estimate_after;
            let entry = runtime.commit_rewrite(&mut candidate, merge).ok()?;
            estimate_after = after;
            Some(entry)
        });
        let brief = crate::compact::segment::chain_briefs(candidate.blocks())
            .last()
            .map(|block| block.id())
            .expect("a committed summary leaves a brief in the chain");
        if let Err(error) = self.commit_materials(&mut state, candidate) {
            if let Some(merged) = &merged {
                runtime.rollback_record(&merged.record_id);
            }
            runtime.rollback_record(&record.record_id);
            return Err(error);
        }
        drop(state);
        drop(guard);
        Ok(CompactionOutcome {
            trigger,
            estimate_before,
            estimate_after,
            record,
            merged,
            brief,
        })
    }

    /// Forget a terminal record after its associated persistence barrier is handled.
    pub fn forget(&self, work: &WorkRef) -> Result<(), ApplicationError> {
        let mut state = self.entry.lock();
        if state.lifecycle == Lifecycle::Closed {
            return Err(ApplicationError::Closed);
        }
        if state.active.as_ref() == Some(work) {
            return Err(ApplicationError::SavePending);
        }
        state
            .owner
            .as_mut()
            .ok_or(ApplicationError::InstanceGone)?
            .forget(work)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Multi-agent coordination, observability, and per-agent material APIs
    // -----------------------------------------------------------------------

    fn coordinator(
        &self,
    ) -> Result<&Arc<super::coordinator::SessionCoordinator>, CoordinatorError> {
        self.entry
            .coordinator
            .get()
            .ok_or(CoordinatorError::Application(
                ApplicationError::InstanceGone,
            ))
    }

    /// Subscribe to Plane 2 non-blocking observability events for this session.
    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<Arc<SessionEvent>> {
        match self.entry.coordinator.get() {
            Some(coord) => coord.subscribe_events(),
            None => {
                let (tx, rx) = tokio::sync::broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }

    /// Return point-in-time read-only snapshots of all registered agents in this session.
    pub fn list_agents(&self) -> Vec<AgentSnapshot> {
        self.entry
            .coordinator
            .get()
            .map_or_else(Vec::new, |coord| coord.list_agents())
    }

    /// Return a point-in-time read-only snapshot for a single agent in this session.
    pub fn agent_snapshot(&self, agent_id: &str) -> Option<AgentSnapshot> {
        self.entry
            .coordinator
            .get()
            .and_then(|coord| coord.agent_snapshot(agent_id))
    }

    /// Number of currently active `Finite` background tasks in this session.
    pub fn active_finite_background_tasks(&self) -> usize {
        self.entry
            .coordinator
            .get()
            .map_or(0, |coord| coord.active_finite_background_tasks())
    }

    /// Return a deterministic snapshot of all registered background tasks in this session.
    pub fn background_task_snapshot(&self) -> Vec<BackgroundTaskSnapshot> {
        self.entry
            .coordinator
            .get()
            .map_or_else(Vec::new, |coord| coord.background_task_snapshot())
    }

    /// Suspend until all active `Finite` background tasks in this session have settled.
    pub async fn wait_background_tasks_settled(&self) {
        if let Some(coord) = self.entry.coordinator.get() {
            coord.wait_background_tasks_settled().await;
        }
    }

    /// Cooperatively cancel any active root work and all active child/background operations,
    /// suppressing pending idle wakeups to prevent zombie turns.
    pub fn cancel_all_active(&self) {
        let (handle, active, coord_cancel) = {
            let entry_state = self.entry.lock();
            let handle = entry_state.handle.clone();
            let active = entry_state.active.clone();
            let coord_cancel = self
                .entry
                .coordinator
                .get()
                .map(|coord| (coord, coord.collect_cancel_all_active()));
            (handle, active, coord_cancel)
        };
        if let (Some(handle), Some(work)) = (handle, active) {
            let _ = handle.cancel(&work);
        }
        if let Some((coord, (tokens, suppress_ids))) = coord_cancel {
            coord.apply_cancel_all_active(tokens, suppress_ids);
        }
    }

    /// Return `true` if root execution, any finite background task, active child operation,
    /// in-flight admission, or eligible idle wakeup is still active (as opposed to only
    /// persistence/saving remaining).
    pub fn has_active_execution(&self) -> bool {
        let mut entry_state = self.entry.lock();
        if entry_state.active.is_some()
            && entry_state.pending.is_none()
            && entry_state.fault.is_none()
        {
            return true;
        }
        self.entry
            .coordinator
            .get()
            .is_some_and(|coord| coord.has_active_execution_with_entry(&mut entry_state))
    }

    /// Wait with zero polling until root execution, persistence, all `Finite` background
    /// tasks, child saves/outboxes, in-flight admissions, and eligible idle wakeups settle.
    ///
    /// Returns the last active root [`WorkRef`] observed during this wait (including an
    /// active root work already running when `wait_settled` was called, and any reactive
    /// wakeup work submitted while settling), or `None` if root was already idle and no
    /// reactive wakeup occurred.
    pub async fn wait_settled(&self) -> Result<Option<WorkRef>, ApplicationError> {
        let (start_seq, had_initial_active) = {
            let entry_state = self.entry.lock();
            (entry_state.root_work_seq, entry_state.active.is_some())
        };
        self.wait_settled_with(|entry_state| {
            if entry_state.root_work_seq > start_seq || had_initial_active {
                entry_state.last_root_work.clone()
            } else {
                None
            }
        })
        .await
    }

    /// Wait with zero polling until all root/child execution and persistence settle,
    /// returning the final root [`WorkRef`] if a new reactive wakeup occurred after `baseline`.
    ///
    /// Unlike [`Self::wait_settled`], this compares against `baseline` so a reactive wakeup
    /// that started and finished before `wait_settled_after` was polled is never missed.
    pub async fn wait_settled_after(
        &self,
        baseline: &WorkRef,
    ) -> Result<Option<WorkRef>, ApplicationError> {
        self.wait_settled_with(|entry_state| {
            entry_state
                .last_root_work
                .as_ref()
                .filter(|w| *w != baseline)
                .cloned()
        })
        .await
    }

    async fn wait_settled_with(
        &self,
        mut pick_result: impl FnMut(&super::entry::State) -> Option<WorkRef>,
    ) -> Result<Option<WorkRef>, ApplicationError> {
        let coord = self.entry.coordinator.get();
        loop {
            let entry_changed = self.entry.changed.notified();
            tokio::pin!(entry_changed);
            entry_changed.as_mut().enable();

            let coord_changed = coord.map(|c| c.state_changed.notified());
            tokio::pin!(coord_changed);
            if let Some(fut) = coord_changed.as_mut().as_pin_mut() {
                fut.enable();
            }

            {
                let mut entry_state = self.entry.lock();
                if let Some(error) = &entry_state.fault {
                    return Err(error.clone());
                }
                if let SaveStatus::SaveBlocked { error, .. } = &entry_state.save {
                    return Err(ApplicationError::SaveBlocked(error.clone()));
                }
                let root_settled = entry_state.active.is_none()
                    && entry_state.pending.is_none()
                    && !entry_state.save_requested;
                let coord_settled = match coord {
                    Some(c) => c.check_settled_with_entry(&mut entry_state)?,
                    None => true,
                };
                if matches!(
                    entry_state.lifecycle,
                    Lifecycle::Closed | Lifecycle::Closing
                ) || (root_settled && coord_settled)
                {
                    return Ok(pick_result(&entry_state));
                }
            }

            match coord_changed.as_pin_mut() {
                Some(coord_notified) => {
                    tokio::select! {
                        _ = entry_changed => {}
                        _ = coord_notified => {}
                    }
                }
                None => entry_changed.await,
            }
        }
    }

    /// Spawn a new narrowed child agent and admit its initial operation.
    pub async fn spawn_agent(
        &self,
        request: SpawnAgentRequest,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.coordinator()?.spawn_agent_operation(request).await
    }

    /// Send follow-up instructions to an existing `Reusable` idle agent.
    pub async fn message_agent(
        &self,
        request: MessageAgentRequest,
    ) -> Result<SpawnAgentOutcome, CoordinatorError> {
        self.coordinator()?.message_agent_operation(request).await
    }

    /// Read-only paginated output slice for a fixed `operation_id`.
    pub async fn read_agent(
        &self,
        operation_id: &str,
        offset_bytes: usize,
        limit_bytes: usize,
    ) -> Result<ReadAgentResult, CoordinatorError> {
        self.coordinator()?
            .read_agent(operation_id, offset_bytes, limit_bytes)
            .await
    }

    /// Cooperatively cancel `operation_id` (and all active descendants) and await settlement.
    pub async fn cancel_agent(
        &self,
        operation_id: &str,
    ) -> Result<OperationOutcome, CoordinatorError> {
        self.coordinator()?.cancel_agent(operation_id).await
    }

    /// Cooperatively cancel an active child operation (`operation_id`) and suppress its idle wakeup.
    ///
    /// Returns `Ok(true)` if the operation was active and cancelled, `Ok(false)` if the operation
    /// exists in this session but is already terminal, or `Err(ApplicationError::...)` if the
    /// session is closed/invalid or the operation ID is unknown.
    pub async fn cancel_agent_operation(
        &self,
        operation_id: &str,
    ) -> Result<bool, ApplicationError> {
        let coord = {
            let state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            if state.lifecycle == Lifecycle::Closing {
                return Err(ApplicationError::Closing);
            }
            if state.owner.is_none() {
                return Err(ApplicationError::InstanceGone);
            }
            self.entry
                .coordinator
                .get()
                .cloned()
                .ok_or(ApplicationError::InstanceGone)?
        };
        coord
            .cancel_operation(operation_id)
            .await
            .map_err(|err| match err {
                CoordinatorError::Application(app_err) => app_err,
                CoordinatorError::UnknownOperation(id) => ApplicationError::UnknownOperation(id),
                CoordinatorError::Store(store_err) => ApplicationError::Store(store_err),
                other => ApplicationError::Faulted(other.to_string()),
            })
    }

    /// Read idle materials for a specific agent scoped by `agent_id`.
    pub fn read_agent_materials(
        &self,
        agent_id: &str,
    ) -> Result<AgentMaterialSnapshot, CoordinatorError> {
        self.coordinator()?.read_agent_materials(agent_id)
    }

    /// Atomically replace an idle agent's Context blocks bound to `(SessionTarget, agent_id, generation)`.
    pub fn edit_agent_materials(
        &self,
        base: AgentMaterialVersion,
        blocks: Vec<ContextBlock>,
    ) -> Result<AgentMaterialVersion, CoordinatorError> {
        self.coordinator()?.edit_agent_materials(base, blocks)
    }

    /// Inspect an agent's idle materials for repair issues without mutating Context.
    pub fn inspect_agent_repair(
        &self,
        agent_id: &str,
        work: Option<&WorkRef>,
    ) -> Result<AgentRepairInspection, CoordinatorError> {
        self.coordinator()?.inspect_agent_repair(agent_id, work)
    }

    /// Prepare a validated repair candidate strictly bound to `(SessionTarget, agent_id, generation)`.
    pub fn prepare_agent_repair(
        &self,
        agent_id: &str,
        mode: super::RepairMode,
    ) -> Result<AgentRepairPlan, CoordinatorError> {
        self.coordinator()?.prepare_agent_repair(agent_id, mode)
    }

    /// Apply a previously prepared [`AgentRepairPlan`] to its exact target agent and generation.
    pub fn apply_agent_repair(
        &self,
        plan: AgentRepairPlan,
    ) -> Result<AgentMaterialVersion, CoordinatorError> {
        self.coordinator()?.apply_agent_repair(plan)
    }

    /// Override coordinator configuration during unit and integration tests.
    #[cfg(test)]
    pub(crate) fn set_coordinator_config(&self, config: CoordinatorConfig) {
        if let Some(coord) = self.entry.coordinator.get() {
            coord.set_config(config);
        }
    }
}

/// Clears the compaction barrier even when the caller drops the future.
struct CompactingGuard<'a> {
    entry: &'a Entry,
}

impl Drop for CompactingGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.entry.lock();
        state.compacting = false;
        self.entry.changed();
    }
}
