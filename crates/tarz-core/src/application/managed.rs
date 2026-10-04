use super::entry::Entry;
use super::types::*;
use crate::definition::SessionDefinition;
use crate::session::{
    SessionError, SubmitRequest, WorkObservation, WorkProgress, WorkReceipt, WorkRef,
};
use crate::store::{PreparedCommit, SavedSessionRecord};
use causa::kernel::ContextBlock;
use std::sync::Arc;
use std::time::Duration;

/// Cloneable view permanently bound to one instance, without low-level mutation access.
#[derive(Clone)]
pub struct ManagedSession {
    pub(super) entry: Arc<Entry>,
}
impl ManagedSession {
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
        let context = state
            .owner
            .as_mut()
            .ok_or(ApplicationError::InstanceGone)?
            .with_context(|context| {
                self.entry.cloned_snapshot();
                context.clone()
            })?;
        Ok(MaterialSnapshot { version, context })
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
        let next = state
            .generation
            .checked_add(1)
            .ok_or_else(|| ApplicationError::Faulted("material generation exhausted".into()))?;
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
        let pending = if state.persistent {
            self.entry.cloned_snapshot();
            let record = SavedSessionRecord::new(
                self.entry.target.conversation_id.as_str().to_owned(),
                uuid::Uuid::now_v7().to_string(),
                state.revision,
                state.definition.clone().expect("definition"),
                candidate.clone(),
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
                material_version: self.entry.version(&state),
            };
        }
        self.entry.notify();
        Ok(self.entry.version(&state))
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
