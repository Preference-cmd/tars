//! The per-work Causa `ContextPreparer`.
//!
//! Installed once per work at assembly; every logical model invocation asks it
//! for the frame. For persistent sessions, it decides whether this round
//! compacts, and on success replaces the Context before returning the frame; a
//! summary that leaves the brief chain over its budget is followed by a merge.
//! Whenever the full frame still exceeds the configured window (counting
//! `max_tokens`), whether because compaction was skipped, abandoned, off, or
//! insufficient, or because the session is transient, the preparer falls back
//! to temporary frame trimming on a copy of the frame without touching the
//! Context or history.

use super::CompactionProgress;
use super::estimate::{self, UsageTracker};
use super::runtime::CompactionRuntime;
use super::trim::{ProtectedInputs, trim_frame};
use crate::events::{InboxDrainHandle, TASK_NOTIFICATION_SOURCE};
use crate::session::ToolReporter;
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, CallControl, Context, ContextBlock, ContextFrame,
    ContextPreparer, EditError, PrepareError, Role, RoundInfo,
};
use std::sync::Arc;

/// One work's compaction decision point for persistent sessions.
pub(crate) struct CompactionPreparer {
    runtime: Arc<CompactionRuntime>,
    reporter: ToolReporter,
    loaded_skills: Option<Arc<crate::skills::LoadedSkillsState>>,
    inbox: Option<Arc<InboxDrainHandle>>,
}

impl CompactionPreparer {
    /// Bind the session's runtime to one accepted work.
    pub(crate) fn new(runtime: Arc<CompactionRuntime>, reporter: ToolReporter) -> Self {
        Self {
            runtime,
            reporter,
            loaded_skills: None,
            inbox: None,
        }
    }

    /// Attach the session's loaded-skills view so compactions clear evicted skills
    /// and refresh promoted `tars:skill` block IDs.
    pub(crate) fn with_loaded_skills(
        mut self,
        loaded_skills: Arc<crate::skills::LoadedSkillsState>,
    ) -> Self {
        self.loaded_skills = Some(loaded_skills);
        self
    }

    /// Attach the per-work inbox drain handle so queued task notifications
    /// are appended strictly after compaction evaluation.
    #[allow(dead_code)]
    pub(crate) fn with_inbox(mut self, inbox: Arc<InboxDrainHandle>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    fn report(&self, round: &RoundInfo<'_>, progress: CompactionProgress) {
        self.reporter
            .compaction(round.invocation_id.clone(), progress);
    }

    fn finish_frame(&self, context: &Context, round: &RoundInfo<'_>) -> ContextFrame {
        let bytes = estimate::context_bytes(context);
        let estimate_tokens = self.runtime.usage().estimate(bytes);
        if let Some(outcome) = self.runtime.trim_frame_if_needed(context, estimate_tokens)
            && outcome.trimmed
        {
            self.runtime.usage().clear_pending_prepare();
            self.report(
                round,
                CompactionProgress::Trimmed {
                    before_tokens: estimate_tokens,
                    after_tokens: outcome.after_tokens,
                },
            );
            return outcome.frame;
        }
        self.runtime.note_prepare(bytes);
        context.frame()
    }
}

#[async_trait::async_trait]
impl ContextPreparer for CompactionPreparer {
    async fn prepare(
        &self,
        context: &mut Context,
        round: &RoundInfo<'_>,
        control: &CallControl,
    ) -> Result<ContextFrame, PrepareError> {
        let bytes = estimate::context_bytes(context);
        let estimate_tokens = self.runtime.usage().estimate(bytes);
        // The fixed prefix never shrinks: once it was reported as too large,
        // later rounds skip rewriting without another attempt, and fall back
        // to temporary frame trimming when the frame exceeds the window.
        let trigger = (!self.runtime.prefix_too_large_reported())
            .then(|| self.runtime.trigger_for(estimate_tokens))
            .flatten();
        if let Some(trigger) = trigger {
            if let Some(plan) = self.runtime.plan_micro_compact_hot(context, trigger) {
                let (estimate_before, estimate_after) =
                    (plan.rewrite.estimate_before, plan.rewrite.estimate_after);
                let cleared_results = plan.cleared_results;
                match self.runtime.commit_rewrite(context, plan.rewrite) {
                    Ok(entry) => {
                        self.report(
                            round,
                            CompactionProgress::MicroCompacted {
                                trigger,
                                estimate_before,
                                estimate_after,
                                cleared_results,
                                record: entry.record_id,
                            },
                        );
                    }
                    Err(reason) => {
                        self.report(round, CompactionProgress::Abandoned { trigger, reason });
                    }
                }
            } else if !(trigger == super::HistoryTrigger::Auto
                && self.runtime.summary_cannot_shrink(context))
            {
                // An automatic summary rejected as `no_shrink` is not repeated
                // while a brief of its size still could not shrink the Context;
                // such rounds fall through to temporary trimming when needed.
                self.report(
                    round,
                    CompactionProgress::Started {
                        trigger,
                        estimate_before: estimate_tokens,
                    },
                );
                match self
                    .runtime
                    .plan(context, round.model, round.generation, trigger, control)
                    .await
                {
                    Ok(rewrite) => {
                        let (estimate_before, estimate_after) =
                            (rewrite.estimate_before, rewrite.estimate_after);
                        match self.runtime.commit_rewrite(context, rewrite) {
                            Ok(entry) => {
                                self.report(
                                    round,
                                    CompactionProgress::Finished {
                                        trigger,
                                        estimate_before,
                                        estimate_after,
                                        record: entry.record_id,
                                    },
                                );
                                if self.runtime.needs_merge(context) {
                                    self.merge(context, round, control, trigger).await;
                                }
                            }
                            Err(reason) => {
                                self.report(
                                    round,
                                    CompactionProgress::Abandoned { trigger, reason },
                                );
                            }
                        }
                    }
                    Err(reason) => {
                        if reason == super::CompactionAbandon::PrefixTooLarge {
                            self.runtime.note_prefix_too_large_reported();
                        }
                        self.report(round, CompactionProgress::Abandoned { trigger, reason });
                    }
                }
            }
        } else if let Some(plan) = self.runtime.plan_micro_compact_cold(context) {
            let (estimate_before, estimate_after) =
                (plan.rewrite.estimate_before, plan.rewrite.estimate_after);
            let cleared_results = plan.cleared_results;
            if let Ok(entry) = self.runtime.commit_rewrite(context, plan.rewrite) {
                self.report(
                    round,
                    CompactionProgress::MicroCompacted {
                        trigger: super::HistoryTrigger::Auto,
                        estimate_before,
                        estimate_after,
                        cleared_results,
                        record: entry.record_id,
                    },
                );
            }
        }
        if let Some(loaded) = &self.loaded_skills {
            loaded.sync_from_context(context.blocks());
        }
        append_inbox_notification(context, self.inbox.as_deref())?;
        Ok(self.finish_frame(context, round))
    }
}

/// One work's frame-trimming preparer for transient runs with a known window.
pub(crate) struct TransientTrimPreparer {
    window: u64,
    max_tokens: Option<u32>,
    usage: Arc<UsageTracker>,
    protected: Arc<ProtectedInputs>,
    reporter: ToolReporter,
    loaded_skills: Option<Arc<crate::skills::LoadedSkillsState>>,
    inbox: Option<Arc<InboxDrainHandle>>,
}

impl TransientTrimPreparer {
    /// Build a preparer that temporarily trims over-window frames without
    /// modifying Context or touching disk.
    pub(crate) fn new(
        window: u64,
        max_tokens: Option<u32>,
        usage: Arc<UsageTracker>,
        protected: Arc<ProtectedInputs>,
        reporter: ToolReporter,
    ) -> Self {
        Self {
            window,
            max_tokens,
            usage,
            protected,
            reporter,
            loaded_skills: None,
            inbox: None,
        }
    }

    /// Attach the session's loaded-skills view so round transitions keep exact
    /// committed `ToolResult` block IDs in sync.
    pub(crate) fn with_loaded_skills(
        mut self,
        loaded_skills: Arc<crate::skills::LoadedSkillsState>,
    ) -> Self {
        self.loaded_skills = Some(loaded_skills);
        self
    }

    /// Attach the per-work inbox drain handle so queued task notifications
    /// are appended before frame trimming.
    #[allow(dead_code)]
    pub(crate) fn with_inbox(mut self, inbox: Arc<InboxDrainHandle>) -> Self {
        self.inbox = Some(inbox);
        self
    }
}

#[async_trait::async_trait]
impl ContextPreparer for TransientTrimPreparer {
    async fn prepare(
        &self,
        context: &mut Context,
        round: &RoundInfo<'_>,
        _control: &CallControl,
    ) -> Result<ContextFrame, PrepareError> {
        if let Some(loaded) = &self.loaded_skills {
            loaded.sync_from_context(context.blocks());
        }
        append_inbox_notification(context, self.inbox.as_deref())?;
        let bytes = estimate::context_bytes(context);
        let estimate_tokens = self.usage.estimate(bytes);
        let protected = self.protected.snapshot();
        if let Some(outcome) = trim_frame(
            context.blocks(),
            self.window,
            self.max_tokens,
            estimate_tokens,
            &protected,
        ) && outcome.trimmed
        {
            self.usage.clear_pending_prepare();
            self.reporter.compaction(
                round.invocation_id.clone(),
                CompactionProgress::Trimmed {
                    before_tokens: estimate_tokens,
                    after_tokens: outcome.after_tokens,
                },
            );
            return Ok(outcome.frame);
        }
        self.usage.note_prepare_bytes(bytes);
        Ok(context.frame())
    }
}

impl CompactionPreparer {
    /// Merge the brief chain after a summary, reported as its own attempt.
    ///
    /// The summary is already committed; a merge that is abandoned leaves
    /// the chain as it is for the next summary to try again.
    async fn merge(
        &self,
        context: &mut Context,
        round: &RoundInfo<'_>,
        control: &CallControl,
        trigger: super::HistoryTrigger,
    ) {
        let estimate_tokens = self
            .runtime
            .usage()
            .estimate(estimate::context_bytes(context));
        self.report(
            round,
            CompactionProgress::Started {
                trigger,
                estimate_before: estimate_tokens,
            },
        );
        let committed = match self
            .runtime
            .plan_merge(context, round.model, round.generation, trigger, control)
            .await
        {
            Ok(rewrite) => {
                let estimates = (rewrite.estimate_before, rewrite.estimate_after);
                self.runtime
                    .commit_rewrite(context, rewrite)
                    .map(|entry| (entry, estimates))
            }
            Err(reason) => Err(reason),
        };
        let progress = match committed {
            Ok((entry, (estimate_before, estimate_after))) => CompactionProgress::Finished {
                trigger,
                estimate_before,
                estimate_after,
                record: entry.record_id,
            },
            Err(reason) => CompactionProgress::Abandoned { trigger, reason },
        };
        self.report(round, progress);
    }
}

fn append_inbox_notification(
    context: &mut Context,
    inbox: Option<&InboxDrainHandle>,
) -> Result<(), PrepareError> {
    let Some(inbox) = inbox else {
        return Ok(());
    };
    let Some(notification) = inbox.drain_notification() else {
        return Ok(());
    };
    loop {
        let block = ContextBlock::new(
            BlockId::new(uuid::Uuid::now_v7()),
            BlockContent::text(Role::User, notification.clone()),
            BlockMeta::from_source(TASK_NOTIFICATION_SOURCE),
        );
        match context.edit().append([block]).commit() {
            Ok(()) => return Ok(()),
            Err(failure) if matches!(failure.reason, EditError::DuplicateBlockId(_)) => continue,
            Err(failure) => return Err(PrepareError::new(failure.reason.to_string())),
        }
    }
}
