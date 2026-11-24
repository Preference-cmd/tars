//! Running compaction settings, planning and the history hand-off.
//!
//! A plan is built in this order: find the head, the retained tail and the
//! protected blocks, ask the model for a brief of the interval between them,
//! build the replacements around the protected blocks, remove sealed items
//! from the replies after the change and estimate the result. After a
//! summary the brief chain may be merged, which is a second plan and a
//! second record. A plan is a [`Rewrite`]; the shared commit path writes and
//! fsyncs its record, edits the Context and only then appends the index
//! entry.

use super::estimate::{self, UsageTracker};
use super::history::{
    HistoryRecordEntry, HistoryStore, PlannedReplacement, Rewrite, RewriteError, RewriteKind,
};
use super::micro::{COLD_CACHE_IDLE_MS, MicroCompactGate, MicroCompactPlan};
use super::summary::{self, SUMMARY_MAX_TOKENS};
use super::trim::{ProtectedInputs, TrimResult, trim_frame};
use super::{CompactionAbandon, HistoryTrigger};
use super::{sealed, segment};
use crate::definition::SessionDefinition;
use crate::session::ConversationId;
use crate::usage::UsageHub;
use causa::kernel::{
    BlockContent, BlockId, CallControl, Context, ContextBlock, GenerationOptions, InvocationId,
    ModelGateway, ModelRef, ModelRequest, RoundId, TurnId,
};
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Share of the window a brief request is trimmed to when its material is
/// over the whole window.
const TRIM_TARGET_PERCENT: u64 = 70;

type ClockFn = Arc<dyn Fn() -> u64 + Send + Sync>;

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
    last_call_source: OnceLock<Arc<UsageHub>>,
    clock: Mutex<ClockFn>,
    force: AtomicBool,
    current_work_inputs: ProtectedInputs,
    prefix_too_large_reported: AtomicBool,
    rejected_brief_bytes: Mutex<Option<u64>>,
    gateway: Arc<dyn ModelGateway>,
    model: ModelRef,
    generation: GenerationOptions,
}

impl CompactionRuntime {
    /// Build the per-session runtime for standalone compaction unit tests.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        definition: &SessionDefinition,
        conversation_id: &ConversationId,
        store_root: Option<&Path>,
        history: Vec<HistoryRecordEntry>,
        current_auto_compact: Option<bool>,
        current_threshold: Option<u8>,
        gateway: Arc<dyn ModelGateway>,
        usage: Arc<UsageTracker>,
    ) -> Option<Arc<Self>> {
        let store_root = store_root?;
        let history = Arc::new(HistoryStore::new(
            store_root,
            conversation_id.as_str(),
            history,
        ));
        Some(Self::with_history(
            definition,
            history,
            current_auto_compact,
            current_threshold,
            gateway,
            usage,
        ))
    }

    /// Build the per-session runtime with strict SQLite foreign-key enforcement.
    #[allow(clippy::too_many_arguments, dead_code)]
    pub(crate) fn new_strict(
        definition: &SessionDefinition,
        conversation_id: &ConversationId,
        store_root: Option<&Path>,
        history: Vec<HistoryRecordEntry>,
        current_auto_compact: Option<bool>,
        current_threshold: Option<u8>,
        gateway: Arc<dyn ModelGateway>,
        usage: Arc<UsageTracker>,
    ) -> Option<Arc<Self>> {
        Self::new_strict_for_agent(
            definition,
            conversation_id,
            conversation_id.as_str(),
            store_root,
            history,
            current_auto_compact,
            current_threshold,
            gateway,
            usage,
        )
    }

    /// Build a per-agent runtime with strict SQLite foreign-key enforcement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_strict_for_agent(
        definition: &SessionDefinition,
        conversation_id: &ConversationId,
        agent_id: &str,
        store_root: Option<&Path>,
        history: Vec<HistoryRecordEntry>,
        current_auto_compact: Option<bool>,
        current_threshold: Option<u8>,
        gateway: Arc<dyn ModelGateway>,
        usage: Arc<UsageTracker>,
    ) -> Option<Arc<Self>> {
        let store_root = store_root?;
        let history = Arc::new(HistoryStore::new_strict_for_agent(
            store_root,
            conversation_id.as_str(),
            agent_id,
            history,
        ));
        Some(Self::with_history(
            definition,
            history,
            current_auto_compact,
            current_threshold,
            gateway,
            usage,
        ))
    }

    fn with_history(
        definition: &SessionDefinition,
        history: Arc<HistoryStore>,
        current_auto_compact: Option<bool>,
        current_threshold: Option<u8>,
        gateway: Arc<dyn ModelGateway>,
        usage: Arc<UsageTracker>,
    ) -> Arc<Self> {
        let options = definition.run_options();
        Arc::new(Self {
            settings: Mutex::new(Settings {
                auto_compact: current_auto_compact.unwrap_or(definition.auto_compact),
                threshold: current_threshold.unwrap_or(definition.compact_threshold),
            }),
            window: definition.context_window,
            max_tokens: definition.max_tokens,
            prompt_cache: definition.prompt_cache,
            history: Some(history),
            usage,
            last_call_source: OnceLock::new(),
            clock: Mutex::new(Arc::new(super::history::now_ms)),
            force: AtomicBool::new(false),
            current_work_inputs: ProtectedInputs::default(),
            prefix_too_large_reported: AtomicBool::new(false),
            rejected_brief_bytes: Mutex::new(None),
            gateway,
            model: options.model,
            generation: options.generation,
        })
    }

    /// Attach the session's usage hub so micro-compaction can read `last_call` timestamps.
    pub(crate) fn attach_usage_hub(&self, hub: Arc<UsageHub>) {
        let _ = self.last_call_source.set(hub);
    }

    /// Timestamp in Unix milliseconds of the most recent successful model call, if any.
    pub(crate) fn last_call_at_unix_ms(&self) -> Option<u64> {
        self.last_call_source
            .get()
            .and_then(|hub| hub.last_call_at_unix_ms())
    }

    fn now_ms(&self) -> u64 {
        let clock = self.clock.lock().expect("clock lock poisoned").clone();
        clock()
    }

    fn is_cache_cold(&self) -> bool {
        let Some(last_call_ms) = self.last_call_at_unix_ms() else {
            return false;
        };
        self.now_ms().saturating_sub(last_call_ms) >= COLD_CACHE_IDLE_MS
    }

    /// Override the wall-clock source for deterministic tests.
    #[cfg(test)]
    pub(crate) fn set_clock(&self, clock: ClockFn) {
        *self.clock.lock().expect("clock lock poisoned") = clock;
    }

    /// Builder-style clock override for unit tests.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn with_clock(self: Arc<Self>, clock: ClockFn) -> Arc<Self> {
        self.set_clock(clock);
        self
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

    /// Artifact store backed by this session's rewrite records and `.artifacts/` directory.
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
        self.current_work_inputs.set(blocks);
    }

    /// Stop protecting the finished work's input blocks.
    pub(crate) fn clear_current_work_inputs(&self) {
        self.current_work_inputs.clear();
    }

    fn current_work_inputs(&self) -> HashSet<BlockId> {
        self.current_work_inputs.snapshot()
    }

    /// Temporarily trim a copy of the frame when `estimate_before + max_tokens > window`.
    pub(crate) fn trim_frame_if_needed(
        &self,
        context: &Context,
        estimate_before: u64,
    ) -> Option<TrimResult> {
        let window = self.window?;
        let protected = self.current_work_inputs();
        trim_frame(
            context.blocks(),
            window,
            self.max_tokens,
            estimate_before,
            &protected,
        )
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

    /// Whether an automatic summary would not shrink the Context.
    ///
    /// True when an earlier automatic summary was rejected as `no_shrink`
    /// after its model call and a brief of that size would still not make
    /// the planned edit smaller. The check needs no model call; once the
    /// interval grows past the rejected brief, summaries are tried again. A
    /// summary that cannot be planned at all is left to [`Self::plan`].
    pub(crate) fn summary_cannot_shrink(&self, context: &Context) -> bool {
        let Some(rejected) = *self
            .rejected_brief_bytes
            .lock()
            .expect("rejected brief size has no callbacks")
        else {
            return false;
        };
        let blocks = context.blocks();
        let Ok(interval) = self.summary_interval(blocks) else {
            return false;
        };
        // Size the edit with an empty brief, then count the rejected brief
        // in its place.
        let empty = summary::summary_block(summary::new_block_id(), "");
        let empty_bytes = segment::block_bytes(&empty);
        let (_, threshold) = self.settings();
        let (removed, inserted) = sealed::replaced_bytes(
            blocks,
            &build_summary_replacements(blocks, &interval, empty, self.window, threshold),
        );
        inserted
            .saturating_sub(empty_bytes)
            .saturating_add(rejected)
            >= removed
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

    /// Plan a zero-LLM micro-compaction on Path A (`Auto` or `Passive` trigger).
    pub(crate) fn plan_micro_compact_hot(
        &self,
        context: &Context,
        trigger: HistoryTrigger,
    ) -> Option<MicroCompactPlan> {
        self.history.as_ref()?;
        let window = self.window?;
        let (_, threshold) = self.settings();
        let trigger_line =
            u64::try_from(u128::from(window) * u128::from(threshold) / 100).unwrap_or(u64::MAX);
        let cache_is_cold = self.is_cache_cold();
        let estimate_before = self.usage.estimate(estimate::context_bytes(context));
        let current_inputs = self.current_work_inputs();
        super::micro::plan(
            context.blocks(),
            Some(window),
            threshold,
            &current_inputs,
            estimate_before,
            trigger,
            MicroCompactGate::Hot {
                trigger_line,
                window,
                max_tokens: self.max_tokens,
                cache_is_cold,
            },
        )
    }

    /// Plan an opportunistic cold-cache micro-compaction on Path B.
    pub(crate) fn plan_micro_compact_cold(&self, context: &Context) -> Option<MicroCompactPlan> {
        self.history.as_ref()?;
        let window = self.window?;
        let (auto_compact, threshold) = self.settings();
        if !auto_compact || self.prefix_too_large_reported() {
            return None;
        }
        let estimate_before = self.usage.estimate(estimate::context_bytes(context));
        let current_inputs = self.current_work_inputs();
        super::micro::plan(
            context.blocks(),
            Some(window),
            threshold,
            &current_inputs,
            estimate_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Cold {
                last_call_at_ms: self.last_call_at_unix_ms(),
                now_ms: self.now_ms(),
            },
        )
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
        let interval = self.summary_interval(blocks)?;
        let SummaryInterval {
            head_end,
            k,
            ref protected,
            request_in_progress,
            promoted_skills: _,
        } = interval;

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
        let mut request_blocks = summary::request_material(blocks, head_end, k, protected, limit);
        request_blocks.push(summary::instruction_block(summary::instruction(
            request_in_progress,
        )));
        let text = self
            .brief(request_blocks, model, generation, control)
            .await?;

        let record_id = super::history::new_record_id();
        let brief = summary::summary_block(
            summary::new_block_id(),
            &summary::summary_text(&text, &record_id),
        );
        let brief_bytes = segment::block_bytes(&brief);
        let (_, threshold) = self.settings();
        let replacements =
            build_summary_replacements(blocks, &interval, brief, self.window, threshold);
        let rewrite = self.rewrite(
            context,
            record_id,
            trigger,
            RewriteKind::Summarize,
            replacements,
        );
        // Later automatic rounds skip the call until the interval outgrows
        // this brief; see `summary_cannot_shrink`.
        if trigger == HistoryTrigger::Auto && matches!(rewrite, Err(CompactionAbandon::NoShrink)) {
            *self
                .rejected_brief_bytes
                .lock()
                .expect("rejected brief size has no callbacks") = Some(brief_bytes);
        }
        rewrite
    }

    /// The interval a summary would replace, or why there is none.
    ///
    /// It runs from the end of the head to the cut point `k` that keeps the
    /// newest material within the retained-tail budget, outside a tool round
    /// whose replies carry sealed items; the running work's inputs inside it
    /// are protected.
    fn summary_interval(
        &self,
        blocks: &[ContextBlock],
    ) -> Result<SummaryInterval, CompactionAbandon> {
        let (_, threshold) = self.settings();
        let current_inputs = self.current_work_inputs();
        let interval = compute_interval_bounds(blocks, self.window, threshold, &current_inputs)?;
        // Protected blocks stay and declarations are only folded, so an
        // interval of nothing else has nothing to summarize.
        let summarizable = (interval.head_end..interval.k).any(|index| {
            !interval.protected.contains(&index)
                && !matches!(blocks[index].content(), BlockContent::ToolChange(_))
        });
        if !summarizable {
            return Err(CompactionAbandon::NoShrink);
        }
        Ok(interval)
    }

    /// Whether the brief chain is due to be merged after a summary.
    pub(crate) fn needs_merge(&self, context: &Context) -> bool {
        let (_, threshold) = self.settings();
        self.history.is_some()
            && segment::chain_needs_merge(context.blocks(), self.window, threshold)
    }

    /// Plan the merge of the brief chain into one brief.
    ///
    /// The request is the head (fixed prefix and chain) followed by the merge
    /// instruction. The chain and the tool declarations between its briefs
    /// are replaced by one declaration of their changes and the merged brief,
    /// whose last line lists a recall id for each brief it replaces. The
    /// fixed prefix is untouched.
    pub(crate) async fn plan_merge(
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
        let (start, end) = (segment::fixed_prefix_end(blocks), segment::head_end(blocks));
        let briefs: Vec<BlockId> = segment::chain_briefs(blocks)
            .into_iter()
            .map(ContextBlock::id)
            .collect();
        if briefs.len() < 2 {
            return Err(CompactionAbandon::NoShrink);
        }
        let mut request_blocks = blocks[..end].to_vec();
        request_blocks.push(summary::instruction_block(summary::merge_instruction()));
        let text = self
            .brief(request_blocks, model, generation, control)
            .await?;

        let record_id = super::history::new_record_id();
        let merged = summary::summary_block(
            summary::new_block_id(),
            &summary::merged_text(&text, &record_id, &briefs),
        );
        let mut new_blocks: Vec<ContextBlock> =
            segment::tool_delta(&blocks[..start], &blocks[..end])
                .into_iter()
                .collect();
        new_blocks.push(merged);
        let replacements = sealed::strip_after(
            blocks,
            start,
            vec![PlannedReplacement {
                range: start..end,
                with: new_blocks,
            }],
        );
        self.rewrite(
            context,
            record_id,
            trigger,
            RewriteKind::Merge,
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
        // A reply that calls a tool did not finish a brief, even when some
        // text came before the call.
        if output.response.tool_calls().next().is_some() {
            return Err(CompactionAbandon::ToolCall);
        }
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
        // The Context changed shape, so the rejected brief no longer predicts
        // the next summary.
        *self
            .rejected_brief_bytes
            .lock()
            .expect("rejected brief size has no callbacks") = None;
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

    /// Plan the merge that follows a manual summary, on the Context the
    /// summary would leave. None when no merge is due or it was abandoned:
    /// the summary still goes ahead.
    pub(crate) async fn manual_merge_plan(
        &self,
        summary: &Rewrite,
        context: &Context,
        control: &CallControl,
    ) -> Option<Rewrite> {
        let after = summary.preview(context)?;
        if !self.needs_merge(&after) {
            return None;
        }
        self.plan_merge(
            &after,
            &self.model,
            &self.generation,
            HistoryTrigger::Manual,
            control,
        )
        .await
        .ok()
    }

    /// Undo a committed rewrite whose Context was then not saved.
    pub(crate) fn rollback_record(&self, record_id: &str) {
        if let Some(history) = &self.history {
            history.remove(record_id);
            history.remove_entry(record_id);
        }
    }
}

/// Where a summary or micro-compaction cuts: the end of the head, the tail
/// start `k`, the protected positions between them, and promoted `load_skill`
/// blocks.
pub(super) struct SummaryInterval {
    pub(super) head_end: usize,
    pub(super) k: usize,
    pub(super) protected: Vec<usize>,
    pub(super) request_in_progress: bool,
    pub(super) promoted_skills: Vec<(usize, ContextBlock)>,
}

/// Compute `head_end..k` and the protected positions inside it using the exact
/// boundary rules shared by `Summarize` and `MicroCompact`.
pub(super) fn compute_interval_bounds(
    blocks: &[ContextBlock],
    window: Option<u64>,
    threshold: u8,
    current_inputs: &HashSet<BlockId>,
) -> Result<SummaryInterval, CompactionAbandon> {
    if has_unpaired_tools(blocks) {
        return Err(CompactionAbandon::UnpairedMaterials);
    }
    let head_end = segment::head_end(blocks);
    let fixed_prefix = segment::fixed_prefix_estimate(blocks);
    let budget = segment::retained_tail_budget(window, threshold, fixed_prefix)
        .map_err(|_| CompactionAbandon::PrefixTooLarge)?;
    let k =
        sealed::tail_start_outside_open_loop(blocks, segment::tail_start(blocks, head_end, budget));
    let k = trim_trailing_work_inputs(blocks, head_end, k, current_inputs);
    let work_inputs = segment::protected_positions(blocks, head_end, k, current_inputs);
    let request_in_progress = !work_inputs.is_empty();
    let skill_budget = segment::skill_protection_budget(window, threshold, fixed_prefix);
    let (skill_protected, promoted_skills) =
        plan_skill_protection(blocks, head_end, k, skill_budget);
    let mut protected = work_inputs;
    protected.extend(skill_protected);
    protected.sort_unstable();
    protected.dedup();
    Ok(SummaryInterval {
        head_end,
        k,
        protected,
        request_in_progress,
        promoted_skills,
    })
}

enum SkillOccurrenceKind {
    Standalone,
    ToolResult { promoted_text: String },
}

struct SkillOccurrence {
    index: usize,
    name: String,
    kind: SkillOccurrenceKind,
}

fn plan_skill_protection(
    blocks: &[ContextBlock],
    head_end: usize,
    k: usize,
    budget_tokens: u64,
) -> (Vec<usize>, Vec<(usize, ContextBlock)>) {
    let mut occurrences = Vec::new();
    let mut latest_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (index, block) in blocks.iter().enumerate().skip(head_end) {
        if let Some((name, _sha256)) = crate::skills::parse_standalone_skill_block(block) {
            latest_by_name.insert(name.clone(), index);
            occurrences.push(SkillOccurrence {
                index,
                name,
                kind: SkillOccurrenceKind::Standalone,
            });
        } else if let Some((name, _sha256, promoted_text)) =
            crate::skills::parse_tool_result_skill(block)
        {
            latest_by_name.insert(name.clone(), index);
            occurrences.push(SkillOccurrence {
                index,
                name,
                kind: SkillOccurrenceKind::ToolResult { promoted_text },
            });
        }
    }

    let mut used_bytes = 0u64;
    let mut interval_candidates = Vec::new();
    for occ in occurrences {
        if latest_by_name.get(&occ.name) != Some(&occ.index) {
            continue;
        }
        if occ.index >= k {
            used_bytes = used_bytes.saturating_add(segment::block_bytes(&blocks[occ.index]));
        } else {
            interval_candidates.push(occ);
        }
    }

    let mut protected_indices = Vec::new();
    let mut promoted_skills = Vec::new();
    // Retain newest active skills first; once a skill exceeds the protection
    // budget, evict it and all older skills into the summary.
    for occ in interval_candidates.into_iter().rev() {
        match occ.kind {
            SkillOccurrenceKind::Standalone => {
                let bytes = segment::block_bytes(&blocks[occ.index]);
                let next_bytes = used_bytes.saturating_add(bytes);
                if estimate::estimate_bytes(next_bytes) <= budget_tokens {
                    used_bytes = next_bytes;
                    protected_indices.push(occ.index);
                } else {
                    break;
                }
            }
            SkillOccurrenceKind::ToolResult { promoted_text } => {
                let promoted_block = crate::skills::make_standalone_skill_block(promoted_text);
                let bytes = segment::block_bytes(&promoted_block);
                let next_bytes = used_bytes.saturating_add(bytes);
                if estimate::estimate_bytes(next_bytes) <= budget_tokens {
                    used_bytes = next_bytes;
                    promoted_skills.push((occ.index, promoted_block));
                } else {
                    break;
                }
            }
        }
    }

    protected_indices.sort_unstable();
    promoted_skills.sort_by_key(|(index, _)| *index);
    (protected_indices, promoted_skills)
}

/// The replacements of a summary over `interval` with `brief` standing for it,
/// plus an atomic fold of dynamic prelude updates in `head_end..k` back into
/// the dynamic prefix when present.
fn build_summary_replacements(
    blocks: &[ContextBlock],
    interval: &SummaryInterval,
    brief: ContextBlock,
    window: Option<u64>,
    threshold: u8,
) -> Vec<PlannedReplacement> {
    let has_updates = blocks[interval.head_end..interval.k].iter().any(|block| {
        block
            .meta()
            .source
            .as_deref()
            .is_some_and(crate::prelude::is_prelude_update_source)
    });
    if !has_updates {
        return summary_replacements(blocks, interval, brief);
    }

    let fixed_end = segment::fixed_prefix_end(blocks);
    let dynamic_start = blocks[..fixed_end]
        .iter()
        .position(segment::is_prelude_dynamic_prefix)
        .unwrap_or(fixed_end);

    let tools = causa::kernel::current_tools(blocks);
    let folded_tool_declaration_estimate = if tools.is_empty() {
        0
    } else {
        let decl = ContextBlock::new(
            BlockId::new(uuid::Uuid::nil()),
            BlockContent::ToolChange(causa::kernel::ToolChange::between(&[], &tools)),
            causa::kernel::BlockMeta::default(),
        );
        estimate::estimate_bytes(segment::block_bytes(&decl))
    };
    let budget = crate::prelude::PreludeBudgetConfig {
        context_window: window,
        compact_threshold: threshold,
        folded_tool_declaration_estimate,
    };
    let mut folded_dynamic = crate::prelude::fold_dynamic_prefix(blocks, interval.k, &budget);

    let existing_dynamic: Vec<&ContextBlock> = blocks[dynamic_start..fixed_end]
        .iter()
        .filter(|b| segment::is_prelude_dynamic_prefix(b))
        .collect();
    let unchanged = existing_dynamic.len() == folded_dynamic.len()
        && existing_dynamic
            .iter()
            .zip(&folded_dynamic)
            .all(|(old, new)| {
                old.meta().source == new.meta().source && old.content() == new.content()
            });
    if unchanged {
        return summary_replacements(blocks, interval, brief);
    }

    for block in &blocks[dynamic_start..fixed_end] {
        if let BlockContent::ToolChange(change) = block.content() {
            folded_dynamic.push(ContextBlock::new(
                summary::new_block_id(),
                BlockContent::ToolChange(change.clone()),
                block.meta().clone(),
            ));
        }
    }

    if dynamic_start < fixed_end {
        let prefix_replacement = if !folded_dynamic.is_empty() {
            Some(PlannedReplacement {
                range: dynamic_start..fixed_end,
                with: folded_dynamic,
            })
        } else if dynamic_start > 0 {
            // Synthetic edge case where all dynamic sections were removed and no `env` existed:
            // put the preceding static block back unchanged to remove `dynamic_start..fixed_end`.
            Some(PlannedReplacement {
                range: (dynamic_start - 1)..fixed_end,
                with: vec![blocks[dynamic_start - 1].clone()],
            })
        } else {
            None
        };
        let mut out = Vec::new();
        if let Some(rep) = prefix_replacement {
            out.push(rep);
        }
        out.extend(summary_replacements(blocks, interval, brief));
        return out;
    }

    // `dynamic_start == fixed_end`: no prior dynamic prefix blocks existed.
    if fixed_end < interval.head_end {
        let mut out = vec![PlannedReplacement {
            range: fixed_end..fixed_end,
            with: folded_dynamic,
        }];
        out.extend(summary_replacements(blocks, interval, brief));
        return out;
    }

    let mut new_blocks = folded_dynamic;
    new_blocks.extend(segment::tool_delta(
        &blocks[..interval.head_end],
        &blocks[..interval.k],
    ));
    new_blocks.push(brief);
    sealed::strip_after(
        blocks,
        interval.head_end,
        segment::replacements_with_skills(
            blocks,
            interval.head_end,
            interval.k,
            &interval.protected,
            &interval.promoted_skills,
            new_blocks,
        ),
    )
}

/// The replacements of a summary over `interval` with `brief` standing for it.
///
/// The interval's tool changes, folded into one declaration before the brief,
/// keep the frame's tool state exactly as it was; sealed items are removed
/// from the replies after the change.
fn summary_replacements(
    blocks: &[ContextBlock],
    interval: &SummaryInterval,
    brief: ContextBlock,
) -> Vec<PlannedReplacement> {
    let mut new_blocks: Vec<ContextBlock> =
        segment::tool_delta(&blocks[..interval.head_end], &blocks[..interval.k])
            .into_iter()
            .collect();
    new_blocks.push(brief);
    sealed::strip_after(
        blocks,
        interval.head_end,
        segment::replacements_with_skills(
            blocks,
            interval.head_end,
            interval.k,
            &interval.protected,
            &interval.promoted_skills,
            new_blocks,
        ),
    )
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
    /// Index entry of the merge that followed, when the brief chain was over
    /// its budget; `estimate_after` then describes the Context after it.
    pub merged: Option<HistoryRecordEntry>,
    /// The brief now standing for the replaced material: the new brief, or
    /// the merged one.
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

/// Clamp `k` before any trailing current-work inputs (and any trailing
/// `ToolChange` appended right after them before the first model round).
///
/// When a work submits a large input `U` on Round 0 before the model has seen
/// `U`, `segment::tail_start` can push `k` past `U` to `blocks.len()`. Keeping
/// `U` in the retained tail `blocks[k..]` excludes the uncached `U` from the
/// brief request and keeps it verbatim after the brief.
fn trim_trailing_work_inputs(
    blocks: &[ContextBlock],
    head_end: usize,
    k: usize,
    current_inputs: &HashSet<BlockId>,
) -> usize {
    if current_inputs.is_empty() {
        return k;
    }
    let mut cursor = k;
    let mut saw_work_input = false;
    while cursor > head_end {
        let block = &blocks[cursor - 1];
        if current_inputs.contains(&block.id()) {
            saw_work_input = true;
            cursor -= 1;
        } else if !saw_work_input && matches!(block.content(), BlockContent::ToolChange(_)) {
            cursor -= 1;
        } else {
            break;
        }
    }
    if saw_work_input && (cursor == head_end || segment::is_cut_point(blocks, cursor)) {
        cursor
    } else {
        k
    }
}
