//! Conservative request-size estimation and upstream usage capture.
//!
//! Causa does not report the previous round's prompt size through `RoundInfo`,
//! so a host that wants to budget the window wraps its model gateway. The
//! estimate itself stays a host policy: one token per three serialized bytes is
//! deliberately pessimistic for non-Latin scripts.
//!
//! Causa feedback candidate (not yet requested): `RoundInfo` carrying the last
//! round's usage would remove the wrapper, but the compaction policy remains
//! TARS's.

use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelOutput, ModelRequest, ModelStream,
    StreamDelta,
};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

/// Serialized size of the current material, used as the estimator's base.
pub(crate) fn context_bytes(context: &Context) -> u64 {
    serde_json::to_vec(context)
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

/// Tokens estimated from serialized bytes; one token per three bytes, rounded up.
pub(crate) fn estimate_bytes(bytes: u64) -> u64 {
    bytes.div_ceil(3)
}

#[derive(Clone, Copy, Debug, Default)]
struct UsageState {
    last_input_tokens: Option<u64>,
    /// Size prepared for the round whose usage is `last_input_tokens`.
    last_prepare_bytes: Option<u64>,
    /// Size prepared for the round in flight, paired once its usage arrives.
    pending_prepare_bytes: Option<u64>,
}

/// Upstream-reported prompt size plus the size noted by the last preparation.
#[derive(Debug, Default)]
pub(crate) struct UsageTracker {
    state: Mutex<UsageState>,
}

impl UsageTracker {
    /// Remember the provider's prompt size for the round that just finished.
    pub(crate) fn record_output(&self, output: &ModelOutput) {
        let Some(usage) = &output.usage else {
            return;
        };
        let mut state = self.state.lock().expect("usage tracker has no callbacks");
        let Some(prepared) = state.pending_prepare_bytes.take() else {
            // Usage without a noted preparation cannot be paired with a size.
            return;
        };
        state.last_input_tokens = Some(u64::try_from(usage.input_tokens).unwrap_or(u64::MAX));
        state.last_prepare_bytes = Some(prepared);
    }

    /// Note the serialized size observed when a request was prepared.
    ///
    /// The size becomes the estimate's base only together with that round's
    /// usage: a round without usage keeps the older pair, so the material it
    /// added is still counted from bytes.
    pub(crate) fn note_prepare_bytes(&self, bytes: u64) {
        let mut state = self.state.lock().expect("usage tracker has no callbacks");
        state.pending_prepare_bytes = Some(bytes);
    }

    /// Estimate the next request's prompt size.
    ///
    /// With upstream usage the estimate is that usage plus the material added
    /// since preparation; without usage (a reopened or just-compacted session)
    /// everything is estimated from bytes.
    pub(crate) fn estimate(&self, current_bytes: u64) -> u64 {
        let state = *self.state.lock().expect("usage tracker has no callbacks");
        match state.last_input_tokens {
            Some(tokens) => {
                let base = state
                    .last_prepare_bytes
                    .unwrap_or(current_bytes)
                    .min(current_bytes);
                tokens.saturating_add(estimate_bytes(current_bytes - base))
            }
            None => estimate_bytes(current_bytes),
        }
    }

    /// Forget both facts; the next estimate is purely byte-based.
    pub(crate) fn reset(&self) {
        *self.state.lock().expect("usage tracker has no callbacks") = UsageState::default();
    }
}

/// Gateway wrapper recording each successful response's prompt usage.
pub(crate) struct UsageTrackingGateway {
    inner: Arc<dyn ModelGateway>,
    tracker: Arc<UsageTracker>,
}

impl UsageTrackingGateway {
    /// Wrap the actual gateway with usage capture.
    pub(crate) fn new(inner: Arc<dyn ModelGateway>, tracker: Arc<UsageTracker>) -> Self {
        Self { inner, tracker }
    }
}

#[async_trait::async_trait]
impl ModelGateway for UsageTrackingGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let output = self.inner.invoke(request, control).await?;
        self.tracker.record_output(&output);
        Ok(output)
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        let inner = self.inner.stream(request, control).await?;
        Ok(Box::pin(TrackedStream {
            inner,
            tracker: self.tracker.clone(),
        }))
    }
}

struct TrackedStream {
    inner: ModelStream,
    tracker: Arc<UsageTracker>,
}

impl futures_util::Stream for TrackedStream {
    type Item = StreamDelta;

    fn poll_next(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(StreamDelta::Done { final_output })) => {
                this.tracker.record_output(&final_output);
                Poll::Ready(Some(StreamDelta::Done { final_output }))
            }
            other => other,
        }
    }
}
