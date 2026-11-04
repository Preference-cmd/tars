//! The per-work Causa `ContextPreparer`.
//!
//! Installed once per work at assembly; every logical model invocation asks it
//! for the frame. It decides whether this round compacts, and on success
//! replaces the Context before returning the frame. Compaction is best effort:
//! any abandonment returns the original frame so the round still goes out.

use super::CompactionProgress;
use super::estimate;
use super::runtime::CompactionRuntime;
use crate::session::ToolReporter;
use causa::kernel::{CallControl, Context, ContextFrame, ContextPreparer, PrepareError, RoundInfo};
use std::sync::Arc;

/// One work's compaction decision point.
pub(crate) struct CompactionPreparer {
    runtime: Arc<CompactionRuntime>,
    reporter: ToolReporter,
}

impl CompactionPreparer {
    /// Bind the session's runtime to one accepted work.
    pub(crate) fn new(runtime: Arc<CompactionRuntime>, reporter: ToolReporter) -> Self {
        Self { runtime, reporter }
    }

    fn report(&self, round: &RoundInfo<'_>, progress: CompactionProgress) {
        self.reporter
            .compaction(round.invocation_id.clone(), progress);
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
        // later rounds go out unchanged without another attempt.
        let trigger = (!self.runtime.prefix_too_large_reported())
            .then(|| self.runtime.trigger_for(estimate_tokens))
            .flatten();
        let Some(trigger) = trigger else {
            self.runtime.note_prepare(bytes);
            return Ok(context.frame());
        };
        self.report(
            round,
            CompactionProgress::Started {
                trigger,
                estimate_before: estimate_tokens,
            },
        );
        let plan = self
            .runtime
            .plan(context, round.model, round.generation, trigger, control)
            .await;
        let rewrite = match plan {
            Ok(rewrite) => rewrite,
            Err(reason) => {
                if reason == super::CompactionAbandon::PrefixTooLarge {
                    self.runtime.note_prefix_too_large_reported();
                }
                self.report(round, CompactionProgress::Abandoned { trigger, reason });
                self.runtime.note_prepare(bytes);
                return Ok(context.frame());
            }
        };
        let (estimate_before, estimate_after) = (rewrite.estimate_before, rewrite.estimate_after);
        let entry = match self.runtime.commit_rewrite(context, rewrite) {
            Ok(entry) => entry,
            Err(reason) => {
                self.runtime.note_prepare(bytes);
                self.report(round, CompactionProgress::Abandoned { trigger, reason });
                return Ok(context.frame());
            }
        };
        self.runtime.note_prepare(estimate::context_bytes(context));
        self.report(
            round,
            CompactionProgress::Finished {
                trigger,
                estimate_before,
                estimate_after,
                record: entry.record_id,
            },
        );
        Ok(context.frame())
    }
}
