use super::state::{Entry, Record, Shared};
use super::types::*;
use super::worker;
use causa::kernel::CallControl;
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
    /// Subscribe to temporary model observations from this exact session instance.
    /// Subscribe before submit; there is no replay. Capacity is 128 observations,
    /// with at most 16 KiB per text fragment. A slow receiver reports `Lagged`;
    /// work results and cancellation remain available through their reliable APIs.
    pub fn subscribe_progress(&self) -> tokio::sync::broadcast::Receiver<Arc<WorkProgress>> {
        self.shared.progress.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn hold_terminal(&self, work: &WorkRef, entered: Arc<Notify>, release: Arc<Notify>) {
        let state = self.shared.lock();
        let record = self.shared.record(&state, work).expect("test work");
        *record.before_publish.lock().expect("test hook") = Some((entered, release));
    }

    // Application admission and forgetting share an outer coordination lock.
    // Reuse the retained table, including exact original block identities.
    pub(crate) fn inspect_request(
        &self,
        request: &SubmitRequest,
    ) -> Option<Result<WorkReceipt, SessionError>> {
        let state = self.shared.lock();
        let turn_id = state.keys.get(&request.request_key)?;
        let entry = state.records.get(turn_id).expect("retained key indexed");
        Some(if entry.blocks == request.blocks {
            Ok(WorkReceipt {
                work: entry.record.work.clone(),
            })
        } else {
            Err(SessionError::Conflict {
                work: entry.record.work.clone(),
            })
        })
    }

    pub(crate) fn begin_shutdown(&self) -> Option<WorkRef> {
        let mut state = self.shared.lock();
        state.closed = true;
        if let Some(work) = &state.active
            && let Ok(record) = self.shared.record(&state, work)
        {
            record.control.cancellation_token().cancel();
        }
        state.active.clone()
    }

    pub(crate) async fn wait_finished(&self, work: &WorkRef) -> Result<(), SessionError> {
        let record = {
            let state = self.shared.lock();
            self.shared.record(&state, work)?
        };
        record.wait_finished().await;
        Ok(())
    }

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
        let control = CallControl::new(CancellationToken::new(), deadline);
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
            tools: super::ToolReporter::new(work.clone(), self.shared.progress.clone()),
            #[cfg(test)]
            abort: Mutex::new(None),
            #[cfg(test)]
            before_publish: Mutex::new(None),
        });
        if let Some(runtime) = &self.shared.compaction {
            runtime.set_current_request(crate::compact::user_request_text(&original_blocks));
        }
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
                read: false,
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
        let observation = {
            let state = self.shared.lock();
            self.shared.record(&state, work)?.snapshot()
        };
        if matches!(observation.state, WorkState::Finished(_)) {
            self.shared.mark_read(work);
        }
        Ok(observation)
    }

    /// Release facts for every retained work, in stable creation order.
    pub(crate) fn retained(&self) -> Vec<crate::session::RetainedWork> {
        let state = self.shared.lock();
        let mut works: Vec<_> = state
            .records
            .values()
            .map(|entry| crate::session::RetainedWork {
                work: entry.record.work.clone(),
                read: entry.read,
                finished: matches!(entry.record.snapshot().state, WorkState::Finished(_)),
                uncommitted: entry.uncommitted.is_some(),
            })
            .collect();
        works.sort_by(|left, right| left.work.turn_id.0.cmp(&right.work.turn_id.0));
        works
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
        let observation = record.wait(timeout).await;
        if matches!(observation.state, WorkState::Finished(_)) {
            self.shared.mark_read(work);
        }
        Ok(observation)
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
