use super::entry::{Entry, State};
use super::host::HostInner;
use super::types::*;
use crate::store::{CommitResolution, PreparedCommit, SavedSessionRecord};
use std::sync::Arc;

pub(super) fn prepare(entry: &Entry, state: &mut State) -> Result<(), ApplicationError> {
    let context = state
        .owner
        .as_mut()
        .expect("assembled owner")
        .with_context(|context| {
            entry.cloned_snapshot();
            context.clone()
        })?;
    let record = SavedSessionRecord::new(
        entry.target.conversation_id.as_str().to_owned(),
        uuid::Uuid::now_v7().to_string(),
        state.revision,
        state.definition.clone().expect("assembled definition"),
        context,
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
