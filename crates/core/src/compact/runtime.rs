//! Running compaction settings, planning and the history hand-off.
//!
//! A plan is built in this order: find the head, the retained tail and the
//! protected blocks, ask the model for a brief of the interval between them,
//! build the replacements around the protected blocks, remove sealed items
//! from the replies after the change and estimate the result. A plan is a
//! [`Rewrite`]; the shared commit path writes and fsyncs its record, edits
//! the Context and only then appends the index entry.

use super::estimate::{self, UsageTracker};
use super::history::{
    HistoryRecordEntry, HistoryStore, PlannedReplacement, Rewrite, RewriteError, RewriteKind,
};
use super::summary::{self, SUMMARY_MAX_TOKENS};
use super::{CompactionAbandon, HistoryTrigger};
use super::{sealed, segment};
use crate::definition::SessionDefinition;
use crate::session::ConversationId;
use causa::kernel::{
    BlockContent, BlockId, CallControl, Context, ContextBlock, GenerationOptions, InvocationId,
    ModelGateway, ModelRef, ModelRequest, RoundId, TurnId,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Share of the window a brief request is trimmed to when its material is
/// over the whole window.
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
    current_work_inputs: Mutex<HashSet<BlockId>>,
    prefix_too_large_reported: AtomicBool,
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
            current_work_inputs: Mutex::new(HashSet::new()),
            prefix_too_large_reported: AtomicBool::new(false),
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

    /// Protect the input blocks of the work that was just accepted.
    ///
    /// They keep their ids and stay in place whenever a summary in this work
    /// covers them, so the request being answered is never paraphrased away.
    pub(crate) fn set_current_work_inputs(&self, blocks: &[ContextBlock]) {
        *self
            .current_work_inputs
            .lock()
            .expect("work inputs have no callbacks") =
            blocks.iter().map(ContextBlock::id).collect();
    }

    /// Stop protecting the finished work's input blocks.
    pub(crate) fn clear_current_work_inputs(&self) {
        self.current_work_inputs
            .lock()
            .expect("work inputs have no callbacks")
            .clear();
    }

    fn current_work_inputs(&self) -> HashSet<BlockId> {
        self.current_work_inputs
            .lock()
            .expect("work inputs have no callbacks")
            .clone()
    }

    /// Whether automatic compaction already reported a too-large fixed prefix.
    pub(crate) fn prefix_too_large_reported(&self) -> bool {
        self.prefix_too_large_reported.load(Ordering::SeqCst)
    }

    /// Note that a too-large fixed prefix was reported; later rounds skip
    /// compaction quietly, since the prefix never changes.
    pub(crate) fn note_prefix_too_large_reported(&self) {
        self.prefix_too_large_reported.store(true, Ordering::SeqCst);
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

    /// Plan one summary of the oldest material after the head.
    ///
    /// The interval runs from the end of the head (fixed prefix and brief
    /// chain) to the cut point `k` that keeps the newest material within the
    /// retained-tail budget; protected blocks inside it stay. Nothing is
    /// written and the Context is untouched; the caller hands the returned
    /// rewrite to [`Self::commit_rewrite`].
    pub(crate) async fn plan(
        &self,
        context: &Context,
        model: &ModelRef,
        generation: &GenerationOptions,
        trigger: HistoryTrigger,
        control: &CallControl,
    ) -> Result<Rewrite, CompactionAbandon> {
        if self.history.is_none() {
            return Err(CompactionAbandon::Unavailable);
        }
        let blocks = context.blocks();
        if has_unpaired_tools(blocks) {
            return Err(CompactionAbandon::UnpairedMaterials);
        }
        let head_end = segment::head_end(blocks);
        let (_, threshold) = self.settings();
        let budget = segment::retained_tail_budget(
            self.window,
            threshold,
            segment::fixed_prefix_estimate(blocks),
        )
        .map_err(|_| CompactionAbandon::PrefixTooLarge)?;
        let k = sealed::tail_start_outside_open_loop(
            blocks,
            segment::tail_start(blocks, head_end, budget),
        );
        let protected =
            segment::protected_positions(blocks, head_end, k, &self.current_work_inputs());
        // Protected blocks stay and declarations are only folded, so an
        // interval of nothing else has nothing to summarize.
        let summarizable = (head_end..k).any(|index| {
            !protected.contains(&index)
                && !matches!(blocks[index].content(), BlockContent::ToolChange(_))
        });
        if !summarizable {
            return Err(CompactionAbandon::NoShrink);
        }

        // Only material over the whole window would be refused; it is then
        // trimmed from its earliest interval blocks to 70% of the window,
        // which leaves room for the brief.
        let limit = self
            .window
            .filter(|&window| self.usage.estimate(segment::blocks_bytes(&blocks[..k])) > window)
            .map(|window| {
                u64::try_from(u128::from(window) * u128::from(TRIM_TARGET_PERCENT) / 100)
                    .unwrap_or(u64::MAX)
            });
        let mut request_blocks = summary::request_material(blocks, head_end, k, &protected, limit);
        request_blocks.push(summary::instruction_block(summary::instruction(
            !protected.is_empty(),
        )));
        let text = self
            .brief(request_blocks, model, generation, control)
            .await?;

        let record_id = super::history::new_record_id();
        let brief = summary::summary_block(
            summary::new_block_id(),
            &summary::summary_text(&text, &record_id),
        );
        // The interval's tool changes, folded into one declaration before the
        // brief, keep the frame's tool state exactly as it was.
        let mut new_blocks: Vec<ContextBlock> =
            segment::tool_delta(&blocks[..head_end], &blocks[..k])
                .into_iter()
                .collect();
        new_blocks.push(brief);
        let replacements = sealed::strip_after(
            blocks,
            head_end,
            segment::replacements(blocks, head_end, k, &protected, new_blocks),
        );
        self.rewrite(
            context,
            record_id,
            trigger,
            RewriteKind::Summarize,
            replacements,
        )
    }

    /// Ask the session's model for a brief and keep only its text.
    async fn brief(
        &self,
        blocks: Vec<ContextBlock>,
        model: &ModelRef,
        generation: &GenerationOptions,
        control: &CallControl,
    ) -> Result<String, CompactionAbandon> {
        let mut request = ModelRequest::new(
            InvocationId {
                turn_id: TurnId::new(format!("compaction-{}", uuid::Uuid::now_v7())),
                round_id: RoundId(0),
            },
            causa::kernel::ContextFrame { blocks },
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
        Ok(text.to_owned())
    }

    /// The rewrite of `replacements`, or `NoShrink` when it does not make the
    /// Context smaller.
    fn rewrite(
        &self,
        context: &Context,
        record_id: String,
        trigger: HistoryTrigger,
        kind: RewriteKind,
        replacements: Vec<PlannedReplacement>,
    ) -> Result<Rewrite, CompactionAbandon> {
        // Compare like with like: serialized sizes before and after the edit.
        let (removed, inserted) = sealed::replaced_bytes(context.blocks(), &replacements);
        if inserted >= removed {
            return Err(CompactionAbandon::NoShrink);
        }
        // The calibrated estimate minus what the edit saves, so both numbers
        // are on the same scale.
        let estimate_before = self.usage.estimate(estimate::context_bytes(context));
        let estimate_after =
            estimate_before.saturating_sub(estimate::estimate_bytes(removed - inserted));
        Ok(Rewrite {
            record_id,
            trigger,
            kind,
            estimate_before,
            estimate_after,
            replacements,
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
    /// The brief now standing for the replaced material.
    pub brief: BlockId,
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
