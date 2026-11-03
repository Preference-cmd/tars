use super::types::{CallUsage, UsagePurpose};
use super::{CountingGateway, UsageHub};
use causa::kernel::{
    CallControl, CancellationToken, ContextFrame, InvocationId, ModelGateway, ModelInvokeError,
    ModelInvokeErrorKind, ModelItem, ModelOutput, ModelRef, ModelRequest, ModelResponse,
    ModelStopReason, ModelStream, ModelUsage, RoundId, StreamDelta, TurnId,
};
use futures_util::StreamExt;
use std::sync::Arc;

fn test_request() -> ModelRequest {
    ModelRequest::new(
        InvocationId {
            turn_id: TurnId::new("test-turn"),
            round_id: RoundId(0),
        },
        ContextFrame { blocks: vec![] },
        ModelRef::new("test-model"),
    )
}

fn test_control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

struct MockGateway {
    fail_invoke: bool,
    fail_stream: bool,
    usage: Option<ModelUsage>,
}

#[async_trait::async_trait]
impl ModelGateway for MockGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        if self.fail_invoke {
            return Err(ModelInvokeError::new(
                ModelInvokeErrorKind::Transient,
                "injected invoke failure",
            ));
        }
        let mut output = ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("ok")],
            },
            ModelStopReason::EndTurn,
        );
        output.usage = self.usage.clone();
        Ok(output)
    }

    async fn stream(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        if self.fail_stream {
            return Err(ModelInvokeError::new(
                ModelInvokeErrorKind::Transient,
                "injected stream failure",
            ));
        }
        let mut output = ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("ok")],
            },
            ModelStopReason::EndTurn,
        );
        output.usage = self.usage.clone();
        let items = vec![
            StreamDelta::TextDelta { delta: "ok".into() },
            StreamDelta::Done {
                final_output: output,
            },
        ];
        Ok(Box::pin(futures_util::stream::iter(items)))
    }
}

#[test]
fn test_call_usage_calculations() {
    let mut usage = ModelUsage::new(100, 20);
    usage.cache_read_tokens = Some(11);
    usage.cache_write_tokens = Some(3);
    usage.reasoning_tokens = Some(5);

    let call = CallUsage::from_model_usage(&usage);
    assert_eq!(call.input_tokens, 100);
    assert_eq!(call.cache_read_tokens, Some(11));
    assert_eq!(call.cache_write_tokens, Some(3));
    assert_eq!(call.output_tokens, 20);
    assert_eq!(call.reasoning_tokens, Some(5));

    assert_eq!(call.prompt_total(), 114);
    assert_eq!(call.call_total(), 134);
    let hit_rate = call.cache_hit_rate().unwrap();
    assert!((hit_rate - (11.0 / 114.0)).abs() < 1e-6);

    // Unreported read
    usage.cache_read_tokens = None;
    let call_no_read = CallUsage::from_model_usage(&usage);
    assert_eq!(call_no_read.prompt_total(), 103);
    assert_eq!(call_no_read.call_total(), 123);
    assert!(call_no_read.cache_hit_rate().is_none());
}

#[tokio::test]
async fn test_counting_gateway_invoke_success_and_failure() {
    let hub = Arc::new(UsageHub::new());
    let mut model_usage = ModelUsage::new(50, 10);
    model_usage.cache_read_tokens = Some(5);

    let ok_gateway = Arc::new(MockGateway {
        fail_invoke: false,
        fail_stream: false,
        usage: Some(model_usage),
    });
    let counted_ok = CountingGateway::new(ok_gateway, UsagePurpose::Conversation, hub.clone());
    let control = test_control();
    let request = test_request();

    let res = counted_ok.invoke(&request, &control).await;
    assert!(res.is_ok());

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.failed, 0);
    assert_eq!(snapshot.conversation.tokens.input_tokens, 50);
    assert_eq!(snapshot.conversation.tokens.output_tokens, 10);
    assert_eq!(snapshot.conversation.tokens.cache_read_tokens, 5);
    assert_eq!(snapshot.conversation.tokens.cache_read_unreported, 0);

    // Invoke failure
    let fail_gateway = Arc::new(MockGateway {
        fail_invoke: true,
        fail_stream: false,
        usage: None,
    });
    let counted_fail = CountingGateway::new(fail_gateway, UsagePurpose::Conversation, hub.clone());
    let res = counted_fail.invoke(&request, &control).await;
    assert!(res.is_err());

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.failed, 1);
    // Failed call adds no tokens
    assert_eq!(snapshot.conversation.tokens.input_tokens, 50);
}

#[tokio::test]
async fn streaming_counts_once_on_done_and_once_on_drop_or_error() {
    let hub = Arc::new(UsageHub::new());
    let control = test_control();
    let request = test_request();

    // 1. Stream completes with StreamDelta::Done
    let mut usage = ModelUsage::new(30, 15);
    usage.cache_read_tokens = Some(6);
    let gateway = Arc::new(MockGateway {
        fail_invoke: false,
        fail_stream: false,
        usage: Some(usage),
    });
    let counted = CountingGateway::new(gateway, UsagePurpose::Conversation, hub.clone());
    let mut stream = counted.stream(&request, &control).await.unwrap();
    while let Some(delta) = stream.next().await {
        if matches!(delta, StreamDelta::Done { .. }) {
            break;
        }
    }
    // Drop the completed stream
    drop(stream);

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.failed, 0);
    assert_eq!(snapshot.conversation.tokens.input_tokens, 30);
    assert_eq!(snapshot.conversation.tokens.output_tokens, 15);
    assert_eq!(snapshot.conversation.tokens.cache_read_tokens, 6);

    // 2. Stream dropped before StreamDelta::Done
    struct UnfinishedGateway;
    #[async_trait::async_trait]
    impl ModelGateway for UnfinishedGateway {
        async fn invoke(
            &self,
            _: &ModelRequest,
            _: &CallControl,
        ) -> Result<ModelOutput, ModelInvokeError> {
            unimplemented!()
        }
        async fn stream(
            &self,
            _: &ModelRequest,
            _: &CallControl,
        ) -> Result<ModelStream, ModelInvokeError> {
            let items = vec![StreamDelta::TextDelta {
                delta: "partial".into(),
            }];
            Ok(Box::pin(futures_util::stream::iter(items)))
        }
    }

    let counted_unfinished = CountingGateway::new(
        Arc::new(UnfinishedGateway),
        UsagePurpose::Conversation,
        hub.clone(),
    );
    let mut stream2 = counted_unfinished.stream(&request, &control).await.unwrap();
    let delta = stream2.next().await.unwrap();
    assert!(matches!(delta, StreamDelta::TextDelta { .. }));
    // Drop before Done!
    drop(stream2);

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.failed, 1);
    // Tokens unchanged by failure
    assert_eq!(snapshot.conversation.tokens.input_tokens, 30);

    // 3. Stream call returns error immediately
    let fail_stream_gw = Arc::new(MockGateway {
        fail_invoke: false,
        fail_stream: true,
        usage: None,
    });
    let counted_err = CountingGateway::new(fail_stream_gw, UsagePurpose::Conversation, hub.clone());
    assert!(counted_err.stream(&request, &control).await.is_err());

    let snapshot = hub.snapshot();
    assert_eq!(snapshot.conversation.calls.succeeded, 1);
    assert_eq!(snapshot.conversation.calls.failed, 2);
}

#[test]
fn aggregate_hit_rate_ignores_calls_that_did_not_report_cache_reads() {
    let mut reported = ModelUsage::new(100, 20);
    reported.cache_read_tokens = Some(11);
    reported.cache_write_tokens = Some(3);
    let unreported = ModelUsage::new(1_000, 20);

    let mut totals = crate::usage::UsageTotals::default();
    totals.record_call(&crate::usage::CallUsage::from_model_usage(&reported));
    totals.record_call(&crate::usage::CallUsage::from_model_usage(&unreported));
    // Only the reporting call's 114 prompt tokens are the denominator.
    let rate = totals.cache_hit_rate().unwrap();
    assert!((rate - 11.0 / 114.0).abs() < 1e-9);

    // A reported zero stays a 0% rate even next to an unreported call.
    let mut miss = ModelUsage::new(100, 20);
    miss.cache_read_tokens = Some(0);
    let mut totals = crate::usage::UsageTotals::default();
    totals.record_call(&crate::usage::CallUsage::from_model_usage(&miss));
    totals.record_call(&crate::usage::CallUsage::from_model_usage(&unreported));
    assert_eq!(totals.cache_hit_rate(), Some(0.0));

    // No call reported cache reads: the rate is unknown, not 0%.
    let mut totals = crate::usage::UsageTotals::default();
    totals.record_call(&crate::usage::CallUsage::from_model_usage(&unreported));
    assert_eq!(totals.cache_hit_rate(), None);
}

#[test]
fn test_work_usage_records_last_call() {
    use std::sync::Mutex;
    let hub = UsageHub::new();
    let work = Arc::new(Mutex::new(crate::usage::WorkUsage::default()));
    hub.set_active_work(Some(work.clone()));

    let mut model_usage = ModelUsage::new(100, 20);
    model_usage.cache_read_tokens = Some(11);
    hub.record_call_success(UsagePurpose::Conversation, Some(&model_usage));

    let work_snap = *work.lock().unwrap();
    assert_eq!(work_snap.model_rounds, 1);
    assert_eq!(work_snap.conversation.calls.succeeded, 1);
    let last = work_snap.last_call.expect("work last_call should be set");
    assert_eq!(last.purpose, UsagePurpose::Conversation);
    let call = last.usage.expect("call usage present");
    assert_eq!(call.input_tokens, 100);
    assert_eq!(call.output_tokens, 20);
    assert_eq!(call.cache_read_tokens, Some(11));

    // Failure does not overwrite last_call
    hub.record_call_failure(UsagePurpose::Conversation);
    let work_snap = *work.lock().unwrap();
    assert_eq!(work_snap.conversation.calls.failed, 1);
    assert!(work_snap.last_call.is_some());
}

#[test]
fn test_session_usage_lines_us01_display() {
    use crate::usage::{
        CallCounts, CallUsage, LastCall, PurposeUsage, SessionUsage, UsageTotals,
        session_usage_lines,
    };
    use std::time::SystemTime;

    // Case 1: fixture usage input 100, read Some(11), write Some(3), output 20
    let call = CallUsage {
        input_tokens: 100,
        cache_read_tokens: Some(11),
        cache_write_tokens: Some(3),
        output_tokens: 20,
        reasoning_tokens: None,
    };
    let mut totals = UsageTotals::default();
    totals.record_call(&call);

    let session = SessionUsage {
        conversation: PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 0,
            },
            tokens: totals,
        },
        compaction: PurposeUsage::default(),
        input_rounds: 1,
        last_call: Some(LastCall {
            purpose: UsagePurpose::Conversation,
            usage: Some(call),
            timestamp: SystemTime::UNIX_EPOCH,
        }),
    };

    let lines = session_usage_lines(&session);
    assert_eq!(
        lines[0],
        "Usage: 1 call(s) (1 ok, 0 failed, 0 without usage) · 1 input round(s)"
    );
    assert_eq!(
        lines[1],
        "  Total: input 100 · cache read 11 · cache write 3 · output 20 · reasoning not reported · prompt 114 · total 134 · cache hit 9.6%"
    );
    assert_eq!(
        lines[2],
        "  Conversation: input 100 · cache read 11 · cache write 3 · output 20 · reasoning not reported · prompt 114 · total 134 · cache hit 9.6%"
    );
    assert_eq!(lines[3], "  Compaction: no calls");
    assert_eq!(
        lines[4],
        "  Last call (conversation): input 100 · cache read 11 · cache write 3 · output 20 · reasoning not reported · prompt 114 · total 134 · cache hit 9.6%"
    );

    // Case 2: read/write None -> "not reported" (not 0)
    let call_no_cache = CallUsage {
        input_tokens: 100,
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: 20,
        reasoning_tokens: None,
    };
    let mut totals_no_cache = UsageTotals::default();
    totals_no_cache.record_call(&call_no_cache);
    let session_no_cache = SessionUsage {
        conversation: PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 0,
            },
            tokens: totals_no_cache,
        },
        compaction: PurposeUsage::default(),
        input_rounds: 1,
        last_call: Some(LastCall {
            purpose: UsagePurpose::Conversation,
            usage: Some(call_no_cache),
            timestamp: SystemTime::UNIX_EPOCH,
        }),
    };
    let lines = session_usage_lines(&session_no_cache);
    assert_eq!(
        lines[1],
        "  Total: input 100 · cache read not reported · cache write not reported · output 20 · reasoning not reported · prompt 100 · total 120 · cache hit not reported"
    );

    // Case 3: call with no usage at all -> usage_unreported 1 and token fields null / "not reported"
    let session_unreported = SessionUsage {
        conversation: PurposeUsage {
            calls: CallCounts {
                succeeded: 1,
                failed: 0,
                unreported: 1,
            },
            tokens: UsageTotals::default(),
        },
        compaction: PurposeUsage::default(),
        input_rounds: 1,
        last_call: Some(LastCall {
            purpose: UsagePurpose::Conversation,
            usage: None,
            timestamp: SystemTime::UNIX_EPOCH,
        }),
    };
    let lines = session_usage_lines(&session_unreported);
    assert_eq!(
        lines[0],
        "Usage: 1 call(s) (1 ok, 0 failed, 1 without usage) · 1 input round(s)"
    );
    assert_eq!(
        lines[1],
        "  Total: input not reported · cache read not reported · cache write not reported · output not reported · reasoning not reported · prompt not reported · total not reported · cache hit not reported"
    );
    assert_eq!(
        lines[4],
        "  Last call (conversation): input not reported · cache read not reported · cache write not reported · output not reported · reasoning not reported · prompt not reported · total not reported · cache hit not reported"
    );
}

#[test]
fn figures_keep_a_count_reported_by_some_calls_and_blank_one_reported_by_none() {
    use crate::usage::{CallCounts, CallUsage, PurposeUsage, UsageFigures, UsageTotals};

    let mut tokens = UsageTotals::default();
    tokens.record_call(&CallUsage {
        input_tokens: 50,
        cache_read_tokens: Some(5),
        cache_write_tokens: None,
        output_tokens: 10,
        reasoning_tokens: None,
    });
    tokens.record_call(&CallUsage {
        input_tokens: 60,
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: 15,
        reasoning_tokens: Some(4),
    });
    // A third successful call reported no usage at all.
    let usage = PurposeUsage {
        calls: CallCounts {
            succeeded: 3,
            failed: 1,
            unreported: 1,
        },
        tokens,
    };
    let figures = usage.figures();
    assert_eq!(figures.input_tokens, Some(110));
    assert_eq!(figures.cache_read_tokens, Some(5));
    assert_eq!(figures.cache_write_tokens, None);
    assert_eq!(figures.output_tokens, Some(25));
    assert_eq!(figures.reasoning_tokens, Some(4));
    assert_eq!(figures.prompt_tokens, Some(115));
    assert_eq!(figures.total_tokens, Some(140));
    // Only the first call reported cache reads: 5 of its 55 prompt tokens.
    assert_eq!(figures.cache_hit_rate, Some(5.0 / 55.0));

    let only_unreported = PurposeUsage {
        calls: CallCounts {
            succeeded: 1,
            failed: 2,
            unreported: 1,
        },
        tokens: UsageTotals::default(),
    };
    assert_eq!(only_unreported.figures(), UsageFigures::default());
}
