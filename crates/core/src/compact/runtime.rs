//! Running compaction settings, planning and the history hand-off.
//!
//! A plan is built in this order: decide on the summary request, obtain the
//! model's brief, build the replacement blocks and estimate the result. The
//! plan is a [`Rewrite`]; the shared commit path writes and fsyncs its record,
//! edits the Context and only then appends the index entry.

use super::estimate::{self, UsageTracker};
use super::history::{
    HistoryRecordEntry, HistoryStore, PlannedReplacement, Rewrite, RewriteError, RewriteKind,
};
use super::summary::{self, SUMMARY_MAX_TOKENS};
use super::{CompactionAbandon, HistoryTrigger};
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
    prompt_cache: bool,
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
        history: Vec<HistoryRecordEntry>,
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
            prompt_cache: definition.prompt_cache,
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
    pub(crate) fn entries(&self) -> Vec<HistoryRecordEntry> {
        self.history
            .as_ref()
            .map(|history| history.entries())
            .unwrap_or_default()
    }

    /// History directory access for display reads.
    pub(crate) fn history(&self) -> Option<Arc<HistoryStore>> {
        self.history.clone()
    }

    /// Artifact store backed by this session's rewrite records.
    #[allow(dead_code)]
    pub(crate) fn artifacts(&self) -> Option<super::HistoryArtifacts> {
        self.history
            .as_ref()
            .map(|history| super::HistoryArtifacts::new(history.clone()))
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

    /// Plan one whole-Context compaction: summary request and replacement.
    ///
    /// Nothing is written and the Context is untouched; the caller hands the
    /// returned rewrite to [`Self::commit_rewrite`].
    pub(crate) async fn plan(
        &self,
        context: &Context,
        model: &ModelRef,
        generation: &GenerationOptions,
        trigger: HistoryTrigger,
        user_text: Option<&str>,
        control: &CallControl,
    ) -> Result<Rewrite, CompactionAbandon> {
        if self.history.is_none() {
            return Err(CompactionAbandon::Unavailable);
        }
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
        if self.prompt_cache {
            request.cache = causa::kernel::CacheDirective::StablePrefix;
        }
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
        let record_id = super::history::new_record_id();
        let summary_id = summary::new_block_id();
        let text = summary::summary_text(text, user_text, &record_id);
        let new_blocks = summary::replacement_blocks(old_blocks, summary_id, &text);
        let new_bytes = serde_json::to_vec(&new_blocks)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        let estimate_after = estimate::estimate_bytes(new_bytes);
        if estimate_after >= estimate_before {
            return Err(CompactionAbandon::NoShrink);
        }
        Ok(Rewrite {
            record_id,
            trigger,
            kind: RewriteKind::Summarize,
            estimate_before,
            estimate_after,
            replacements: vec![PlannedReplacement {
                range: 0..old_blocks.len(),
                with: new_blocks,
            }],
        })
    }

    /// Commit one rewrite through the shared commit path.
    pub(crate) fn commit_rewrite(
        &self,
        context: &mut Context,
        rewrite: Rewrite,
    ) -> Result<HistoryRecordEntry, CompactionAbandon> {
        let Some(history) = &self.history else {
            return Err(CompactionAbandon::Unavailable);
        };
        let entry = history
            .commit_rewrite(context, rewrite)
            .map_err(|error| match error {
                RewriteError::Rejected(_) => CompactionAbandon::EditRejected,
                RewriteError::Store(_) => CompactionAbandon::UnwritableHistory,
            })?;
        self.usage.reset();
        Ok(entry)
    }

    /// Plan one manual compaction with the session's frozen model settings.
    pub(crate) async fn manual_plan(
        &self,
        context: &Context,
        control: &CallControl,
    ) -> Result<Rewrite, CompactionAbandon> {
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

    /// Undo a committed rewrite whose Context was then not saved.
    pub(crate) fn rollback_record(&self, record_id: &str) {
        if let Some(history) = &self.history {
            history.remove(record_id);
            history.remove_entry(record_id);
        }
    }
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
    /// Index entry of the rewrite record that holds the replaced material.
    pub record: HistoryRecordEntry,
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
