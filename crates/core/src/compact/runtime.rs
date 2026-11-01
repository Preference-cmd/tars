//! Running compaction settings, planning and the history hand-off.
//!
//! A plan is built in this order: decide on the summary request, obtain the
//! model's brief, build the replacement blocks, estimate the result, write and
//! fsync the history segment, and only then let the caller edit the Context.
//! Committing the plan appends the index entry; discarding it deletes the file.

use super::estimate::{self, UsageTracker};
use super::history::{HISTORY_FORMAT, HistorySegmentDocument, HistoryStore, now_ms};
use super::summary::{self, SUMMARY_MAX_TOKENS};
use super::{CompactionAbandon, HistorySegment, HistoryTrigger};
use crate::definition::SessionDefinition;
use crate::session::ConversationId;
use causa::kernel::{
    BlockContent, CallControl, Context, GenerationOptions, InvocationId, ModelGateway, ModelRef,
    ModelRequest, RoundId, TurnId,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Trigger percentage used when the old context itself exceeds the window.
const TRIM_TARGET_PERCENT: u64 = 70;

#[derive(Clone, Copy, Debug)]
struct Settings {
    auto_compact: bool,
    threshold: u8,
}

/// Session-wide compaction state: settings, estimator, history and force flag.
pub(crate) struct CompactionRuntime {
    settings: Mutex<Settings>,
    window: Option<u64>,
    max_tokens: Option<u32>,
    conversation_id: String,
    history: Option<Arc<HistoryStore>>,
    usage: Arc<UsageTracker>,
    force: AtomicBool,
    current_request: Mutex<Option<String>>,
    gateway: Arc<dyn ModelGateway>,
    model: ModelRef,
    generation: GenerationOptions,
}

impl CompactionRuntime {
    /// Build the per-session runtime, if this session can keep history.
    ///
    /// Compaction needs a history directory; without a store there is nowhere
    /// to keep the replaced conversation, so no runtime is installed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        definition: &SessionDefinition,
        conversation_id: &ConversationId,
        sessions_directory: Option<&Path>,
        history: Vec<HistorySegment>,
        current_auto_compact: Option<bool>,
        current_threshold: Option<u8>,
        gateway: Arc<dyn ModelGateway>,
        usage: Arc<UsageTracker>,
    ) -> Option<Arc<Self>> {
        let sessions_directory = sessions_directory?;
        let history = Arc::new(HistoryStore::new(
            sessions_directory,
            conversation_id.as_str(),
            history,
        ));
        let options = definition.run_options();
        Some(Arc::new(Self {
            settings: Mutex::new(Settings {
                auto_compact: current_auto_compact.unwrap_or(definition.auto_compact),
                threshold: current_threshold.unwrap_or(definition.compact_threshold),
            }),
            window: definition.context_window,
            max_tokens: definition.max_tokens,
            conversation_id: conversation_id.as_str().to_owned(),
            history: Some(history),
            usage,
            force: AtomicBool::new(false),
            current_request: Mutex::new(None),
            gateway,
            model: options.model,
            generation: options.generation,
        }))
    }

    /// Current automatic switch and trigger percentage.
    pub(crate) fn settings(&self) -> (bool, u8) {
        let settings = self.settings.lock().expect("settings have no callbacks");
        (settings.auto_compact, settings.threshold)
    }

    /// Replace the automatic switch; the caller persists the record.
    pub(crate) fn set_auto_compact(&self, enabled: bool) {
        self.settings
            .lock()
            .expect("settings have no callbacks")
            .auto_compact = enabled;
    }

    /// Replace the trigger percentage; the caller persists the record.
    pub(crate) fn set_threshold(&self, threshold: u8) {
        self.settings
            .lock()
            .expect("settings have no callbacks")
            .threshold = threshold;
    }

    /// Running values that differ from the frozen definition's initial ones.
    pub(crate) fn record_overrides(
        &self,
        definition: &SessionDefinition,
    ) -> (Option<bool>, Option<u8>) {
        let (auto, threshold) = self.settings();
        (
            (auto != definition.auto_compact).then_some(auto),
            (threshold != definition.compact_threshold).then_some(threshold),
        )
    }

    /// Configured model context window, or none when it is unknown.
    pub(crate) fn window(&self) -> Option<u64> {
        self.window
    }

    /// Current history index, possibly empty.
    pub(crate) fn entries(&self) -> Vec<HistorySegment> {
        self.history
            .as_ref()
            .map(|history| history.entries())
            .unwrap_or_default()
    }

    /// History directory access for display reads.
    pub(crate) fn history(&self) -> Option<Arc<HistoryStore>> {
        self.history.clone()
    }

    /// Reader for the current estimator state.
    pub(crate) fn usage(&self) -> Arc<UsageTracker> {
        self.usage.clone()
    }

    /// Remember the user's submitted request text for in-turn summaries.
    pub(crate) fn set_current_request(&self, text: Option<String>) {
        *self
            .current_request
            .lock()
            .expect("request text has no callbacks") = text;
    }

    /// The current work's submitted request text, when it had one.
    pub(crate) fn current_request(&self) -> Option<String> {
        self.current_request
            .lock()
            .expect("request text has no callbacks")
            .clone()
    }

    /// Ask for one forced compaction after the provider reported an overflow.
    pub(crate) fn request_force(&self) {
        if self.window.is_some() {
            self.force.store(true, Ordering::SeqCst);
        }
    }

    /// Decide whether this round compacts, consuming the force flag once.
    pub(crate) fn trigger_for(&self, estimate: u64) -> Option<HistoryTrigger> {
        let window = self.window?;
        if self.force.swap(false, Ordering::SeqCst) {
            return Some(HistoryTrigger::Passive);
        }
        let (auto_compact, threshold_percent) = self.settings();
        if !auto_compact {
            return None;
        }
        let threshold = u128::from(window) * u128::from(threshold_percent) / 100;
        let over_threshold = u128::from(estimate) >= threshold;
        let leaves_no_output = self
            .max_tokens
            .is_some_and(|output| estimate.saturating_add(u64::from(output)) > window);
        (over_threshold || leaves_no_output).then_some(HistoryTrigger::Auto)
    }

    /// Record the size seen by a preparation round; the next estimate starts here.
    pub(crate) fn note_prepare(&self, bytes: u64) {
        self.usage.note_prepare_bytes(bytes);
    }

    /// Plan one compaction: summary request, replacement, history write.
    ///
    /// The old Context is untouched. On success the segment file is already
    /// written; the caller either commits the plan after editing the Context or
    /// discards it to delete the file.
    pub(crate) async fn plan(
        &self,
        context: &Context,
        model: &ModelRef,
        generation: &GenerationOptions,
        trigger: HistoryTrigger,
        user_text: Option<&str>,
        control: &CallControl,
    ) -> Result<CompactionPlan, CompactionAbandon> {
        let Some(history) = &self.history else {
            return Err(CompactionAbandon::Unavailable);
        };
        let old_blocks = context.blocks();
        if has_unpaired_tools(old_blocks) {
            return Err(CompactionAbandon::UnpairedMaterials);
        }
        let estimate_before = self.usage.estimate(estimate::context_bytes(context));
        // When the old material itself is over the window the summary request
        // would be refused too; drop the earliest material until it fits.
        let mut request_blocks: Vec<causa::kernel::ContextBlock> =
            if self.window.is_some_and(|window| estimate_before > window) {
                let window = self.window.expect("checked window");
                let limit = u128::from(window) * u128::from(TRIM_TARGET_PERCENT) / 100;
                summary::trim_to_limit(old_blocks, u64::try_from(limit).unwrap_or(u64::MAX))
            } else {
                old_blocks.to_vec()
            };
        request_blocks.push(summary::instruction_block(summary::instruction(user_text)));
        let mut request = ModelRequest::new(
            InvocationId {
                turn_id: TurnId::new(format!("compaction-{}", uuid::Uuid::now_v7())),
                round_id: RoundId(0),
            },
            causa::kernel::ContextFrame {
                blocks: request_blocks,
            },
            model.clone(),
        );
        let mut generation = generation.clone();
        generation.max_tokens = Some(match self.max_tokens {
            Some(limit) => SUMMARY_MAX_TOKENS.min(limit),
            None => SUMMARY_MAX_TOKENS,
        });
        request.generation = generation;
        let output = self
            .gateway
            .invoke(&request, control)
            .await
            .map_err(|_| CompactionAbandon::RequestFailed)?;
        let text = output.response.text();
        let text = text.trim();
        if text.is_empty() {
            return Err(CompactionAbandon::EmptySummary);
        }
        let summary_id = summary::new_block_id();
        let new_blocks = summary::replacement_blocks(old_blocks, summary_id, text);
        let new_bytes = serde_json::to_vec(&new_blocks)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        let estimate_after = estimate::estimate_bytes(new_bytes);
        if estimate_after >= estimate_before {
            return Err(CompactionAbandon::NoShrink);
        }
        let segment_id = uuid::Uuid::now_v7().to_string();
        let created_at_ms = now_ms();
        let document = HistorySegmentDocument {
            format: HISTORY_FORMAT.to_owned(),
            conversation_id: self.conversation_id.clone(),
            segment_id: segment_id.clone(),
            created_at_ms,
            trigger,
            estimate_before,
            estimate_after,
            summary_block_id: summary_id,
            context: context.clone(),
        };
        if history.write(&document).is_err() {
            return Err(CompactionAbandon::UnwritableHistory);
        }
        Ok(CompactionPlan {
            entry: document.index_entry(),
            new_blocks,
            estimate_before,
            estimate_after,
            trigger,
        })
    }

    /// Plan one manual compaction with the session's frozen model settings.
    pub(crate) async fn manual_plan(
        &self,
        context: &Context,
        control: &CallControl,
    ) -> Result<CompactionPlan, CompactionAbandon> {
        self.plan(
            context,
            &self.model,
            &self.generation,
            HistoryTrigger::Manual,
            None,
            control,
        )
        .await
    }

    /// Drop a committed plan when the material installation failed.
    pub(crate) fn rollback(&self, plan: &CompactionPlan) {
        self.discard(plan);
        if let Some(history) = &self.history {
            history.remove_entry(&plan.entry.id);
        }
    }

    /// Append the plan's index entry after the Context edit committed.
    pub(crate) fn commit(&self, plan: &CompactionPlan) {
        if let Some(history) = &self.history {
            history.append(plan.entry.clone());
        }
        self.usage.reset();
    }

    /// Delete the plan's segment file when the Context edit did not happen.
    pub(crate) fn discard(&self, plan: &CompactionPlan) {
        if let Some(history) = &self.history {
            history.remove(&plan.entry.id);
        }
    }
}

/// One prepared compaction whose segment file exists on disk.
pub(crate) struct CompactionPlan {
    /// Replacement blocks; the caller installs them atomically.
    pub(crate) new_blocks: Vec<causa::kernel::ContextBlock>,
    /// Record index entry backing the written file.
    pub(crate) entry: HistorySegment,
    /// Estimated request size before the replacement.
    pub(crate) estimate_before: u64,
    /// Estimated request size after the replacement.
    pub(crate) estimate_after: u64,
    /// What triggered this attempt.
    pub(crate) trigger: HistoryTrigger,
}

/// The public fact returned to a manual compaction caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactionOutcome {
    /// What triggered this attempt.
    pub trigger: HistoryTrigger,
    /// Estimated request size before the replacement.
    pub estimate_before: u64,
    /// Estimated request size after the replacement.
    pub estimate_after: u64,
    /// Index entry of the segment that holds the replaced material.
    pub segment: HistorySegment,
}

impl CompactionPlan {
    /// Convert the plan to the manual caller's outcome.
    pub(crate) fn outcome(&self) -> CompactionOutcome {
        CompactionOutcome {
            trigger: self.trigger,
            estimate_before: self.estimate_before,
            estimate_after: self.estimate_after,
            segment: self.entry.clone(),
        }
    }
}

/// Whether the material still has declarations without answers.
///
/// Auto and manual compaction both require a clean Context; the repair path
/// owns interrupted material and must not be bypassed by a summary.
fn has_unpaired_tools(blocks: &[causa::kernel::ContextBlock]) -> bool {
    let calls = blocks
        .iter()
        .filter_map(|block| match block.content() {
            BlockContent::ToolCall(_) => Some(block.id()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    let answered = blocks
        .iter()
        .filter_map(|block| match block.content() {
            BlockContent::ToolResult(result) => Some(result.call_block_id),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    !calls.is_subset(&answered)
}
