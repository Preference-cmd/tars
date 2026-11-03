//! Public types and calculations for model call usage and session statistics.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Whole Unix milliseconds of `time`; a time before the epoch counts as 0.
pub(crate) fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// The time `millis` Unix milliseconds after the epoch, or `None` when this
/// platform cannot represent it.
pub(crate) fn from_unix_millis(millis: u64) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::from_millis(millis))
}

/// Token counts and derived totals for a single model call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallUsage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cached tokens read from prompt cache, or `None` if unreported.
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to prompt cache, or `None` if unreported.
    pub cache_write_tokens: Option<u64>,
    /// Output tokens produced by the model.
    pub output_tokens: u64,
    /// Reasoning tokens (a subset of output tokens), or `None` if unreported.
    pub reasoning_tokens: Option<u64>,
}

impl CallUsage {
    /// Construct from Causa's raw [`causa::kernel::ModelUsage`].
    pub fn from_model_usage(usage: &causa::kernel::ModelUsage) -> Self {
        Self {
            input_tokens: u64::try_from(usage.input_tokens).unwrap_or(u64::MAX),
            cache_read_tokens: usage
                .cache_read_tokens
                .map(|t| u64::try_from(t).unwrap_or(u64::MAX)),
            cache_write_tokens: usage
                .cache_write_tokens
                .map(|t| u64::try_from(t).unwrap_or(u64::MAX)),
            output_tokens: u64::try_from(usage.output_tokens).unwrap_or(u64::MAX),
            reasoning_tokens: usage
                .reasoning_tokens
                .map(|t| u64::try_from(t).unwrap_or(u64::MAX)),
        }
    }

    /// Prompt token total: uncached input plus reported cache read and cache write tokens.
    pub fn prompt_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_write_tokens.unwrap_or(0))
    }

    /// Call token total: prompt total plus output tokens.
    ///
    /// Reasoning tokens are a subset of output tokens and are not added again.
    pub fn call_total(&self) -> u64 {
        self.prompt_total().saturating_add(self.output_tokens)
    }

    /// Cache hit rate: cache read tokens divided by prompt total.
    ///
    /// Returns `None` if cache read tokens were not reported or if the prompt total is 0.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let read = self.cache_read_tokens?;
        let prompt = self.prompt_total();
        if prompt == 0 {
            None
        } else {
            Some(read as f64 / prompt as f64)
        }
    }

    /// The figures to show for this call; the optional cache and reasoning
    /// counts stay `None` when the call did not report them.
    pub fn figures(&self) -> UsageFigures {
        UsageFigures {
            input_tokens: Some(self.input_tokens),
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: Some(self.output_tokens),
            reasoning_tokens: self.reasoning_tokens,
            prompt_tokens: Some(self.prompt_total()),
            total_tokens: Some(self.call_total()),
            cache_hit_rate: self.cache_hit_rate(),
        }
    }
}

/// Token figures ready to show for one call or a group of calls.
///
/// A field is `None` when no counted call reported it, so a value that was
/// never reported is never shown as 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageFigures {
    /// Uncached input tokens.
    pub input_tokens: Option<u64>,
    /// Tokens read from the prompt cache.
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to the prompt cache.
    pub cache_write_tokens: Option<u64>,
    /// Output tokens.
    pub output_tokens: Option<u64>,
    /// Reasoning tokens, a part of the output tokens.
    pub reasoning_tokens: Option<u64>,
    /// Prompt total: input plus cache read plus cache write.
    pub prompt_tokens: Option<u64>,
    /// Call total: prompt total plus output.
    pub total_tokens: Option<u64>,
    /// Cache read tokens over the prompt total of the calls that reported them.
    pub cache_hit_rate: Option<f64>,
}

/// Purpose for which a model call was made.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsagePurpose {
    /// Regular conversation turn or tool round.
    Conversation,
    /// Context compaction summary generation.
    Compaction,
}

impl UsagePurpose {
    /// The lowercase name shown in CLI and TUI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Compaction => "compaction",
        }
    }
}

/// Call counts categorized by outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallCounts {
    /// Number of successful calls.
    pub succeeded: u64,
    /// Number of failed calls (an invoke error, stream error, or stream dropped before completion).
    pub failed: u64,
    /// Number of successful calls that did not report usage (`usage == None`).
    pub unreported: u64,
}

impl CallCounts {
    /// Total call attempts (succeeded + failed).
    pub fn total(&self) -> u64 {
        self.succeeded.saturating_add(self.failed)
    }

    /// Combine two call counts with saturating addition.
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            succeeded: self.succeeded.saturating_add(other.succeeded),
            failed: self.failed.saturating_add(other.failed),
            unreported: self.unreported.saturating_add(other.unreported),
        }
    }
}

/// Aggregated token totals across multiple model calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    /// Total uncached input tokens across reported calls.
    pub input_tokens: u64,
    /// Total output tokens across reported calls.
    pub output_tokens: u64,
    /// Sum of reported cache read tokens.
    pub cache_read_tokens: u64,
    /// Number of successful calls where cache read tokens were not reported.
    pub cache_read_unreported: u64,
    /// Prompt total of the calls that did report cache read tokens; the
    /// denominator of [`UsageTotals::cache_hit_rate`].
    pub cache_read_reported_prompt_tokens: u64,
    /// Sum of reported cache write tokens.
    pub cache_write_tokens: u64,
    /// Number of successful calls where cache write tokens were not reported.
    pub cache_write_unreported: u64,
    /// Sum of reported reasoning tokens (a subset of output tokens).
    pub reasoning_tokens: u64,
    /// Number of successful calls where reasoning tokens were not reported.
    pub reasoning_unreported: u64,
}

impl UsageTotals {
    /// Prompt token total: input tokens + cache read tokens + cache write tokens.
    pub fn prompt_total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Call token total: prompt total + output tokens.
    pub fn call_total(&self) -> u64 {
        self.prompt_total().saturating_add(self.output_tokens)
    }

    /// Cache hit rate over the calls that reported cache reads: their read
    /// tokens divided by their prompt total.
    ///
    /// Calls that did not report cache reads are left out, so they neither
    /// dilute the rate nor hide a reported 0%. Returns `None` when no call
    /// reported cache reads (or their prompt total is 0).
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let prompt = self.cache_read_reported_prompt_tokens;
        (prompt > 0).then(|| self.cache_read_tokens as f64 / prompt as f64)
    }

    /// Record a single call's usage figures into these totals.
    pub fn record_call(&mut self, call: &CallUsage) {
        self.input_tokens = self.input_tokens.saturating_add(call.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(call.output_tokens);
        match call.cache_read_tokens {
            Some(read) => {
                self.cache_read_tokens = self.cache_read_tokens.saturating_add(read);
                self.cache_read_reported_prompt_tokens = self
                    .cache_read_reported_prompt_tokens
                    .saturating_add(call.prompt_total());
            }
            None => self.cache_read_unreported = self.cache_read_unreported.saturating_add(1),
        }
        match call.cache_write_tokens {
            Some(write) => self.cache_write_tokens = self.cache_write_tokens.saturating_add(write),
            None => self.cache_write_unreported = self.cache_write_unreported.saturating_add(1),
        }
        match call.reasoning_tokens {
            Some(reasoning) => {
                self.reasoning_tokens = self.reasoning_tokens.saturating_add(reasoning)
            }
            None => self.reasoning_unreported = self.reasoning_unreported.saturating_add(1),
        }
    }

    /// Combine two token totals with saturating addition.
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_read_unreported: self
                .cache_read_unreported
                .saturating_add(other.cache_read_unreported),
            cache_read_reported_prompt_tokens: self
                .cache_read_reported_prompt_tokens
                .saturating_add(other.cache_read_reported_prompt_tokens),
            cache_write_tokens: self
                .cache_write_tokens
                .saturating_add(other.cache_write_tokens),
            cache_write_unreported: self
                .cache_write_unreported
                .saturating_add(other.cache_write_unreported),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            reasoning_unreported: self
                .reasoning_unreported
                .saturating_add(other.reasoning_unreported),
        }
    }
}

/// Combined call counts and token totals for a specific purpose or combined total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurposeUsage {
    /// Call counts.
    pub calls: CallCounts,
    /// Token totals.
    pub tokens: UsageTotals,
}

impl PurposeUsage {
    /// The figures to show for these calls.
    ///
    /// Input, output, prompt and total are `None` when no successful call
    /// reported usage. Cache read, cache write and reasoning are `None` when
    /// every call that reported usage left that count out; otherwise they are
    /// the sum of the reported values.
    pub fn figures(&self) -> UsageFigures {
        let reporting = self.calls.succeeded.saturating_sub(self.calls.unreported);
        if reporting == 0 {
            return UsageFigures::default();
        }
        let tokens = &self.tokens;
        let optional = |sum: u64, unreported: u64| (unreported < reporting).then_some(sum);
        UsageFigures {
            input_tokens: Some(tokens.input_tokens),
            cache_read_tokens: optional(tokens.cache_read_tokens, tokens.cache_read_unreported),
            cache_write_tokens: optional(tokens.cache_write_tokens, tokens.cache_write_unreported),
            output_tokens: Some(tokens.output_tokens),
            reasoning_tokens: optional(tokens.reasoning_tokens, tokens.reasoning_unreported),
            prompt_tokens: Some(tokens.prompt_total()),
            total_tokens: Some(tokens.call_total()),
            cache_hit_rate: tokens.cache_hit_rate(),
        }
    }

    /// Combine two purpose usages with saturating addition.
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            calls: self.calls.saturating_add(&other.calls),
            tokens: self.tokens.saturating_add(&other.tokens),
        }
    }
}

/// Facts about the most recent successful model call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastCall {
    /// Purpose of the call (conversation or compaction).
    pub purpose: UsagePurpose,
    /// Raw usage reported by the provider, or `None` if unreported.
    pub usage: Option<CallUsage>,
    /// Wall-clock timestamp when the call completed.
    pub timestamp: SystemTime,
}

impl LastCall {
    /// The figures to show for this call; all `None` when it reported no usage.
    pub fn figures(&self) -> UsageFigures {
        self.usage.map(|usage| usage.figures()).unwrap_or_default()
    }

    /// When the call completed, in whole Unix milliseconds.
    pub fn at_unix_ms(&self) -> u64 {
        unix_millis(self.timestamp)
    }
}

/// Complete session-wide usage statistics across all purposes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    /// Usage from conversation turns.
    pub conversation: PurposeUsage,
    /// Usage from compaction summaries.
    pub compaction: PurposeUsage,
    /// Number of works that entered the runner (`WorkResult::Run(_)`).
    pub input_rounds: u64,
    /// The most recent successful model call, if any.
    pub last_call: Option<LastCall>,
}

/// Usage statistics attributed to a single work.
///
/// Within a single session, works execute sequentially one at a time. All model
/// calls that take place while a work is active (including any automatic compaction
/// summaries triggered during that work's runner rounds) are attributed to that
/// work in progress.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkUsage {
    /// Conversation usage for this work.
    pub conversation: PurposeUsage,
    /// Compaction usage occurring during this work.
    pub compaction: PurposeUsage,
    /// Model rounds executed in this work (successful conversation calls).
    pub model_rounds: u64,
    /// The most recent successful model call made during this work, if any.
    pub last_call: Option<LastCall>,
}

impl SessionUsage {
    /// Combined usage across conversation and compaction.
    pub fn combined(&self) -> PurposeUsage {
        self.conversation.saturating_add(&self.compaction)
    }
}

impl WorkUsage {
    /// Combined usage across conversation and compaction for this work.
    pub fn combined(&self) -> PurposeUsage {
        self.conversation.saturating_add(&self.compaction)
    }
}
