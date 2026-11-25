//! Bounded model retry gateway covering `invoke`, pre-first-delta `stream`,
//! and pre-`Done` mid-stream transient failures.

use async_trait::async_trait;
use causa::kernel::{
    CallControl, ControlError, InvocationId, ModelGateway, ModelInvokeError, ModelInvokeErrorKind,
    ModelOutput, ModelRequest, ModelStream, StreamDelta,
};
use causa::runtime::RetryPolicy;
use futures_util::StreamExt;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Optional callback invoked right before a mid-stream retry starts its next attempt.
pub(crate) type StreamRetryHook = Arc<dyn Fn(&InvocationId) + Send + Sync>;

/// [`ModelGateway`] wrapper enforcing [`RetryPolicy`] across `invoke`,
/// pre-first-delta `stream`, and pre-`Done` mid-stream transient failures.
#[derive(Clone)]
pub(crate) struct StreamRetryGateway {
    inner: Arc<dyn ModelGateway>,
    policy: RetryPolicy,
    on_stream_retry: Option<StreamRetryHook>,
}

impl std::fmt::Debug for StreamRetryGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRetryGateway")
            .field("policy", &self.policy)
            .field("has_stream_retry_hook", &self.on_stream_retry.is_some())
            .finish_non_exhaustive()
    }
}

impl StreamRetryGateway {
    /// Wrap `inner` with `policy`.
    pub(crate) fn new(inner: Arc<dyn ModelGateway>, policy: RetryPolicy) -> Self {
        Self {
            inner,
            policy,
            on_stream_retry: None,
        }
    }

    /// Attach a callback invoked when a mid-stream retry is about to start a
    /// new attempt after at least one non-terminal delta was already yielded.
    pub(crate) fn with_on_stream_retry(mut self, hook: StreamRetryHook) -> Self {
        self.on_stream_retry = Some(hook);
        self
    }

    fn pre_stream_retryable(&self, error: &ModelInvokeError, attempt: u32) -> bool {
        attempt < self.policy.max_attempts
            && matches!(
                error.kind,
                ModelInvokeErrorKind::Transient | ModelInvokeErrorKind::TimedOut
            )
    }

    fn mid_stream_retryable(&self, error: &ModelInvokeError, attempt: u32) -> bool {
        attempt < self.policy.max_attempts
            && matches!(
                error.kind,
                ModelInvokeErrorKind::Transient
                    | ModelInvokeErrorKind::TimedOut
                    | ModelInvokeErrorKind::UnknownOutcome
            )
    }

    async fn wait_retry(
        &self,
        attempt: u32,
        error: &ModelInvokeError,
        control: &CallControl,
    ) -> Result<(), ModelInvokeError> {
        let delay = match error.retry_delay {
            Some(server) if server <= self.policy.max_server_delay => server,
            Some(_) => return Err(error.clone()),
            None => backoff(&self.policy, attempt),
        };
        if let Some(deadline) = control.deadline()
            && Instant::now()
                .checked_add(delay)
                .is_none_or(|end| end >= deadline)
        {
            return Err(error.clone());
        }
        tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => {
                Err(control_error(ControlError::Cancelled))
            }
            _ = tokio::time::sleep(delay) => match control.check() {
                Ok(()) => Ok(()),
                Err(ControlError::Cancelled) => Err(control_error(ControlError::Cancelled)),
                Err(ControlError::TimedOut) => Err(error.clone()),
            },
        }
    }

    async fn open_attempt(
        &self,
        request: &ModelRequest,
        control: &CallControl,
        attempt: &mut u32,
    ) -> Result<(StreamDelta, ModelStream), ModelInvokeError> {
        loop {
            check_control(control)?;
            let first = under_control(control, async {
                let mut source = self.inner.stream(request, control).await?;
                Ok(source.next().await.map(|delta| (delta, source)))
            })
            .await;
            match first {
                Attempt::Stopped(error) => return Err(error),
                Attempt::Ran(Err(error)) => {
                    if !self.pre_stream_retryable(&error, *attempt) {
                        return Err(error);
                    }
                    self.wait_retry(*attempt, &error, control).await?;
                    *attempt += 1;
                }
                Attempt::Ran(Ok(None)) => {
                    return Err(ModelInvokeError::new(
                        ModelInvokeErrorKind::UnknownOutcome,
                        "model stream ended without Done before yielding any item",
                    ));
                }
                Attempt::Ran(Ok(Some((first, source)))) => {
                    if let StreamDelta::Error { error } = &first
                        && self.pre_stream_retryable(error, *attempt)
                    {
                        let error = error.clone();
                        drop(source);
                        self.wait_retry(*attempt, &error, control).await?;
                        *attempt += 1;
                        continue;
                    }
                    return Ok((first, source));
                }
            }
        }
    }
}

#[async_trait]
impl ModelGateway for StreamRetryGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let mut attempt = 1;
        loop {
            check_control(control)?;
            match under_control(control, self.inner.invoke(request, control)).await {
                Attempt::Stopped(error) => return Err(error),
                Attempt::Ran(Ok(output)) => return Ok(output),
                Attempt::Ran(Err(error)) => {
                    if !self.pre_stream_retryable(&error, attempt) {
                        return Err(error);
                    }
                    self.wait_retry(attempt, &error, control).await?;
                    attempt += 1;
                }
            }
        }
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        let mut attempt = 1;
        let (first, source) = self.open_attempt(request, control, &mut attempt).await?;
        let state = RetryStreamState {
            gateway: self.clone(),
            request: request.clone(),
            control: control.clone(),
            attempt,
            source: Some(source),
            pending_first: Some(first),
            finished: false,
        };
        Ok(Box::pin(futures_util::stream::unfold(state, next_delta)))
    }
}

struct RetryStreamState {
    gateway: StreamRetryGateway,
    request: ModelRequest,
    control: CallControl,
    attempt: u32,
    source: Option<ModelStream>,
    pending_first: Option<StreamDelta>,
    finished: bool,
}

async fn next_delta(mut state: RetryStreamState) -> Option<(StreamDelta, RetryStreamState)> {
    if state.finished {
        return None;
    }

    if let Some(first) = state.pending_first.take() {
        if matches!(first, StreamDelta::Done { .. } | StreamDelta::Error { .. }) {
            state.source = None;
            state.finished = true;
        }
        return Some((first, state));
    }

    let source = state.source.as_mut()?;
    if let Err(error) = check_control(&state.control) {
        state.source = None;
        state.finished = true;
        return Some((StreamDelta::Error { error }, state));
    }

    let next = tokio::select! {
        biased;
        cause = stop(&state.control) => Err(control_error(cause)),
        item = source.next() => Ok(item),
    };

    let item = match next {
        Err(error) => {
            state.source = None;
            state.finished = true;
            return Some((StreamDelta::Error { error }, state));
        }
        Ok(item) => item,
    };

    let error = match item {
        Some(StreamDelta::Done { final_output }) => {
            state.source = None;
            state.finished = true;
            return Some((StreamDelta::Done { final_output }, state));
        }
        Some(StreamDelta::Error { error }) => error,
        Some(delta) => return Some((delta, state)),
        None => ModelInvokeError::new(
            ModelInvokeErrorKind::UnknownOutcome,
            "model stream ended without Done",
        ),
    };

    // Drop the failed inner stream first so inner wrappers (CountingStream) settle cleanly.
    state.source = None;

    if let Err(control_err) = check_control(&state.control) {
        state.finished = true;
        return Some((StreamDelta::Error { error: control_err }, state));
    }

    if !state.gateway.mid_stream_retryable(&error, state.attempt) {
        state.finished = true;
        return Some((StreamDelta::Error { error }, state));
    }

    if let Err(wait_err) = state
        .gateway
        .wait_retry(state.attempt, &error, &state.control)
        .await
    {
        state.finished = true;
        return Some((StreamDelta::Error { error: wait_err }, state));
    }

    state.attempt += 1;
    if let Some(hook) = &state.gateway.on_stream_retry {
        hook(&state.request.invocation_id);
    }

    match state
        .gateway
        .open_attempt(&state.request, &state.control, &mut state.attempt)
        .await
    {
        Ok((first, next_source)) => {
            if matches!(first, StreamDelta::Done { .. } | StreamDelta::Error { .. }) {
                state.finished = true;
            } else {
                state.source = Some(next_source);
            }
            Some((first, state))
        }
        Err(open_err) => {
            state.finished = true;
            Some((StreamDelta::Error { error: open_err }, state))
        }
    }
}

fn backoff(policy: &RetryPolicy, attempt: u32) -> Duration {
    let doubling = 1u32 << attempt.saturating_sub(1).min(31);
    let capped = policy
        .base_delay
        .saturating_mul(doubling)
        .min(policy.max_delay);
    let factor = 0.75 + 0.25 * jitter_unit();
    Duration::from_nanos(((capped.as_nanos() as f64) * factor) as u64)
}

fn jitter_unit() -> f64 {
    let mask = (1u64 << 53) - 1;
    let bits = (uuid::Uuid::now_v7().as_u128() as u64) & mask;
    (bits as f64) / ((1u64 << 53) as f64)
}

enum Attempt<T> {
    Ran(Result<T, ModelInvokeError>),
    Stopped(ModelInvokeError),
}

async fn under_control<T>(
    control: &CallControl,
    work: impl Future<Output = Result<T, ModelInvokeError>>,
) -> Attempt<T> {
    tokio::select! {
        biased;
        cause = stop(control) => Attempt::Stopped(control_error(cause)),
        result = work => Attempt::Ran(result),
    }
}

async fn stop(control: &CallControl) -> ControlError {
    tokio::select! {
        biased;
        _ = control.cancellation_token().cancelled() => ControlError::Cancelled,
        _ = async {
            match control.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        } => ControlError::TimedOut,
    }
}

fn check_control(control: &CallControl) -> Result<(), ModelInvokeError> {
    control.check().map_err(control_error)
}

fn control_error(cause: ControlError) -> ModelInvokeError {
    match cause {
        ControlError::Cancelled => ModelInvokeError::new(
            ModelInvokeErrorKind::Cancelled,
            "model invocation cancelled",
        ),
        ControlError::TimedOut => ModelInvokeError::new(
            ModelInvokeErrorKind::TimedOut,
            "model invocation deadline exceeded",
        ),
    }
}
