use super::entry::Entry;
use super::host::HostInner;
use super::saving;
use super::types::*;
use crate::store::CommitResolution;
use std::sync::Arc;

pub(super) async fn finish(host: &Arc<HostInner>, entry: &Arc<Entry>, policy: ClosePolicy) -> bool {
    let confirm = {
        let state = entry.lock();
        if state.closing != Some(policy) {
            return false;
        }
        if policy == ClosePolicy::DiscardLocal && state.pending_attempted {
            state.pending.clone().zip(state.writer.clone())
        } else {
            None
        }
    };
    // Confirmation never retries a write. The original commit remains fixed,
    // and the guard remains held even if all close waiters abandon this call.
    let disposition = if let Some((pending, writer)) = confirm {
        Some(
            match saving::io(host, move || writer.resolve_commit(&pending)).await {
                Ok(CommitResolution::Applied(record)) => CommitDisposition::Applied {
                    revision: record.revision,
                },
                Ok(CommitResolution::NotApplied) => CommitDisposition::NotApplied,
                Err(error) => CommitDisposition::Unresolved { error },
            },
        )
    } else {
        let state = entry.lock();
        state
            .pending
            .as_ref()
            .map(|_| CommitDisposition::NotSubmitted)
    };
    // Release MCP connections before publishing Closed. The owner is removed
    // from the lock scope so no application lock is held across the wait; a
    // policy replacement while we wait leaves the rest to the next attempt.
    let mcp = {
        let entries = host.entries.lock().expect("directory has no callbacks");
        let mut state = entry.lock();
        if state.closing != Some(policy) {
            return false;
        }
        if policy == ClosePolicy::Save && state.persistent {
            let error = if state.fault.is_some() {
                state.fault.clone()
            } else if state.pending.is_some() {
                state.error.clone().or(Some(ApplicationError::SavePending))
            } else if !matches!(state.save, SaveStatus::Saved { .. }) {
                state.error.clone().or_else(|| {
                    Some(ApplicationError::Faulted(
                        "latest materials are not confirmed".into(),
                    ))
                })
            } else {
                state
                    .owner
                    .as_mut()
                    .and_then(|owner| owner.with_context(|_| ()).err())
                    .map(Into::into)
            };
            if let Some(error) = error {
                state.error = Some(error);
                state.close_in_flight = false;
                entry.changed();
                return false;
            }
        }
        let mcp = state.owner.as_mut().and_then(|owner| owner.take_mcp());
        drop(state);
        drop(entries);
        mcp
    };
    if let Some(mcp) = mcp {
        // Close failures are not a close outcome; the transport is gone either
        // way and the session state must still be published.
        mcp.close().await;
    }
    let mut entries = host.entries.lock().expect("directory has no callbacks");
    let mut state = entry.lock();
    // Final external-host disposal may replace an abandoned Save with Discard.
    // A stale continuation must not clear that cleanup request or its authority.
    if state.closing != Some(policy) {
        return false;
    }
    if let Some(CommitDisposition::Applied { revision }) = &disposition {
        state.revision = Some(*revision);
        state.save = SaveStatus::Saved {
            revision: *revision,
            material_version: entry.version(&state),
        };
    }
    state.close_outcome = Some(CloseOutcome {
        policy,
        save: state.save.clone(),
        pending_commit: disposition,
        discarded_unconfirmed: policy == ClosePolicy::DiscardLocal
            && (!matches!(
                state.save,
                SaveStatus::Saved { .. } | SaveStatus::NotTracked
            ) || state.fault.is_some()),
    });
    state.lifecycle = Lifecycle::Closed;
    state.close_in_flight = false;
    state.active = None;
    state.pending = None;
    state.save_requested = false;
    state.owner = None;
    state.writer = None;
    // Publish terminal state and remove the old routing slot under the same
    // directory -> entry lock order as close/claim/register. Successful close
    // therefore permits an immediate reopen without joining the old Closed slot.
    if entries
        .get(&entry.target.conversation_id)
        .is_some_and(|current| current.target == entry.target)
    {
        entries.remove(&entry.target.conversation_id);
    }
    drop(state);
    drop(entries);
    entry.changed();
    true
}
