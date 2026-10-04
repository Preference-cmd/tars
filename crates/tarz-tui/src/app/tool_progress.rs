//! Bounded transient tool state; complete content comes from material cache.
use causa::kernel::ToolResultStatus;
use tarz_core::session::{ToolPhase, ToolProgress, ToolProgressSnapshot};

const MAX_CALLS: usize = 128;
#[derive(Default)]
pub(crate) struct ToolView {
    pub calls: Vec<ToolProgress>,
    pub lost: u64,
    pub truncated: bool,
    pub corrected: bool,
}
impl ToolView {
    pub fn apply(&mut self, call: &ToolProgress) {
        if let Some(previous) = self.calls.iter_mut().find(|previous| {
            previous.invocation_id == call.invocation_id
                && previous.call_block_id == call.call_block_id
        }) {
            // A delayed advisory return must never undo observed commit.
            if previous.phase == ToolPhase::Committed && call.phase != ToolPhase::Committed {
                return;
            }
            *previous = call.clone();
        } else if self.calls.len() < MAX_CALLS {
            self.calls.push(call.clone());
        } else {
            self.truncated = true;
        }
    }
    pub fn reconcile(&mut self, snapshot: &ToolProgressSnapshot) {
        self.calls.clear();
        self.truncated = snapshot.truncated || snapshot.calls.len() > MAX_CALLS;
        self.calls
            .extend(snapshot.calls.iter().take(MAX_CALLS).cloned());
        self.corrected = true;
    }
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.calls.len() + 2);
        if self.lost > 0 || self.truncated {
            lines.push(format!(
                "Tool observations incomplete · {} lost{}{}",
                self.lost,
                if self.truncated {
                    " · bounded view"
                } else {
                    ""
                },
                if self.corrected {
                    " · corrected from final facts"
                } else {
                    ""
                }
            ));
        }
        for call in &self.calls {
            let phase = match call.phase {
                ToolPhase::Waiting => "Waiting",
                ToolPhase::Running => "Running",
                ToolPhase::Returned if call.operation_not_started => {
                    "Not executed · Pending material commit"
                }
                ToolPhase::Returned if call.status == Some(ToolResultStatus::UnknownOutcome) => {
                    "Execution result unknown · Pending material commit"
                }
                ToolPhase::Returned => "Returned · Pending material commit",
                ToolPhase::Committed if call.operation_not_started => "Committed · Not executed",
                ToolPhase::Committed if call.status == Some(ToolResultStatus::UnknownOutcome) => {
                    "Committed · Execution result unknown"
                }
                ToolPhase::Committed => "Committed",
                ToolPhase::NotStarted => "Not executed",
                ToolPhase::Unknown => "Execution result unknown",
            };
            let status = call
                .status
                .as_ref()
                .filter(|status| {
                    !call.operation_not_started || **status != ToolResultStatus::UnknownOutcome
                })
                .map(|s| format!(" · {s:?}"))
                .unwrap_or_default();
            lines.push(
                crate::transcript::display(&format!(
                    "Tool {} · {phase}{status} · {} · {}",
                    call.tool_name, call.call_block_id.0, call.arguments_summary
                ))
                .replace('\n', " "),
            );
        }
        lines
    }
}
