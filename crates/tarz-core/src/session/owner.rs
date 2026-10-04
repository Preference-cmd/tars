use super::handle::SessionHandle;
use super::state::{Shared, State};
use super::types::*;
use causa::kernel::{Context, ToolBatch, TurnId};
use causa::runtime::{RunControl, TurnRunOptions, TurnRunner};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Exclusive lifecycle owner for one conversation's current materials.
pub struct Session {
    pub(super) shared: Arc<Shared>,
}

impl Session {
    /// Construct without assembly, worker creation, input edits or history recovery.
    pub fn new<F, Fut>(
        conversation_id: ConversationId,
        context: Context,
        assemble: F,
        config: SessionConfig,
    ) -> Self
    where
        F: Fn(WorkRef, RunControl) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(Arc<TurnRunner>, TurnRunOptions), Box<dyn Error + Send + Sync>>>
            + Send
            + 'static,
    {
        Self {
            shared: Arc::new(Shared {
                conversation_id,
                state: Mutex::new(State {
                    context: Some(context),
                    active: None,
                    closed: false,
                    faulted: false,
                    records: HashMap::new(),
                    keys: HashMap::new(),
                }),
                assemble: Arc::new(move |work, control| Box::pin(assemble(work, control))),
                config,
                candidate: Arc::new(|| TurnId::new(uuid::Uuid::now_v7().to_string())),
                progress: tokio::sync::broadcast::channel(super::progress::PROGRESS_CAPACITY).0,
            }),
        }
    }

    /// Clone a handle with submission and retained work access only.
    pub fn handle(&self) -> SessionHandle {
        SessionHandle {
            shared: self.shared.clone(),
        }
    }

    /// Access idle materials under the admission lock.
    ///
    /// The callback must be short, synchronous and non-reentrant. Successful edits
    /// are not rolled back by its return value or a panic. A panic poisons the
    /// session and prevents fresh execution. Closure captures may be dropped even
    /// when this method rejects access without invoking the callback.
    pub fn with_context<R>(
        &mut self,
        f: impl FnOnce(&mut Context) -> R,
    ) -> Result<R, SessionError> {
        let mut state = self.shared.lock();
        if let Some(work) = &state.active {
            return Err(SessionError::Busy { work: work.clone() });
        }
        let context = state
            .context
            .as_mut()
            .ok_or(SessionError::ContextUnavailable)?;
        // Preserve ContextUnavailable as the precise lost-worker diagnosis.
        if self.shared.state.is_poisoned() {
            return Err(SessionError::Faulted);
        }
        Ok(f(context))
    }

    /// Claim the original uncommitted batch once, independently of result Arc counts.
    pub fn take_uncommitted_tool_batch(
        &mut self,
        work: &WorkRef,
    ) -> Result<Option<ToolBatch>, SessionError> {
        let mut state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        if !matches!(record.snapshot().state, WorkState::Finished(_)) {
            return Err(SessionError::NotFinished { work: work.clone() });
        }
        Ok(state
            .records
            .get_mut(&work.turn_id)
            .expect("record located")
            .uncommitted
            .take())
    }

    /// Explicitly release a terminal record, its key, original input and unclaimed batch.
    ///
    /// Captured observations and waits retain the original terminal value; a later
    /// submission with this key is independent and cannot rebind them.
    pub fn forget(&mut self, work: &WorkRef) -> Result<(), SessionError> {
        let mut state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        if !matches!(record.snapshot().state, WorkState::Finished(_)) {
            return Err(SessionError::NotFinished { work: work.clone() });
        }
        let entry = state.records.remove(&work.turn_id).expect("record located");
        state.keys.remove(&entry.request_key);
        drop(state);
        // Error payloads may have application-defined destructors.
        drop(entry);
        Ok(())
    }

    /// Close new admission, cooperatively cancel active work and await supervision.
    ///
    /// Retained keys still replay. This does not forget records or discard returned
    /// materials. It requires the runtime to remain alive for the worker to finish.
    pub async fn shutdown(&mut self) {
        let active = {
            let mut state = self.shared.lock();
            state.closed = true;
            state
                .active
                .as_ref()
                .and_then(|work| self.shared.record(&state, work).ok())
        };
        if let Some(record) = active {
            record.control.cancellation_token().cancel();
            record.wait_finished().await;
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let mut state = self.shared.lock();
        state.closed = true;
        if let Some(work) = &state.active
            && let Ok(record) = self.shared.record(&state, work)
        {
            record.control.cancellation_token().cancel();
        }
    }
}

#[cfg(test)]
impl Session {
    pub(super) fn set_candidate(&mut self, candidate: impl Fn() -> TurnId + Send + Sync + 'static) {
        Arc::get_mut(&mut self.shared)
            .expect("inject before handles")
            .candidate = Arc::new(candidate);
    }
}
