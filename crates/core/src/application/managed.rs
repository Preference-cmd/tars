use super::entry::Entry;
use super::saving;
use super::types::*;
use crate::compact::{CompactionOutcome, CompactionRuntime, HistoryRecordEntry};
use crate::definition::SessionDefinition;
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
        state
            .owner
            .as_ref()
            .and_then(|owner| owner.permission_mode())
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
        let owner = state.owner.as_ref().ok_or(ApplicationError::InstanceGone)?;
        let prev_mode = owner.permission_mode().map(|(m, _)| m);
        let (new_mode, rev) = owner.set_permission_mode(mode)?;
        if prev_mode != Some(new_mode)
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
        state.owner.as_ref().map_or_else(
            || {
                let (_, rx) = tokio::sync::mpsc::channel(1);
                rx
            },
            |owner| owner.connect_approvals(),
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

    /// Append an extra before processor between the mode gate and precheck/permission processors during test runs.
    #[cfg(test)]
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn add_test_intermediate_processor(
        &self,
        processor: Arc<dyn causa::kernel::ToolBatchProcessor>,
    ) {
        let state = self.entry.lock();
        if let Some(owner) = &state.owner {
            owner.add_test_intermediate_processor(processor);
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
    /// Facts learned while this instance was opened.
    pub fn open_report(&self) -> OpenReport {
        self.entry.lock().open_report
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
        let readiness = state
            .owner
            .as_mut()
            .expect("published owner")
            .with_context(|context| super::repair::admission_check(context));
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
        let receipt = handle
            .submit(request)
            .map_err(|failure| ManagedSubmitRejection {
                reason: failure.reason.into(),
                request: failure.request,
            })?;
        // The supervisor already exists. This synchronous registration holds the
        // application gate until it captures and confirms this work's materials.
        state.active = Some(receipt.work.clone());
        if state.persistent {
            state.save = SaveStatus::Dirty {
                base_revision: state.revision,
                material_version: self.entry.version(&state),
            };
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
        self.entry
            .lock()
            .handle
            .as_ref()
            .expect("published handle")
            .cancel(work)
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
        let history = owner
            .compaction()
            .map(|runtime| runtime.entries())
            .unwrap_or_default();
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
        let current_mode = state
            .owner
            .as_ref()
            .and_then(|o| o.permission_mode().map(|(m, _)| m));
        let pending = if state.persistent {
            self.entry.cloned_snapshot();
            let (auto_compact, compact_threshold, history) = saving::compaction_record(state);
            let usage = state.owner.as_ref().map(|o| o.session_usage());
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
            )?;
            Some(Arc::new(PreparedCommit::new(state.revision, record)?))
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
        Ok(self.entry.version(state))
    }
    /// Wait for this instance's current execution/save barrier to settle.
    pub async fn wait_saved(&self) -> Result<SaveStatus, ApplicationError> {
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
    /// Confirm or retry the same immutable pending commit; concurrent retries join.
    pub async fn retry_save(&self) -> Result<SaveStatus, ApplicationError> {
        {
            let mut state = self.entry.lock();
            if state.lifecycle == Lifecycle::Closed {
                return Err(ApplicationError::Closed);
            }
            if state.pending.is_some() && matches!(state.save, SaveStatus::SaveBlocked { .. }) {
                state.save_requested = true;
                state.error = None;
                if state.closing.is_some() {
                    state.close_in_flight = true;
                }
                self.entry.notify();
            }
        }
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
        let history = {
            let state = self.entry.lock();
            let runtime = state
                .owner
                .as_ref()
                .and_then(|owner| owner.compaction())
                .ok_or(ApplicationError::CompactionUnavailable)?;
            runtime
                .history()
                .ok_or(ApplicationError::CompactionUnavailable)?
        };
        history
            .read(record_id)
            .map_err(|error| ApplicationError::Store(Arc::new(error)))
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
            let readiness =
                state
                    .owner
                    .as_mut()
                    .expect("published owner")
                    .with_context(|context| {
                        self.entry.cloned_snapshot();
                        super::repair::admission_check(context)
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
        let (trigger, estimate_before, estimate_after) = (
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
        if let Err(error) = self.commit_materials(&mut state, candidate) {
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
