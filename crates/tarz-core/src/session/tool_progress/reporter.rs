use super::types::{ToolPhase, ToolProgress, ToolProgressSnapshot};
use crate::session::{ProgressUpdate, WorkProgress, WorkRef};
use causa::kernel::{
    BlockContent, ContextBlock, InvocationId, ToolBatch, ToolCallContext, ToolResultStatus,
};
use causa::runtime::RunEvent;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

const MAX_CALLS: usize = 128;

struct State {
    snapshot: ToolProgressSnapshot,
    frozen: bool,
    settled: HashSet<causa::kernel::BlockId>,
    watched: HashSet<causa::kernel::BlockId>,
    started: HashSet<causa::kernel::BlockId>,
}

struct Inner {
    work: WorkRef,
    sender: broadcast::Sender<Arc<WorkProgress>>,
    state: Mutex<State>,
    tasks: TaskTracker,
    sealed: Mutex<bool>,
}

/// Per-work application observation and supervision handle supplied during assembly.
///
/// Clones refer to one retained work. Workspace adapters use this handle to
/// report real starts and register blocking jobs; the supervisor drains those
/// jobs before publishing a terminal result. This is not a persistent task log.
#[derive(Clone)]
pub struct ToolReporter(Arc<Inner>);

impl ToolReporter {
    pub(crate) fn new(work: WorkRef, sender: broadcast::Sender<Arc<WorkProgress>>) -> Self {
        Self(Arc::new(Inner {
            work,
            sender,
            state: Mutex::new(State {
                snapshot: ToolProgressSnapshot::default(),
                frozen: false,
                settled: HashSet::new(),
                watched: HashSet::new(),
                started: HashSet::new(),
            }),
            tasks: TaskTracker::new(),
            sealed: Mutex::new(false),
        }))
    }

    pub(crate) fn waiting(&self, call: &ToolCallContext) {
        self.operation_stage(call, false);
        self.report(call, ToolPhase::Waiting, None);
    }

    pub(crate) fn running(&self, call: &ToolCallContext) {
        // Keep actual start information even when a controlled runner return
        // has already settled its conservative UnknownOutcome payload.
        self.operation_stage(call, true);
        self.report(call, ToolPhase::Running, None);
    }

    pub(crate) fn returned(&self, call: &ToolCallContext, status: ToolResultStatus) {
        self.report(call, ToolPhase::Returned, Some(status));
    }

    pub(crate) fn spawn_blocking<F, R>(&self, operation: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        // TaskTracker::close alone does not prevent later spawns. Serialize
        // admission against drain and reject an internal late dispatch.
        let sealed = self.0.sealed.lock().expect("tool task admission");
        assert!(!*sealed, "cannot start a tool job after work settlement");
        self.0.tasks.spawn_blocking(operation)
    }

    pub(crate) async fn drain(&self) {
        {
            let mut sealed = self.0.sealed.lock().expect("tool task admission");
            *sealed = true;
            self.0.tasks.close();
        }
        self.0.tasks.wait().await;
    }

    pub(crate) fn snapshot(&self) -> Arc<ToolProgressSnapshot> {
        Arc::new(
            self.0
                .state
                .lock()
                .expect("tool observations have no callbacks")
                .snapshot
                .clone(),
        )
    }

    pub(crate) fn freeze(&self) {
        let mut state = self
            .0
            .state
            .lock()
            .expect("tool observations have no callbacks");
        let State {
            snapshot,
            watched,
            started,
            ..
        } = &mut *state;
        for call in &mut snapshot.calls {
            call.operation_not_started |=
                watched.contains(&call.call_block_id) && !started.contains(&call.call_block_id);
            // Causa dispatch can enter a wrapper before it acquires a file slot.
            // Only after tracked jobs are drained is absence of a physical
            // start final. Preserve the original status/U; this projection adds
            // the application's narrower execution fact.
            if call.status == Some(ToolResultStatus::UnknownOutcome) {
                if call.phase != ToolPhase::Committed {
                    call.phase = if call.operation_not_started {
                        ToolPhase::NotStarted
                    } else {
                        ToolPhase::Unknown
                    };
                }
                continue;
            }
            match call.phase {
                ToolPhase::Waiting => {
                    call.phase = ToolPhase::NotStarted;
                    call.operation_not_started = true;
                }
                ToolPhase::Running => {
                    call.phase = ToolPhase::Unknown;
                    call.status = Some(ToolResultStatus::UnknownOutcome);
                }
                _ => {}
            }
        }
        state.frozen = true;
    }

    fn operation_stage(&self, call: &ToolCallContext, started: bool) {
        let mut state = self
            .0
            .state
            .lock()
            .expect("tool observations have no callbacks");
        if state.frozen
            || !state
                .snapshot
                .calls
                .iter()
                .any(|row| row.call_block_id == call.call_block_id)
        {
            return;
        }
        state.watched.insert(call.call_block_id);
        if started {
            state.started.insert(call.call_block_id);
        }
    }

    /// Routes the runner's observations to the state updates below. The
    /// updates take their facts as arguments, so tests drive any event order
    /// without building a `RunEvent`.
    pub(crate) fn observe(&self, event: &RunEvent<'_>) {
        match event {
            RunEvent::ToolBatchReady {
                invocation_id,
                batch,
                ..
            } => self.batch_ready(invocation_id, batch),
            RunEvent::ToolBatchReturned {
                invocation_id,
                batch,
                ..
            } => self.batch_returned(invocation_id, batch),
            RunEvent::BlocksCommitted {
                invocation_id,
                blocks,
                ..
            } => self.blocks_committed(invocation_id, blocks),
            _ => {}
        }
    }

    /// Newly declared calls are waiting for tool processing.
    pub(crate) fn batch_ready(&self, invocation_id: &InvocationId, batch: &ToolBatch) {
        for entry in batch.calls() {
            self.update(entry.call(), invocation_id, ToolPhase::Waiting, None, false);
        }
    }

    /// Tool processing returned: resolved calls carry their status, the rest
    /// never started. Both are authoritative over later wrapper callbacks.
    pub(crate) fn batch_returned(&self, invocation_id: &InvocationId, batch: &ToolBatch) {
        for entry in batch.results() {
            if let Some((_, result)) = entry.result() {
                self.update(
                    entry.call(),
                    invocation_id,
                    ToolPhase::Returned,
                    Some(result.status.clone()),
                    true,
                );
            }
        }
        for entry in batch.calls() {
            self.update(
                entry.call(),
                invocation_id,
                ToolPhase::NotStarted,
                None,
                true,
            );
        }
    }

    /// Results reached the context; only an actual append marks a call committed.
    pub(crate) fn blocks_committed(&self, invocation_id: &InvocationId, blocks: &[ContextBlock]) {
        for block in blocks {
            if let BlockContent::ToolResult(result) = block.content() {
                let event = {
                    let mut state = self
                        .0
                        .state
                        .lock()
                        .expect("tool observations have no callbacks");
                    if state.frozen {
                        return;
                    }
                    state
                        .snapshot
                        .calls
                        .iter_mut()
                        .find(|call| {
                            call.call_block_id == result.call_block_id
                                && call.invocation_id == *invocation_id
                        })
                        .map(|call| {
                            call.phase = ToolPhase::Committed;
                            call.status = Some(result.status.clone());
                            call.clone()
                        })
                };
                if let Some(event) = event {
                    self.send(event);
                }
            }
        }
    }

    fn report(&self, call: &ToolCallContext, phase: ToolPhase, status: Option<ToolResultStatus>) {
        let invocation = self
            .0
            .state
            .lock()
            .expect("tool observations have no callbacks")
            .snapshot
            .calls
            .iter()
            .find(|row| row.call_block_id == call.call_block_id)
            .map(|row| row.invocation_id.clone());
        if let Some(invocation) = invocation {
            self.update(call, &invocation, phase, status, false);
        }
    }

    pub(crate) fn record_permission_decision(
        &self,
        call: &ToolCallContext,
        invocation: &InvocationId,
        decision: crate::permission::PermissionDecision,
    ) {
        let event = {
            let mut state = self
                .0
                .state
                .lock()
                .expect("tool observations have no callbacks");
            if state.frozen {
                return;
            }
            let position = state.snapshot.calls.iter().position(|entry| {
                entry.call_block_id == call.call_block_id && entry.invocation_id == *invocation
            });
            if let Some(pos) = position {
                state.snapshot.calls[pos].permission_decision = Some(decision);
                state.snapshot.calls[pos].clone()
            } else if state.snapshot.calls.len() < MAX_CALLS {
                let next = ToolProgress {
                    invocation_id: invocation.clone(),
                    call_block_id: call.call_block_id,
                    tool_name: bounded(&call.input.tool_name, 128),
                    arguments_summary: argument_summary(&call.input.arguments),
                    phase: ToolPhase::Waiting,
                    status: None,
                    operation_not_started: false,
                    permission_decision: Some(decision),
                };
                state.snapshot.calls.push(next.clone());
                next
            } else {
                state.snapshot.truncated = true;
                return;
            }
        };
        self.send(event);
    }

    fn update(
        &self,
        call: &ToolCallContext,
        invocation: &InvocationId,
        phase: ToolPhase,
        status: Option<ToolResultStatus>,
        authoritative: bool,
    ) {
        let event = {
            let mut state = self
                .0
                .state
                .lock()
                .expect("tool observations have no callbacks");
            if state.frozen {
                return;
            }
            if !authoritative && state.settled.contains(&call.call_block_id) {
                return;
            }
            let position = state.snapshot.calls.iter().position(|entry| {
                entry.call_block_id == call.call_block_id && entry.invocation_id == *invocation
            });
            if authoritative && position.is_some() {
                state.settled.insert(call.call_block_id);
            }
            let existing_decision =
                position.and_then(|pos| state.snapshot.calls[pos].permission_decision.clone());
            let next = ToolProgress {
                invocation_id: invocation.clone(),
                call_block_id: call.call_block_id,
                tool_name: bounded(&call.input.tool_name, 128),
                arguments_summary: argument_summary(&call.input.arguments),
                phase,
                status,
                operation_not_started: phase == ToolPhase::NotStarted,
                permission_decision: existing_decision,
            };
            if let Some(position) = position {
                let previous = &state.snapshot.calls[position];
                // A late start from a queued blocking closure must not reopen
                // a call already settled by the runner's controlled return.
                if (matches!(phase, ToolPhase::Waiting | ToolPhase::Running)
                    && !matches!(previous.phase, ToolPhase::Waiting | ToolPhase::Running))
                    || previous.phase == ToolPhase::Committed
                    || *previous == next
                {
                    return;
                }
                state.snapshot.calls[position] = next.clone();
            } else if state.snapshot.calls.len() < MAX_CALLS {
                state.snapshot.calls.push(next.clone());
            } else {
                state.snapshot.truncated = true;
                return;
            }
            if authoritative {
                state.settled.insert(call.call_block_id);
            }
            next
        };
        self.send(event);
    }

    fn send(&self, call: ToolProgress) {
        let _ = self.0.sender.send(Arc::new(WorkProgress {
            work: self.0.work.clone(),
            invocation_id: call.invocation_id.clone(),
            update: ProgressUpdate::Tool(call),
        }));
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit - '…'.len_utf8();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn argument_summary(value: &serde_json::Value) -> String {
    struct Limited(Vec<u8>);
    impl Write for Limited {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let remaining = 256usize.saturating_sub(self.0.len());
            if remaining == 0 {
                return Err(io::Error::other("argument preview full"));
            }
            let count = remaining.min(bytes.len());
            self.0.extend_from_slice(&bytes[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Limited(Vec::with_capacity(256));
    let truncated = serde_json::to_writer(&mut output, value).is_err();
    let end = std::str::from_utf8(&output.0).map_or_else(|e| e.valid_up_to(), |_| output.0.len());
    let text = std::str::from_utf8(&output.0[..end]).expect("validated UTF-8");
    if truncated {
        bounded(&format!("{text}…"), 256)
    } else {
        text.to_owned()
    }
}
