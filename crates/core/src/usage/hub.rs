//! Central usage hub coordinating session totals and active-work attribution.

use super::types::{CallUsage, LastCall, PurposeUsage, SessionUsage, UsagePurpose, WorkUsage};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[derive(Default)]
struct HubState {
    conversation: PurposeUsage,
    compaction: PurposeUsage,
    input_rounds: u64,
    last_call: Option<LastCall>,
    active_work: Option<Arc<Mutex<WorkUsage>>>,
}

/// Thread-safe usage tracker coordinating session totals and active work statistics.
#[derive(Default)]
pub(crate) struct UsageHub {
    state: Mutex<HubState>,
}

impl UsageHub {
    /// Construct a new empty usage hub.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Construct a usage hub seeded with initial session usage.
    pub(crate) fn from_session_usage(usage: SessionUsage) -> Self {
        Self {
            state: Mutex::new(HubState {
                conversation: usage.conversation,
                compaction: usage.compaction,
                input_rounds: usage.input_rounds,
                last_call: usage.last_call,
                active_work: None,
            }),
        }
    }

    /// Record a successful model call and update session totals, last call, and active work stats.
    pub(crate) fn record_call_success(
        &self,
        purpose: UsagePurpose,
        usage: Option<&causa::kernel::ModelUsage>,
    ) {
        let call_usage = usage.map(CallUsage::from_model_usage);
        let mut state = self.state.lock().expect("usage lock poisoned");

        // 1. Session totals
        let session_target = match purpose {
            UsagePurpose::Conversation => &mut state.conversation,
            UsagePurpose::Compaction => &mut state.compaction,
        };
        session_target.calls.succeeded = session_target.calls.succeeded.saturating_add(1);
        if let Some(call) = &call_usage {
            session_target.tokens.record_call(call);
        } else {
            session_target.calls.unreported = session_target.calls.unreported.saturating_add(1);
        }

        // 2. Last call, truncated to the whole milliseconds the record keeps,
        // so a saved and reopened session reports the same time.
        let now = SystemTime::now();
        let timestamp =
            super::types::from_unix_millis(super::types::unix_millis(now)).unwrap_or(now);
        let last_call = LastCall {
            purpose,
            usage: call_usage,
            timestamp,
        };
        state.last_call = Some(last_call);

        // 3. Active work
        if let Some(active) = &state.active_work {
            let mut work = active.lock().expect("active work usage lock poisoned");
            work.last_call = Some(last_call);
            let work_target = match purpose {
                UsagePurpose::Conversation => {
                    work.model_rounds = work.model_rounds.saturating_add(1);
                    &mut work.conversation
                }
                UsagePurpose::Compaction => &mut work.compaction,
            };
            work_target.calls.succeeded = work_target.calls.succeeded.saturating_add(1);
            if let Some(call) = &call_usage {
                work_target.tokens.record_call(call);
            } else {
                work_target.calls.unreported = work_target.calls.unreported.saturating_add(1);
            }
        }
    }

    /// Record a failed model call (an invoke error, stream error, or dropped stream).
    pub(crate) fn record_call_failure(&self, purpose: UsagePurpose) {
        let mut state = self.state.lock().expect("usage lock poisoned");

        // 1. Session totals
        let session_target = match purpose {
            UsagePurpose::Conversation => &mut state.conversation,
            UsagePurpose::Compaction => &mut state.compaction,
        };
        session_target.calls.failed = session_target.calls.failed.saturating_add(1);

        // 2. Active work
        if let Some(active) = &state.active_work {
            let mut work = active.lock().expect("active work usage lock poisoned");
            let work_target = match purpose {
                UsagePurpose::Conversation => &mut work.conversation,
                UsagePurpose::Compaction => &mut work.compaction,
            };
            work_target.calls.failed = work_target.calls.failed.saturating_add(1);
        }
    }

    /// Record a finished work result. Only works that entered the runner (`WorkResult::Run(_)`)
    /// increment input rounds.
    pub(crate) fn record_work_result(&self, result: &crate::session::WorkResult) {
        if matches!(result, crate::session::WorkResult::Run(_)) {
            let mut state = self.state.lock().expect("usage lock poisoned");
            state.input_rounds = state.input_rounds.saturating_add(1);
        }
    }

    /// Bind or clear the active work's usage reference for call attribution.
    pub(crate) fn set_active_work(&self, work: Option<Arc<Mutex<WorkUsage>>>) {
        let mut state = self.state.lock().expect("usage lock poisoned");
        state.active_work = work;
    }

    /// Snapshot the current session-wide usage statistics.
    pub(crate) fn snapshot(&self) -> SessionUsage {
        let state = self.state.lock().expect("usage lock poisoned");
        SessionUsage {
            conversation: state.conversation,
            compaction: state.compaction,
            input_rounds: state.input_rounds,
            last_call: state.last_call,
        }
    }
}
