use super::types::*;
use causa::kernel::{CallControl, Context, ContextBlock, ToolBatch, TurnId};
use causa::runtime::{TurnRunOptions, TurnRunner};
use std::collections::HashMap;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::{Notify, broadcast};

type AssemblyFuture = Pin<
    Box<
        dyn Future<Output = Result<(Arc<TurnRunner>, TurnRunOptions), Box<dyn Error + Send + Sync>>>
            + Send,
    >,
>;
pub(super) type Assemble =
    dyn Fn(WorkRef, CallControl, super::ToolReporter) -> AssemblyFuture + Send + Sync;

pub(super) struct Shared {
    pub conversation_id: ConversationId,
    pub state: Mutex<State>,
    pub assemble: Arc<Assemble>,
    pub config: SessionConfig,
    pub candidate: Arc<dyn Fn() -> TurnId + Send + Sync>,
    pub progress: broadcast::Sender<Arc<WorkProgress>>,
    pub compaction: Option<Arc<crate::compact::CompactionRuntime>>,
}
pub(super) struct State {
    pub context: Option<Context>,
    pub active: Option<WorkRef>,
    pub closed: bool,
    pub faulted: bool,
    pub records: HashMap<TurnId, Entry>,
    pub keys: HashMap<String, TurnId>,
    pub claims_revision: u64,
}
pub(super) struct Entry {
    pub request_key: String,
    pub blocks: Vec<ContextBlock>,
    pub record: Arc<Record>,
    pub uncommitted: Option<ToolBatch>,
    /// A caller obtained the terminal result through observe or wait.
    pub read: bool,
}
pub(super) struct Record {
    pub work: WorkRef,
    pub control: CallControl,
    pub state: Mutex<WorkState>,
    pub changed: Notify,
    pub tools: super::ToolReporter,
    #[cfg(test)]
    pub abort: Mutex<Option<tokio::task::AbortHandle>>,
    #[cfg(test)]
    pub before_publish: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

impl Shared {
    // Poison is an explicit session fault. Recovering the guard permits replay,
    // shutdown and retained-result inspection, never fresh execution.
    pub fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.faulted = true;
                guard
            }
        }
    }
    pub fn record(&self, state: &State, work: &WorkRef) -> Result<Arc<Record>, SessionError> {
        if work.conversation_id == self.conversation_id
            && let Some(entry) = state.records.get(&work.turn_id)
        {
            return Ok(entry.record.clone());
        }
        Err(SessionError::NotFound { work: work.clone() })
    }
    /// Record that one caller has seen this work's terminal result.
    pub fn mark_read(&self, work: &WorkRef) {
        let mut state = self.lock();
        if let Some(entry) = state.records.get_mut(&work.turn_id) {
            entry.read = true;
        }
    }

    pub fn finish(
        &self,
        record: &Record,
        context: Option<Context>,
        uncommitted: Option<ToolBatch>,
        result: WorkResult,
    ) {
        let mut state = self.lock();
        if matches!(result, WorkResult::Faulted(_)) {
            state.faulted = true;
        }
        if let Some(runtime) = &self.compaction {
            runtime.set_current_request(None);
            if context_window_exceeded(&result) {
                // One forced compaction on the session's next round, even with
                // automatic compaction switched off; failure does not retry.
                runtime.request_force();
            }
        }
        state.context = context;
        if let Some(entry) = state.records.get_mut(&record.work.turn_id) {
            entry.uncommitted = uncommitted;
        }
        // Materials and batch are back before the terminal value becomes visible.
        record.publish(WorkState::Finished(Arc::new(result)));
        state.active = None;
    }
}
/// Whether this failed work ended because the request no longer fit the model's
/// context window, either as a provider stop reason or as an overflow refusal.
fn context_window_exceeded(result: &WorkResult) -> bool {
    use causa::kernel::ModelInvokeErrorKind;
    use causa::runtime::{TurnInterruption, TurnResult};
    match result {
        WorkResult::Run(TurnResult::Interrupted { cause }) => match cause {
            TurnInterruption::ModelContextWindowExceeded { .. } => true,
            TurnInterruption::ModelCallFailed { error, .. } => {
                error.kind == ModelInvokeErrorKind::InvalidRequest
                    && mentions_context_window(&error.message)
            }
            _ => false,
        },
        _ => false,
    }
}

fn mentions_context_window(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("context")
        && (message.contains("length")
            || message.contains("window")
            || message.contains("token")
            || message.contains("too long")
            || message.contains("maximum"))
}

impl Record {
    pub fn snapshot(&self) -> WorkObservation {
        WorkObservation {
            work: self.work.clone(),
            state: self
                .state
                .lock()
                .expect("record state has no user callbacks")
                .clone(),
            tools: self.tools.snapshot(),
        }
    }
    pub fn publish(&self, state: WorkState) {
        *self
            .state
            .lock()
            .expect("record state has no user callbacks") = state;
        self.changed.notify_waiters();
    }
    pub async fn wait(&self, timeout: Duration) -> WorkObservation {
        let wait = async {
            loop {
                // Register before checking to close the terminal notification race.
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let observation = self.snapshot();
                if matches!(observation.state, WorkState::Finished(_)) {
                    return observation;
                }
                notified.await;
            }
        };
        if timeout.is_zero() {
            return self.snapshot();
        }
        match tokio::time::timeout(timeout, wait).await {
            Ok(observation) => observation,
            Err(_) => self.snapshot(),
        }
    }
    pub async fn wait_finished(&self) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(self.snapshot().state, WorkState::Finished(_)) {
                return;
            }
            notified.await;
        }
    }
}
