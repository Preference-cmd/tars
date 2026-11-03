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
