use super::entry::{Entry, State};
use super::host::HostInner;
use super::types::*;
use crate::store::{CommitResolution, PreparedCommit, SavedSessionRecord};
use std::sync::Arc;

/// Running compaction values and history index for the next record.
pub(super) fn compaction_record(
    state: &State,
) -> (
    Option<bool>,
    Option<u8>,
    Vec<crate::compact::HistoryRecordEntry>,
) {
    let Some(runtime) = state.owner.as_ref().and_then(|owner| owner.compaction()) else {
        return (None, None, Vec::new());
    };
    let history = runtime.entries();
    let Some(definition) = state.definition.as_ref() else {
        return (None, None, history);
    };
    let (auto_compact, compact_threshold) = runtime.record_overrides(definition);
    (auto_compact, compact_threshold, history)
}

/// Title and last-activity time for the next record over `context`. A
/// prepared commit it replaces may already be stored, so its time is a floor.
pub(super) fn next_metadata(
    state: &State,
    context: &causa::kernel::Context,
) -> (Option<String>, u64) {
    let floor = state
        .pending
        .as_ref()
        .map_or(0, |pending| pending.record.updated_at_ms);
    state.saved.next(context, floor)
}

pub(super) fn prepare(entry: &Entry, state: &mut State) -> Result<(), ApplicationError> {
    let context = state
        .owner
        .as_mut()
        .expect("assembled owner")
        .with_context(|context| {
            entry.cloned_snapshot();
            context.clone()
        })?;
    let current_mode = state
        .owner
        .as_ref()
        .and_then(|o| o.permission_mode().map(|(m, _)| m));
    let (auto_compact, compact_threshold, history) = compaction_record(state);
    let usage = state.owner.as_ref().map(|o| o.session_usage());
    let (title, updated_at_ms) = next_metadata(state, &context);
    let record = SavedSessionRecord::new_with_compaction(
        entry.target.conversation_id.as_str().to_owned(),
        uuid::Uuid::now_v7().to_string(),
        state.revision,
        state.definition.clone().expect("assembled definition"),
        context,
        current_mode,
        auto_compact,
        compact_threshold,
        history,
        usage,
        title,
        updated_at_ms,
    )?;
    state.pending = Some(Arc::new(PreparedCommit::new(state.revision, record)?));
    state.save_requested = true;
    state.pending_attempted = false;
    state.save = SaveStatus::Dirty {
        base_revision: state.revision,
        material_version: entry.version(state),
    };
    Ok(())
}

pub(super) async fn save(host: &Arc<HostInner>, entry: &Arc<Entry>) {
    let (writer, pending, attempted) = {
        let mut state = entry.lock();
        state.save_requested = false;
        let attempted = state.pending_attempted;
        state.pending_attempted = true;
        let pending = state.pending.clone().expect("pending save");
        state.save = SaveStatus::Saving {
            commit_id: pending.record.last_commit_id.clone(),
            base_revision: pending.expected_revision,
            material_version: entry.version(&state),
        };
        entry.changed();
        (
            state.writer.clone().expect("persistent writer"),
            pending,
            attempted,
        )
    };
    // Both the supervisor and blocking closure own the writer. Dropping a wait
    // cannot release the OS lock while serialization/replacement may still run.
    let result = io(host, move || {
        if attempted {
            match writer.resolve_commit(&pending)? {
                CommitResolution::Applied(record) => return Ok(*record),
                CommitResolution::NotApplied => {}
            }
        }
        writer.commit(&pending)
    })
    .await;
    let mut state = entry.lock();
    match result {
        Ok(record) => {
            state.revision = Some(record.revision);
            state.saved = crate::store::RecordMetadata::of(&record);
            state.pending = None;
            state.save_requested = false;
            state.save = SaveStatus::Saved {
                revision: record.revision,
                material_version: entry.version(&state),
            };
            state.active = None;
            state.error = None;
            state.initialized = true;
            if state.closing.is_none() {
                state.lifecycle = Lifecycle::Open;
            }
            if state.persistent {
                let initial_mode = state
                    .definition
                    .as_ref()
                    .map(crate::definition::SessionDefinition::initial_permission_mode)
                    .unwrap_or(crate::permission::PermissionMode::Ask);
                let committed_mode = record.permission_mode.unwrap_or(initial_mode);
                let current_mode = state
                    .owner
                    .as_ref()
                    .and_then(|o| o.permission_mode())
                    .map(|(m, _)| m)
                    .unwrap_or(initial_mode);
                if current_mode != committed_mode
                    && state.active.is_none()
                    && state.closing != Some(ClosePolicy::DiscardLocal)
                {
                    if let Err(error) = prepare(entry, &mut state) {
                        state.lifecycle = Lifecycle::Faulted;
                        state.error = Some(error);
                        state.fault = state.error.clone();
                    } else {
                        entry.wake.notify_one();
                    }
                }
                // A running setting changed while this commit was in flight;
                // the next fixed snapshot carries it, like the permission mode.
                if let Some(definition) = state.definition.clone()
                    && let Some(runtime) = state.owner.as_ref().and_then(|o| o.compaction())
                {
                    let (auto_compact, compact_threshold) = runtime.settings();
                    let committed_auto = record.auto_compact.unwrap_or(definition.auto_compact);
                    let committed_threshold = record
                        .compact_threshold
                        .unwrap_or(definition.compact_threshold);
                    if (auto_compact != committed_auto || compact_threshold != committed_threshold)
                        && state.active.is_none()
                        && state.closing != Some(ClosePolicy::DiscardLocal)
                    {
                        if let Err(error) = prepare(entry, &mut state) {
                            state.lifecycle = Lifecycle::Faulted;
                            state.error = Some(error);
                            state.fault = state.error.clone();
                        } else {
                            entry.wake.notify_one();
                        }
                    }
                }
            }
        }
        Err(error) => {
            let pending = state.pending.as_ref().expect("failed save retained");
            let error = match error {
                ApplicationError::Store(error) => error,
                other => {
                    state.lifecycle = Lifecycle::Faulted;
                    state.error = Some(other);
                    state.fault = state.error.clone();
                    state.close_in_flight = state.closing == Some(ClosePolicy::DiscardLocal);
                    entry.changed();
                    return;
                }
            };
            state.save = SaveStatus::SaveBlocked {
                error: error.clone(),
                commit_id: pending.record.last_commit_id.clone(),
                material_version: entry.version(&state),
            };
            state.error = Some(ApplicationError::SaveBlocked(error));
            if state.closing == Some(ClosePolicy::Save) {
                state.close_in_flight = false;
            } else if !state.initialized {
                state.lifecycle = Lifecycle::InitializationBlocked;
            }
        }
    }
    entry.changed();
}

pub(super) async fn io<T: Send + 'static>(
    host: &Arc<HostInner>,
    operation: impl FnOnce() -> Result<T, crate::store::StoreError> + Send + 'static,
) -> Result<T, ApplicationError> {
    let permit = host
        .io
        .clone()
        .acquire_owned()
        .await
        .expect("host semaphore remains open");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|error| ApplicationError::Faulted(format!("storage task failed: {error}")))?
    .map_err(Into::into)
}
