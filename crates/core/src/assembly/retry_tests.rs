use super::provider::{SessionResources, assemble};
use super::retry::StreamRetryGateway;
use crate::compact::estimate::{UsageTracker, UsageTrackingGateway};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::session::{ProgressUpdate, SubmitRequest, WorkProgress, WorkResult, WorkState};
use crate::usage::{CountingGateway, UsageHub, UsagePurpose};
use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockMeta, CallControl, ContentPart, Context, ContextBlock, ContextFrame,
    InvocationId, ModelGateway, ModelInvokeError, ModelInvokeErrorKind, ModelItem,
    ModelItemContent, ModelOutput, ModelRef, ModelRequest, ModelResponse, ModelStopReason,
    ModelStream, ModelUsage, Role, RoundId, StreamDelta, Tool, ToolCallContext, ToolCallDraft,
    ToolDefinition, ToolOutput, ToolResultPayload, ToolResultStatus, TurnId,
};
use causa::runtime::RetryPolicy;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn test_policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy::new(max_attempts)
        .with_base_delay(Duration::from_millis(1))
        .with_max_delay(Duration::from_millis(4))
        .with_max_server_delay(Duration::from_millis(50))
}

fn test_request() -> ModelRequest {
    ModelRequest::new(
        InvocationId {
            turn_id: TurnId::new("turn-1"),
            round_id: RoundId(0),
        },
        ContextFrame { blocks: vec![] },
        ModelRef::new("test-model"),
    )
}

fn item_text(item: &ModelItem) -> Option<&str> {
    match &item.content {
        ModelItemContent::Parts(parts) => parts.iter().find_map(|part| match part {
            ContentPart::Text(payload) => Some(payload.0.as_str()),
            _ => None,
        }),
        _ => None,
    }
}

fn ok_output(text: &str, input_tokens: usize, output_tokens: usize) -> ModelOutput {
    let mut out = ModelOutput::new(
        ModelResponse {
            items: vec![ModelItem::text(text)],
        },
        ModelStopReason::EndTurn,
    );
    out.usage = Some(ModelUsage::new(input_tokens, output_tokens));
    out
}

fn err_with(kind: ModelInvokeErrorKind, msg: &str) -> ModelInvokeError {
    ModelInvokeError::new(kind, msg)
}

type StreamAction = Box<dyn FnOnce() -> Result<ModelStream, ModelInvokeError> + Send>;
type InvokeAction = Box<dyn FnOnce() -> Result<ModelOutput, ModelInvokeError> + Send>;

struct ScriptedGateway {
    invokes: Mutex<VecDeque<InvokeAction>>,
    streams: Mutex<VecDeque<StreamAction>>,
    calls: AtomicUsize,
}

impl ScriptedGateway {
    fn from_invokes(actions: Vec<InvokeAction>) -> Self {
        Self {
            invokes: Mutex::new(actions.into()),
            streams: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn from_streams(actions: Vec<StreamAction>) -> Self {
        Self {
            invokes: Mutex::new(VecDeque::new()),
            streams: Mutex::new(actions.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ModelGateway for ScriptedGateway {
    async fn invoke(
        &self,
        _request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let action = self
            .invokes
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted invoke action");
        action()
    }

    async fn stream(
        &self,
        _request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let action = self
            .streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted stream action");
        action()
    }
}

fn delta_stream(items: Vec<StreamDelta>) -> ModelStream {
    Box::pin(futures_util::stream::iter(items))
}

async fn collect_stream(mut stream: ModelStream) -> Vec<StreamDelta> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item);
    }
    out
}

#[tokio::test]
async fn invoke_retries_transient_and_timed_out_up_to_max_attempts() {
    let inner = Arc::new(ScriptedGateway::from_invokes(vec![
        Box::new(|| Err(err_with(ModelInvokeErrorKind::Transient, "503"))),
        Box::new(|| Err(err_with(ModelInvokeErrorKind::TimedOut, "timeout"))),
        Box::new(|| Ok(ok_output("recovered", 10, 5))),
    ]));
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3));
    let control = CallControl::new(CancellationToken::new(), None);

    let output = gateway.invoke(&test_request(), &control).await.unwrap();
    assert_eq!(inner.call_count(), 3);
    assert_eq!(item_text(&output.response.items[0]), Some("recovered"));
}

#[tokio::test]
async fn invoke_never_retries_permanent_invalid_undecodable_or_cancelled() {
    for kind in [
        ModelInvokeErrorKind::Permanent,
        ModelInvokeErrorKind::InvalidRequest,
        ModelInvokeErrorKind::Undecodable,
        ModelInvokeErrorKind::Cancelled,
        ModelInvokeErrorKind::UnknownOutcome,
    ] {
        let error_kind = kind.clone();
        let inner = Arc::new(ScriptedGateway::from_invokes(vec![
            Box::new(move || Err(err_with(error_kind, "fatal"))),
            Box::new(|| Ok(ok_output("must not run", 1, 1))),
        ]));
        let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3));
        let control = CallControl::new(CancellationToken::new(), None);

        let err = gateway.invoke(&test_request(), &control).await.unwrap_err();
        assert_eq!(err.kind, kind);
        assert_eq!(inner.call_count(), 1);
    }
}

#[tokio::test]
async fn pre_first_delta_stream_retries_open_and_initial_error_delta() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| Err(err_with(ModelInvokeErrorKind::Transient, "connect fail"))),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Error {
                error: err_with(ModelInvokeErrorKind::TimedOut, "first item timeout"),
            }]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta {
                    delta: "hello".into(),
                },
                StreamDelta::Done {
                    final_output: ok_output("hello", 12, 3),
                },
            ]))
        }),
    ]));
    let hooks = Arc::new(AtomicUsize::new(0));
    let hooks_ref = hooks.clone();
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3)).with_on_stream_retry(
        Arc::new(move |_| {
            hooks_ref.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let control = CallControl::new(CancellationToken::new(), None);

    let stream = gateway.stream(&test_request(), &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(inner.call_count(), 3);
    assert_eq!(
        hooks.load(Ordering::SeqCst),
        0,
        "pre-first-delta retries must not trigger mid-stream preview reset"
    );
    assert!(matches!(
        &items[..],
        [
            StreamDelta::TextDelta { delta },
            StreamDelta::Done { .. }
        ] if delta == "hello"
    ));
}

#[tokio::test]
async fn pre_first_delta_empty_eof_returns_unknown_outcome_without_retry() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| Ok(delta_stream(vec![]))),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3));
    let control = CallControl::new(CancellationToken::new(), None);

    let err = match gateway.stream(&test_request(), &control).await {
        Ok(_) => panic!("empty stream must fail with UnknownOutcome"),
        Err(err) => err,
    };
    assert_eq!(err.kind, ModelInvokeErrorKind::UnknownOutcome);
    assert_eq!(inner.call_count(), 1);
}

#[tokio::test]
async fn pre_first_delta_respects_server_retry_delay_ceiling() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Err(err_with(ModelInvokeErrorKind::Transient, "slow down")
                .with_retry_delay(Duration::from_millis(5)))
        }),
        Box::new(|| {
            Err(err_with(ModelInvokeErrorKind::Transient, "too long")
                .with_retry_delay(Duration::from_millis(200)))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(4));
    let control = CallControl::new(CancellationToken::new(), None);

    let err = match gateway.stream(&test_request(), &control).await {
        Ok(_) => panic!("excessive server retry_delay must fail immediately"),
        Err(err) => err,
    };
    assert_eq!(err.kind, ModelInvokeErrorKind::Transient);
    assert_eq!(err.message, "too long");
    assert_eq!(inner.call_count(), 2);
}

#[tokio::test]
async fn max_attempts_one_never_retries_invoke_pre_stream_or_mid_stream() {
    let invoke_inner = Arc::new(ScriptedGateway::from_invokes(vec![
        Box::new(|| Err(err_with(ModelInvokeErrorKind::Transient, "once"))),
        Box::new(|| Ok(ok_output("unreachable", 1, 1))),
    ]));
    let gateway = StreamRetryGateway::new(invoke_inner.clone(), test_policy(1));
    let control = CallControl::new(CancellationToken::new(), None);
    assert!(gateway.invoke(&test_request(), &control).await.is_err());
    assert_eq!(invoke_inner.call_count(), 1);

    let mid_inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta {
                    delta: "partial".into(),
                },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "drop"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let gateway = StreamRetryGateway::new(mid_inner.clone(), test_policy(1));
    let stream = gateway.stream(&test_request(), &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(mid_inner.call_count(), 1);
    assert!(matches!(
        &items[..],
        [
            StreamDelta::TextDelta { .. },
            StreamDelta::Error { error }
        ] if error.kind == ModelInvokeErrorKind::Transient
    ));
}

#[tokio::test]
async fn mid_stream_retries_after_text_and_reasoning_on_transient_error() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::ReasoningDelta {
                    delta: "bad thought".into(),
                },
                StreamDelta::TextDelta {
                    delta: "partial".into(),
                },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "sse reset"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::ReasoningDelta {
                    delta: "clean thought".into(),
                },
                StreamDelta::TextDelta {
                    delta: "final answer".into(),
                },
                StreamDelta::Done {
                    final_output: ok_output("final answer", 40, 12),
                },
            ]))
        }),
    ]));
    let hook_invocations = Arc::new(Mutex::new(Vec::new()));
    let hook_capture = hook_invocations.clone();
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(2)).with_on_stream_retry(
        Arc::new(move |id| {
            hook_capture.lock().unwrap().push(id.clone());
        }),
    );
    let req = test_request();
    let control = CallControl::new(CancellationToken::new(), None);

    let stream = gateway.stream(&req, &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(inner.call_count(), 2);
    assert_eq!(*hook_invocations.lock().unwrap(), vec![req.invocation_id]);
    assert_eq!(items.len(), 5);
    assert!(matches!(
        &items[4],
        StreamDelta::Done { final_output }
            if item_text(&final_output.response.items[0]) == Some("final answer")
    ));
}

#[tokio::test]
async fn mid_stream_retries_on_timed_out_unknown_outcome_and_premature_eof() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        // Attempt 1: TimedOut mid-stream
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "a1".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::TimedOut, "mid timeout"),
                },
            ]))
        }),
        // Attempt 2: UnknownOutcome mid-stream (Causa SSE premature EOF classification)
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "a2".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::UnknownOutcome, "sse malformed"),
                },
            ]))
        }),
        // Attempt 3: Raw premature None before Done
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::TextDelta {
                delta: "a3".into(),
            }]))
        }),
        // Attempt 4: Succeeds
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "ok".into() },
                StreamDelta::Done {
                    final_output: ok_output("ok", 20, 4),
                },
            ]))
        }),
    ]));
    let hooks = Arc::new(AtomicUsize::new(0));
    let hooks_ref = hooks.clone();
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(4)).with_on_stream_retry(
        Arc::new(move |_| {
            hooks_ref.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let control = CallControl::new(CancellationToken::new(), None);

    let stream = gateway.stream(&test_request(), &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(inner.call_count(), 4);
    assert_eq!(hooks.load(Ordering::SeqCst), 3);
    assert!(matches!(
        items.last(),
        Some(StreamDelta::Done { final_output })
            if item_text(&final_output.response.items[0]) == Some("ok")
    ));
}

#[tokio::test]
async fn mid_stream_retry_handles_pre_first_delta_failure_on_reconnect() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        // Attempt 1: yields delta then fails mid-stream
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta {
                    delta: "first".into(),
                },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "drop"),
                },
            ]))
        }),
        // Attempt 2: fails before yielding any delta (stays within max_attempts = 3)
        Box::new(|| Err(err_with(ModelInvokeErrorKind::Transient, "reconnect 502"))),
        // Attempt 3: succeeds
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("done on 3", 15, 5),
            }]))
        }),
    ]));
    let hooks = Arc::new(AtomicUsize::new(0));
    let hooks_ref = hooks.clone();
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3)).with_on_stream_retry(
        Arc::new(move |_| {
            hooks_ref.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let control = CallControl::new(CancellationToken::new(), None);

    let stream = gateway.stream(&test_request(), &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(inner.call_count(), 3);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    assert!(matches!(
        &items[..],
        [
            StreamDelta::TextDelta { .. },
            StreamDelta::Done { final_output }
        ] if item_text(&final_output.response.items[0]) == Some("done on 3")
    ));
}

#[tokio::test]
async fn mid_stream_deltas_are_forwarded_with_zero_buffering_latency() {
    let (tx, rx) = mpsc::unbounded_channel::<StreamDelta>();
    let inner_stream: ModelStream =
        Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
    let mut slot = Some(inner_stream);
    let inner = Arc::new(ScriptedGateway::from_streams(vec![Box::new(move || {
        Ok(slot.take().unwrap())
    })]));
    let gateway = StreamRetryGateway::new(inner, test_policy(2));
    let control = CallControl::new(CancellationToken::new(), None);

    tx.send(StreamDelta::TextDelta {
        delta: "immediate".into(),
    })
    .unwrap();
    let mut stream = gateway.stream(&test_request(), &control).await.unwrap();
    let first = tokio::time::timeout(Duration::from_millis(50), stream.next())
        .await
        .expect("first delta must arrive immediately without waiting for Done")
        .unwrap();
    assert!(matches!(first, StreamDelta::TextDelta { delta } if delta == "immediate"));

    tx.send(StreamDelta::Done {
        final_output: ok_output("immediate", 5, 1),
    })
    .unwrap();
    let second = stream.next().await.unwrap();
    assert!(matches!(second, StreamDelta::Done { .. }));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn mid_stream_never_retries_permanent_invalid_undecodable_or_cancelled() {
    for kind in [
        ModelInvokeErrorKind::Permanent,
        ModelInvokeErrorKind::InvalidRequest,
        ModelInvokeErrorKind::Undecodable,
        ModelInvokeErrorKind::Cancelled,
    ] {
        let error_kind = kind.clone();
        let inner = Arc::new(ScriptedGateway::from_streams(vec![
            Box::new(move || {
                Ok(delta_stream(vec![
                    StreamDelta::TextDelta {
                        delta: "partial".into(),
                    },
                    StreamDelta::Error {
                        error: err_with(error_kind, "non-retryable"),
                    },
                ]))
            }),
            Box::new(|| {
                Ok(delta_stream(vec![StreamDelta::Done {
                    final_output: ok_output("unreachable", 1, 1),
                }]))
            }),
        ]));
        let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3));
        let control = CallControl::new(CancellationToken::new(), None);

        let stream = gateway.stream(&test_request(), &control).await.unwrap();
        let items = collect_stream(stream).await;
        assert_eq!(inner.call_count(), 1);
        assert!(matches!(
            &items[..],
            [
                StreamDelta::TextDelta { .. },
                StreamDelta::Error { error }
            ] if error.kind == kind
        ));
    }
}

#[tokio::test]
async fn never_retries_after_done_even_if_buggy_stream_follows_with_error() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "ok".into() },
                StreamDelta::Done {
                    final_output: ok_output("ok", 10, 2),
                },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "post-done error"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(3));
    let control = CallControl::new(CancellationToken::new(), None);

    let stream = gateway.stream(&test_request(), &control).await.unwrap();
    let items = collect_stream(stream).await;
    assert_eq!(inner.call_count(), 1);
    assert_eq!(items.len(), 2);
    assert!(matches!(items[1], StreamDelta::Done { .. }));
}

#[tokio::test]
async fn mid_stream_exhausted_max_attempts_surfaces_last_error_and_eof() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "1".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "first fail"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "2".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::TimedOut, "last fail"),
                },
            ]))
        }),
    ]));
    let hooks = Arc::new(AtomicUsize::new(0));
    let hooks_ref = hooks.clone();
    let gateway = StreamRetryGateway::new(inner.clone(), test_policy(2)).with_on_stream_retry(
        Arc::new(move |_| {
            hooks_ref.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let control = CallControl::new(CancellationToken::new(), None);

    let items = collect_stream(gateway.stream(&test_request(), &control).await.unwrap()).await;
    assert_eq!(inner.call_count(), 2);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    assert!(matches!(
        items.last(),
        Some(StreamDelta::Error { error })
            if error.kind == ModelInvokeErrorKind::TimedOut && error.message == "last fail"
    ));

    // Exhausted premature EOF surfaces synthetic UnknownOutcome error.
    let eof_inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::TextDelta {
                delta: "1".into(),
            }]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::TextDelta {
                delta: "2".into(),
            }]))
        }),
    ]));
    let gateway = StreamRetryGateway::new(eof_inner.clone(), test_policy(2));
    let items = collect_stream(gateway.stream(&test_request(), &control).await.unwrap()).await;
    assert_eq!(eof_inner.call_count(), 2);
    assert!(matches!(
        items.last(),
        Some(StreamDelta::Error { error }) if error.kind == ModelInvokeErrorKind::UnknownOutcome
    ));
}

#[tokio::test]
async fn cancellation_during_backoff_wait_returns_cancelled_promptly() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "a".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "wait 5s"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let policy = RetryPolicy::new(2)
        .with_base_delay(Duration::from_secs(5))
        .with_max_delay(Duration::from_secs(5));
    let gateway = StreamRetryGateway::new(inner.clone(), policy);
    let token = CancellationToken::new();
    let control = CallControl::new(token.clone(), None);

    let mut stream = gateway.stream(&test_request(), &control).await.unwrap();
    assert!(matches!(
        stream.next().await,
        Some(StreamDelta::TextDelta { .. })
    ));

    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(15)).await;
        token.cancel();
    });

    let started = std::time::Instant::now();
    let terminal = stream.next().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(matches!(
        terminal,
        StreamDelta::Error { error } if error.kind == ModelInvokeErrorKind::Cancelled
    ));
    assert_eq!(inner.call_count(), 1);
    cancel_task.await.unwrap();
}

#[tokio::test]
async fn deadline_shorter_than_backoff_fails_immediately_without_sleeping() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta { delta: "a".into() },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "short deadline"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![StreamDelta::Done {
                final_output: ok_output("unreachable", 1, 1),
            }]))
        }),
    ]));
    let policy = RetryPolicy::new(2)
        .with_base_delay(Duration::from_secs(5))
        .with_max_delay(Duration::from_secs(5));
    let gateway = StreamRetryGateway::new(inner.clone(), policy);
    let deadline = std::time::Instant::now() + Duration::from_millis(100);
    let control = CallControl::new(CancellationToken::new(), Some(deadline));

    let started = std::time::Instant::now();
    let items = collect_stream(gateway.stream(&test_request(), &control).await.unwrap()).await;
    assert!(started.elapsed() < Duration::from_millis(80));
    assert_eq!(inner.call_count(), 1);
    assert!(matches!(
        items.last(),
        Some(StreamDelta::Error { error })
            if error.kind == ModelInvokeErrorKind::Transient && error.message == "short deadline"
    ));
}

#[tokio::test]
async fn counting_inside_and_usage_tracking_outside_match_mid_stream_retries() {
    let inner = Arc::new(ScriptedGateway::from_streams(vec![
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta {
                    delta: "aborted".into(),
                },
                StreamDelta::Usage(ModelUsage::new(999, 888)),
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "mid drop"),
                },
            ]))
        }),
        Box::new(|| {
            Ok(delta_stream(vec![
                StreamDelta::TextDelta {
                    delta: "clean".into(),
                },
                StreamDelta::Done {
                    final_output: ok_output("clean", 120, 30),
                },
            ]))
        }),
    ]));
    let hub = Arc::new(UsageHub::new());
    let tracker = Arc::new(UsageTracker::default());
    tracker.note_prepare_bytes(300);

    let counted: Arc<dyn ModelGateway> = Arc::new(CountingGateway::new(
        inner,
        UsagePurpose::Conversation,
        hub.clone(),
    ));
    let retried: Arc<dyn ModelGateway> = Arc::new(StreamRetryGateway::new(counted, test_policy(2)));
    let tracked = UsageTrackingGateway::new(retried, tracker.clone());

    let control = CallControl::new(CancellationToken::new(), None);
    let _items = collect_stream(tracked.stream(&test_request(), &control).await.unwrap()).await;

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.failed, 1);
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.total(), 2);
    assert_eq!(snapshot.conversation.tokens.input_tokens, 120);
    assert_eq!(snapshot.conversation.tokens.output_tokens, 30);

    // UsageTracker outside StreamRetryGateway recorded only the final Done prompt tokens (120).
    assert_eq!(tracker.estimate(300), 120);
}

struct SpyTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for SpyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "spy_tool",
            "Read-only spy tool",
            serde_json::json!({ "type": "object", "properties": {} }),
        )
    }

    fn metadata(&self) -> causa::kernel::ToolMetadata {
        crate::tools::managed_tool_metadata(Arc::new(crate::tools::ReadOnlyManagedTool))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        _control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        self.executions.fetch_add(1, Ordering::SeqCst);
        ToolResultPayload::new(
            call.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({ "ok": true })),
        )
    }
}

struct TwoRoundMidStreamGateway {
    round: AtomicUsize,
}

#[async_trait]
impl ModelGateway for TwoRoundMidStreamGateway {
    async fn invoke(
        &self,
        _request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        unreachable!("Session worker uses streaming")
    }

    async fn stream(
        &self,
        _request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        let step = self.round.fetch_add(1, Ordering::SeqCst);
        match step {
            // Round 0: call spy_tool once.
            0 => {
                let mut output = ModelOutput::new(
                    ModelResponse {
                        items: vec![ModelItem::tool_call(ToolCallDraft::new(
                            "spy_tool",
                            serde_json::json!({}),
                        ))],
                    },
                    ModelStopReason::ToolUse,
                );
                output.usage = Some(ModelUsage::new(50, 10));
                Ok(delta_stream(vec![StreamDelta::Done {
                    final_output: output,
                }]))
            }
            // Round 1, Attempt 1: yields reasoning + partial text then fails mid-stream.
            1 => Ok(delta_stream(vec![
                StreamDelta::ReasoningDelta {
                    delta: "aborted thought".into(),
                },
                StreamDelta::TextDelta {
                    delta: "aborted partial".into(),
                },
                StreamDelta::Error {
                    error: err_with(ModelInvokeErrorKind::Transient, "upstream mid-stream drop"),
                },
            ])),
            // Round 1, Attempt 2: recovers cleanly and finishes with Done.
            2 => Ok(delta_stream(vec![
                StreamDelta::ReasoningDelta {
                    delta: "clean thought".into(),
                },
                StreamDelta::TextDelta {
                    delta: "final clean answer".into(),
                },
                StreamDelta::Done {
                    final_output: ok_output("final clean answer", 80, 20),
                },
            ])),
            other => panic!("unexpected gateway stream call #{other}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn end_to_end_session_mid_stream_retry_resets_preview_commits_final_done_and_never_reruns_tools()
 {
    let env_name = format!("TARS_TEST_KEY_{}", uuid::Uuid::now_v7().simple());
    let definition = SessionDefinition {
        provider: Provider::Anthropic,
        model: "test-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment(env_name),
        work_timeout_secs: None,
        retained_work_capacity: 4,
        max_model_rounds: 4,
        max_tool_calls: 4,
        max_tokens: Some(1024),
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: Some(2),
        render_options: Default::default(),
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: false,
        compact_threshold: 70,
        prompt_cache: false,
        prelude: Default::default(),
        code_execution: None,
    };

    let tool_executions = Arc::new(AtomicUsize::new(0));
    let spy_tool: Arc<dyn Tool> = Arc::new(SpyTool {
        executions: tool_executions.clone(),
    });
    let gateway: Arc<dyn ModelGateway> = Arc::new(TwoRoundMidStreamGateway {
        round: AtomicUsize::new(0),
    });
    let resources = SessionResources::new(crate::tools::ToolResources::new(4))
        .with_test_gateway(gateway)
        .with_extra_tools(vec![spy_tool]);

    let mut assembled = assemble(
        &definition,
        "conv-retry-e2e".into(),
        Context::default(),
        resources,
        crate::permission::PermissionMode::FullAccess,
        None,
        true,
    )
    .unwrap();

    let handle = assembled.session.handle();
    let mut progress_rx = handle.subscribe_progress();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "req-1".into(),
            blocks: vec![ContextBlock::new(
                causa::runtime::new_block_id(),
                BlockContent::text(Role::User, "run tool then answer"),
                BlockMeta::default(),
            )],
        })
        .unwrap();

    let observed = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = observed.state else {
        panic!("work must finish")
    };
    assert!(matches!(
        &*result,
        WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));

    // Tool ran strictly once in Round 0; Round 1 retry never re-executed it.
    assert_eq!(tool_executions.load(Ordering::SeqCst), 1);

    // UsageHub counted Round 0 (1 ok) + Round 1 attempt 1 (1 fail) + Round 1 attempt 2 (1 ok).
    let usage = assembled.session.session_usage();
    assert_eq!(usage.conversation.calls.succeeded, 2);
    assert_eq!(usage.conversation.calls.failed, 1);
    assert_eq!(usage.conversation.calls.total(), 3);
    assert_eq!(usage.conversation.tokens.input_tokens, 130);
    assert_eq!(usage.conversation.tokens.output_tokens, 30);

    // Context committed ONLY the authoritative Round 1 Attempt 2 output.
    assembled
        .session
        .with_context(|ctx| {
            let texts: Vec<&str> = ctx
                .blocks()
                .iter()
                .filter_map(|block| match block.content() {
                    BlockContent::Message {
                        role: Role::Assistant,
                        parts,
                    } => parts.iter().find_map(|part| match part {
                        ContentPart::Text(payload) => Some(payload.0.as_str()),
                        _ => None,
                    }),
                    _ => None,
                })
                .collect();
            assert_eq!(texts, vec!["final clean answer"]);
        })
        .unwrap();

    // Collect all progress events and verify Round 1 emitted ModelStarted AGAIN before Attempt 2!
    let mut events: Vec<Arc<WorkProgress>> = Vec::new();
    while let Ok(ev) = progress_rx.try_recv() {
        events.push(ev);
    }
    let round1_events: Vec<&ProgressUpdate> = events
        .iter()
        .filter(|e| e.invocation_id.round_id == RoundId(1))
        .map(|e| &e.update)
        .collect();
    assert_eq!(
        round1_events,
        vec![
            &ProgressUpdate::ModelStarted,
            &ProgressUpdate::Reasoning {
                delta: "aborted thought".into(),
                truncated: false,
            },
            &ProgressUpdate::Text {
                delta: "aborted partial".into(),
                truncated: false,
            },
            &ProgressUpdate::ModelStarted,
            &ProgressUpdate::Reasoning {
                delta: "clean thought".into(),
                truncated: false,
            },
            &ProgressUpdate::Text {
                delta: "final clean answer".into(),
                truncated: false,
            },
            &ProgressUpdate::ModelOutputReady,
        ]
    );

    assembled.session.shutdown().await;
}
