use super::state::{Entry, Record, Shared};
use super::types::*;
use super::worker;
use causa::runtime::RunControl;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Cloneable admission and retained-work query access without material ownership.
#[derive(Clone)]
pub struct SessionHandle {
    pub(super) shared: Arc<Shared>,
}

impl SessionHandle {
    /// Atomically admit a new key or replay the exact retained input's stable receipt.
    ///
    /// New work needs a current Tokio runtime with time enabled. Every refusal
    /// returns the original request. After admission, failures appear only in the
    /// work result. Assembly and execution are never called under the lock.
    pub fn submit(&self, mut request: SubmitRequest) -> Result<WorkReceipt, SubmitRejection> {
        let mut state = self.shared.lock();
        if let Some(turn_id) = state.keys.get(&request.request_key) {
            let entry = state
                .records
                .get(turn_id)
                .expect("key indexes retained record");
            return if entry.blocks == request.blocks {
                Ok(WorkReceipt {
                    work: entry.record.work.clone(),
                })
            } else {
                Err(reject(
                    SessionError::Conflict {
                        work: entry.record.work.clone(),
                    },
                    request,
                ))
            };
        }
        let reason = if state.faulted {
            Some(SessionError::Faulted)
        } else if state.closed {
            Some(SessionError::Closed)
        } else if let Some(work) = &state.active {
            Some(SessionError::Busy { work: work.clone() })
        } else if state.records.len() >= self.shared.config.retained_work_capacity {
            Some(SessionError::Capacity {
                capacity: self.shared.config.retained_work_capacity,
            })
        } else if state.context.is_none() {
            Some(SessionError::ContextUnavailable)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(reject(reason, request));
        }
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(reason) => return Err(reject(reason, request)),
        };
        let accepted_at = Instant::now();
        let deadline = match self.shared.config.work_timeout {
            Some(timeout) => match accepted_at.checked_add(timeout) {
                Some(deadline) => Some(deadline),
                None => return Err(reject(SessionError::DeadlineUnrepresentable, request)),
            },
            None => None,
        };
        let turn_id = (self.shared.candidate)();
        if let Some(entry) = state.records.get(&turn_id) {
            return Err(reject(
                SessionError::IdentityCollision {
                    work: entry.record.work.clone(),
                },
                request,
            ));
        }
        let work = WorkRef {
            conversation_id: self.shared.conversation_id.clone(),
            turn_id,
        };
        let control = RunControl::new(CancellationToken::new(), deadline);
        let original_blocks = request.blocks.clone();
        let context = state.context.as_mut().expect("idle context checked");
        if let Err(failure) = context.edit().append(request.blocks).commit() {
            request.blocks = failure.appended;
            return Err(reject(SessionError::ContextEdit(failure.reason), request));
        }
        let record = Arc::new(Record {
            work: work.clone(),
            control,
            state: Mutex::new(WorkState::Accepted),
            changed: Notify::new(),
            #[cfg(test)]
            abort: Mutex::new(None),
            #[cfg(test)]
            before_publish: Mutex::new(None),
        });
        state
            .keys
            .insert(request.request_key.clone(), work.turn_id.clone());
        state.records.insert(
            work.turn_id.clone(),
            Entry {
                request_key: request.request_key,
                blocks: original_blocks,
                record: record.clone(),
                uncommitted: None,
            },
        );
        state.active = Some(work.clone());
        let context = state.context.take().expect("context transferred once");
        drop(state);
        worker::start(runtime, self.shared.clone(), record, context);
        Ok(WorkReceipt { work })
    }

    /// Read retained progress; references from other conversations are not found.
    pub fn observe(&self, work: &WorkRef) -> Result<WorkObservation, SessionError> {
        let state = self.shared.lock();
        Ok(self.shared.record(&state, work)?.snapshot())
    }

    /// Wait for terminal work or return its current snapshot at the finite timeout.
    ///
    /// This wait does not cancel work. A captured record survives owner forgetting;
    /// waits begun after forgetting return NotFound. Zero timeout observes immediately.
    pub async fn wait(
        &self,
        work: &WorkRef,
        timeout: Duration,
    ) -> Result<WorkObservation, SessionError> {
        let record = {
            let state = self.shared.lock();
            self.shared.record(&state, work)?
        };
        Ok(record.wait(timeout).await)
    }

    /// Request cooperative cancellation; completion and side effects must be observed.
    ///
    /// Repeated and late cancellation succeeds without rewriting a terminal result.
    pub fn cancel(&self, work: &WorkRef) -> Result<(), SessionError> {
        let state = self.shared.lock();
        let record = self.shared.record(&state, work)?;
        if !matches!(record.snapshot().state, WorkState::Finished(_)) {
            record.control.cancellation_token().cancel();
        }
        Ok(())
    }
}

fn reject(reason: SessionError, request: SubmitRequest) -> SubmitRejection {
    SubmitRejection { reason, request }
}

fn runtime() -> Result<Handle, SessionError> {
    let runtime = Handle::try_current().map_err(|_| SessionError::RuntimeUnavailable)?;
    // Tokio exposes no timer-driver query; constructing a timer checks availability
    // before inputs move. Catch only this documented runtime configuration panic.
    drop(
        std::panic::catch_unwind(|| tokio::time::sleep(Duration::ZERO))
            .map_err(|_| SessionError::RuntimeUnavailable)?,
    );
    Ok(runtime)
}
