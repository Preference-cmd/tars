use super::support::*;
use crate::application::ClosePolicy;
use crate::definition::WorkspaceTool;
use crate::permission::PermissionMode;
use crate::usage::CallUsage;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelInvokeErrorKind, ModelItem,
    ModelOutput, ModelRequest, ModelResponse, ModelStopReason, ModelStream, ModelUsage,
    StreamDelta, ToolCallDraft,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// US01: fixture usage input 100, read Some(11), write Some(3), output 20 → prompt total 114,
/// call total 134, hit rate ≈ 0.0965 (11/114); read None → hit rate None and the session sums
/// record one "read unreported" (not a 0 that looks reported).
#[test]
fn us01_per_call_figures_and_unreported_cache_reads() {
    let mut usage = ModelUsage::new(100, 20);
    usage.cache_read_tokens = Some(11);
    usage.cache_write_tokens = Some(3);
    // Reasoning is part of output and never adds to a total.
    usage.reasoning_tokens = Some(5);

    let call = CallUsage::from_model_usage(&usage);
    assert_eq!(call.prompt_total(), 114);
    assert_eq!(call.call_total(), 134);
    let hit_rate = call.cache_hit_rate().expect("hit rate present");
    assert!((hit_rate - (11.0 / 114.0)).abs() < 1e-4);

    // read None -> hit rate None
    usage.cache_read_tokens = None;
    let call_none = CallUsage::from_model_usage(&usage);
    assert_eq!(call_none.prompt_total(), 103);
    assert_eq!(call_none.call_total(), 123);
    assert!(call_none.cache_hit_rate().is_none());

    // Verify session sums record one "read unreported"
    let mut totals = crate::usage::UsageTotals::default();
    totals.record_call(&call);
    totals.record_call(&call_none);

    assert_eq!(totals.input_tokens, 200);
    assert_eq!(totals.output_tokens, 40);
    assert_eq!(totals.cache_read_tokens, 11);
    assert_eq!(totals.cache_read_unreported, 1);
    assert_eq!(totals.cache_write_tokens, 6);
    assert_eq!(totals.cache_write_unreported, 0);
}

/// Fixture gateway for retry testing that fails the first attempt with a transient error.
struct TransientRetryGateway {
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelGateway for TransientRetryGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            Err(ModelInvokeError::new(
                ModelInvokeErrorKind::Transient,
                "transient connection reset",
            ))
        } else {
            let mut output = ModelOutput::new(
                ModelResponse {
                    items: vec![ModelItem::text("retry succeeded")],
                },
                ModelStopReason::EndTurn,
            );
            let mut usage = ModelUsage::new(100, 20);
            usage.cache_read_tokens = Some(10);
            output.usage = Some(usage);
            Ok(output)
        }
    }
}

/// US03: max_model_attempts = 2, the fixture protocol gateway fails the first attempt
/// with a retryable/transient error and succeeds on the second → conversation calls = 2
/// (1 failed, 1 succeeded), token sums added once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us03_retrying_gateway_counts_each_attempt() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    let fixture = Arc::new(TransientRetryGateway {
        attempts: AtomicUsize::new(0),
    });
    host.set_test_gateway(Some(fixture.clone()));

    let mut def = definition();
    def.max_model_attempts = Some(2);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let receipt = session.submit(request("retry-test")).unwrap();
    finished(&session, &receipt.work).await;

    assert_eq!(fixture.attempts.load(Ordering::SeqCst), 2);

    let session_usage = session.session_usage().expect("session usage present");
    assert_eq!(session_usage.conversation.calls.succeeded, 1);
    assert_eq!(session_usage.conversation.calls.failed, 1);
    assert_eq!(session_usage.conversation.calls.total(), 2);
    assert_eq!(session_usage.conversation.tokens.input_tokens, 100);
    assert_eq!(session_usage.conversation.tokens.output_tokens, 20);
    assert_eq!(session_usage.conversation.tokens.cache_read_tokens, 10);

    let work_usage = session
        .work_usage(&receipt.work)
        .expect("work usage present");
    assert_eq!(work_usage.conversation.calls.succeeded, 1);
    assert_eq!(work_usage.conversation.calls.failed, 1);
    assert_eq!(work_usage.model_rounds, 1);
    assert_eq!(work_usage.conversation.tokens.input_tokens, 100);
    assert_eq!(work_usage.conversation.tokens.output_tokens, 20);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

/// US04: after one compaction (manual or automatic), compaction calls = 1 and conversation
/// counts unchanged by it; both purposes' token sums are separate and the combined total includes both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us04_compaction_calls_and_conversation_counts_remain_separate() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    let gateway = CompactionGateway::new(SummaryBehavior::Text(
        "Goal: test compaction usage separation.".into(),
    ));
    host.set_test_gateway(Some(gateway.clone()));

    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();

    // Submit work that triggers automatic compaction
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let usage = session.session_usage().expect("session usage present");
    assert_eq!(usage.compaction.calls.succeeded, 1);
    assert_eq!(usage.compaction.calls.failed, 0);
    assert_eq!(usage.conversation.calls.succeeded, 1);
    assert_eq!(usage.conversation.calls.failed, 0);

    // Tokens are counted separately per purpose
    assert_eq!(usage.compaction.tokens.input_tokens, 400); // from summary_output()
    assert_eq!(usage.compaction.tokens.output_tokens, 30);
    assert_eq!(usage.conversation.tokens.input_tokens, 120); // from CompactionGateway main_call
    assert_eq!(usage.conversation.tokens.output_tokens, 20);

    // Combined total includes both
    assert_eq!(usage.combined().calls.succeeded, 2);
    assert_eq!(usage.combined().tokens.input_tokens, 520);
    assert_eq!(usage.combined().tokens.output_tokens, 50);

    // Also verify work_usage for this work
    let work_usage = session
        .work_usage(&receipt.work)
        .expect("work usage present");
    assert_eq!(work_usage.compaction.calls.succeeded, 1);
    assert_eq!(work_usage.conversation.calls.succeeded, 1);
    assert_eq!(work_usage.combined().calls.succeeded, 2);
    assert_eq!(work_usage.model_rounds, 1);

    // Now trigger manual compaction when idle
    assert!(session.compact().await.is_ok());

    let usage_after_manual = session.session_usage().expect("session usage present");
    assert_eq!(usage_after_manual.compaction.calls.succeeded, 2);
    // Conversation counts completely unchanged by manual compaction
    assert_eq!(usage_after_manual.conversation.calls.succeeded, 1);
    assert_eq!(usage_after_manual.conversation.tokens.input_tokens, 120);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

/// US05: submit 3 works, cancel 1 before it enters the runner → input rounds = 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us05_input_rounds_count_only_works_entering_runner() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    normal_fixture(&host);

    let session = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();

    // Work 1: runs to completion
    let r1 = session.submit(request("w1")).unwrap();
    finished(&session, &r1.work).await;
    session.wait_saved().await.unwrap();
    let obs1 = session.observe(&r1.work).unwrap();
    assert!(
        matches!(obs1.state, WorkState::Finished(ref res) if matches!(**res, WorkResult::Run(_)))
    );

    // Work 2: cancel before entering the runner
    let r2 = session.submit(request("w2")).unwrap();
    session.cancel(&r2.work).unwrap();
    finished(&session, &r2.work).await;
    session.wait_saved().await.unwrap();
    let obs2 = session.observe(&r2.work).unwrap();
    assert!(matches!(
        obs2.state,
        WorkState::Finished(ref res) if matches!(**res, WorkResult::StoppedBeforeRun(causa::kernel::ControlError::Cancelled))
    ));

    // Work 3: runs to completion
    let r3 = session.submit(request("w3")).unwrap();
    finished(&session, &r3.work).await;
    session.wait_saved().await.unwrap();
    let obs3 = session.observe(&r3.work).unwrap();
    assert!(
        matches!(obs3.state, WorkState::Finished(ref res) if matches!(**res, WorkResult::Run(_)))
    );

    let usage = session.session_usage().expect("session usage present");
    assert_eq!(usage.input_rounds, 2);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

/// Fixture gateway for tool-loop test.
struct ThreeRoundToolGateway {
    round: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelGateway for ThreeRoundToolGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let round = self.round.fetch_add(1, Ordering::SeqCst);
        let (output, usage) = match round {
            0 => {
                let out = ModelOutput::new(
                    ModelResponse {
                        items: vec![ModelItem::tool_call(ToolCallDraft::new(
                            "list_directory",
                            serde_json::json!({"path": "."}),
                        ))],
                    },
                    ModelStopReason::ToolUse,
                );
                (out, ModelUsage::new(100, 10))
            }
            1 => {
                let out = ModelOutput::new(
                    ModelResponse {
                        items: vec![ModelItem::tool_call(ToolCallDraft::new(
                            "list_directory",
                            serde_json::json!({"path": "."}),
                        ))],
                    },
                    ModelStopReason::ToolUse,
                );
                (out, ModelUsage::new(150, 15))
            }
            _ => {
                let out = ModelOutput::new(
                    ModelResponse {
                        items: vec![ModelItem::text("final answer")],
                    },
                    ModelStopReason::EndTurn,
                );
                (out, ModelUsage::new(200, 20))
            }
        };
        let mut out = output;
        out.usage = Some(usage);
        Ok(out)
    }
}

/// US06: a 3-round tool loop (tool call, tool call, final text) → that work's
/// model rounds = 3 and calls = 3, token sums equal the sum of the three responses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us06_multi_round_tool_loop_accumulates_model_rounds_and_tokens() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(None);

    let gateway = Arc::new(ThreeRoundToolGateway {
        round: AtomicUsize::new(0),
    });
    host.set_test_gateway(Some(gateway));

    let mut def = definition();
    def.workspace_root = Some(workspace_dir.path().canonicalize().unwrap());
    def.tools = vec![WorkspaceTool::ListDirectory];
    def.permission_mode = Some(PermissionMode::FullAccess);
    def.max_model_rounds = 4;
    def.max_tool_calls = 10;

    let session = host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();

    let receipt = session.submit(request("tool-loop")).unwrap();
    finished(&session, &receipt.work).await;

    let work_usage = session
        .work_usage(&receipt.work)
        .expect("work usage present");
    assert_eq!(work_usage.model_rounds, 3);
    assert_eq!(work_usage.conversation.calls.succeeded, 3);
    assert_eq!(work_usage.conversation.calls.total(), 3);
    assert_eq!(work_usage.conversation.tokens.input_tokens, 100 + 150 + 200);
    assert_eq!(work_usage.conversation.tokens.output_tokens, 10 + 15 + 20);

    let session_usage = session.session_usage().expect("session usage present");
    assert_eq!(session_usage.conversation.calls.succeeded, 3);
    assert_eq!(session_usage.conversation.tokens.input_tokens, 450);
    assert_eq!(session_usage.conversation.tokens.output_tokens, 45);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

/// Fixture gateway returning output without usage.
struct NoUsageGateway;

#[async_trait::async_trait]
impl ModelGateway for NoUsageGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let mut output = ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("no usage reported")],
            },
            ModelStopReason::EndTurn,
        );
        output.usage = None;
        Ok(output)
    }
}

/// US07: a successful response with usage = None → unreported count +1, token sums unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us07_successful_response_with_unreported_usage() {
    let host = SessionHost::new(None);
    host.set_test_gateway(Some(Arc::new(NoUsageGateway)));

    let def = definition();

    let session = host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();

    let receipt = session.submit(request("no-usage-test")).unwrap();
    finished(&session, &receipt.work).await;

    let session_usage = session.session_usage().expect("session usage present");
    assert_eq!(session_usage.conversation.calls.succeeded, 1);
    assert_eq!(session_usage.conversation.calls.unreported, 1);
    assert_eq!(session_usage.conversation.tokens.input_tokens, 0);
    assert_eq!(session_usage.conversation.tokens.output_tokens, 0);

    let work_usage = session
        .work_usage(&receipt.work)
        .expect("work usage present");
    assert_eq!(work_usage.conversation.calls.succeeded, 1);
    assert_eq!(work_usage.conversation.calls.unreported, 1);
    assert_eq!(work_usage.conversation.tokens.input_tokens, 0);
    assert_eq!(work_usage.conversation.tokens.output_tokens, 0);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

/// Work usage is released together with the retained record when capacity is reached,
/// and returns None after session close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_usage_released_when_retained_record_released_at_capacity() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    normal_fixture(&host);

    let mut def = definition();
    def.retained_work_capacity = 2;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let r1 = session.submit(request("w1")).unwrap();
    finished(&session, &r1.work).await;
    session.wait_saved().await.unwrap();
    assert!(session.work_usage(&r1.work).is_some());

    let r2 = session.submit(request("w2")).unwrap();
    finished(&session, &r2.work).await;
    session.wait_saved().await.unwrap();
    assert!(session.work_usage(&r2.work).is_some());

    // Work 3 forces work 1 to be released (capacity = 2)
    let r3 = session.submit(request("w3")).unwrap();
    finished(&session, &r3.work).await;
    session.wait_saved().await.unwrap();

    assert!(
        session.work_usage(&r1.work).is_none(),
        "work 1 usage must be released when record is released"
    );
    assert!(session.work_usage(&r2.work).is_some());
    assert!(session.work_usage(&r3.work).is_some());

    // Close session: session_usage and work_usage return None
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    assert!(session.session_usage().is_none());
    assert!(session.work_usage(&r3.work).is_none());
}

/// Streams like a provider: the first attempt fails inside the stream before
/// any output, later attempts stream text and finish with usage.
struct StreamFailsFirst {
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelGateway for StreamFailsFirst {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        unreachable!("the runner streams")
    }

    async fn stream(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            let error = ModelInvokeError::new(ModelInvokeErrorKind::Transient, "stream reset");
            return Ok(Box::pin(futures_util::stream::iter(vec![
                StreamDelta::Error { error },
            ])));
        }
        let mut output = ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("streamed")],
            },
            ModelStopReason::EndTurn,
        );
        let mut usage = ModelUsage::new(70, 7);
        usage.cache_read_tokens = Some(30);
        output.usage = Some(usage);
        Ok(causa::kernel::completed_model_stream(output))
    }
}

async fn streamed_session_usage(max_model_attempts: Option<u32>) -> crate::usage::SessionUsage {
    let host = SessionHost::new(None);
    host.set_test_gateway(Some(Arc::new(StreamFailsFirst {
        attempts: AtomicUsize::new(0),
    })));
    let mut def = definition();
    def.max_model_attempts = max_model_attempts;
    let session = host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();
    let receipt = session.submit(request("stream")).unwrap();
    finished(&session, &receipt.work).await;
    session.session_usage().expect("session usage present")
}

/// A stream that fails inside counts one failed call and no tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_error_through_the_session_counts_one_failed_call() {
    let usage = streamed_session_usage(None).await;
    assert_eq!(usage.conversation.calls.failed, 1);
    assert_eq!(usage.conversation.calls.succeeded, 0);
    assert_eq!(
        usage.conversation.tokens,
        crate::usage::UsageTotals::default()
    );
    assert_eq!(usage.input_rounds, 1);
}

/// A retried stream counts both attempts and the tokens of the one that finished.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retried_stream_through_the_session_counts_each_attempt() {
    let usage = streamed_session_usage(Some(2)).await;
    assert_eq!(usage.conversation.calls.failed, 1);
    assert_eq!(usage.conversation.calls.succeeded, 1);
    assert_eq!(usage.conversation.tokens.input_tokens, 70);
    assert_eq!(usage.conversation.tokens.cache_read_tokens, 30);
    assert_eq!(usage.conversation.tokens.output_tokens, 7);
    assert_eq!(usage.conversation.tokens.cache_hit_rate(), Some(0.3));
}

/// Gateway returning distinct usage per call to verify save/reopen accumulation.
struct VariedUsageGateway {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelGateway for VariedUsageGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut output = ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(format!("reply {n}"))],
            },
            ModelStopReason::EndTurn,
        );
        if n == 0 {
            let mut usage = ModelUsage::new(100, 20);
            usage.cache_read_tokens = Some(15);
            usage.cache_write_tokens = None;
            usage.reasoning_tokens = Some(8);
            output.usage = Some(usage);
        } else {
            let mut usage = ModelUsage::new(50, 10);
            usage.cache_read_tokens = None;
            usage.cache_write_tokens = Some(5);
            usage.reasoning_tokens = None;
            output.usage = Some(usage);
        }
        Ok(output)
    }

    async fn stream(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelStream, ModelInvokeError> {
        let output = self.invoke(request, control).await?;
        Ok(causa::kernel::completed_model_stream(output))
    }
}

/// US08: save, close, reopen a persistent session with fixture usage -> session_usage() equals
/// the pre-close snapshot; running one more work -> totals grow from the saved values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us08_save_close_reopen_persistent_session_restores_usage_and_continues_counting() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    let gateway = Arc::new(VariedUsageGateway {
        calls: AtomicUsize::new(0),
    });
    host.set_test_gateway(Some(gateway));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();

    let id = session.target().conversation_id.as_str().to_owned();

    let receipt1 = session.submit(request("first-work")).unwrap();
    finished(&session, &receipt1.work).await;
    session.wait_saved().await.unwrap();

    let pre_close = session.session_usage().expect("session usage present");
    assert_eq!(pre_close.input_rounds, 1);
    assert_eq!(pre_close.conversation.calls.succeeded, 1);
    assert_eq!(pre_close.conversation.tokens.input_tokens, 100);
    assert_eq!(pre_close.conversation.tokens.cache_read_tokens, 15);
    assert_eq!(pre_close.conversation.tokens.cache_write_unreported, 1);
    assert_eq!(pre_close.conversation.tokens.reasoning_tokens, 8);
    assert!(pre_close.last_call.is_some());
    let last = pre_close.last_call.unwrap();
    assert_eq!(last.purpose, crate::usage::UsagePurpose::Conversation);
    assert_eq!(last.usage.unwrap().cache_write_tokens, None);
    assert_eq!(last.usage.unwrap().cache_read_tokens, Some(15));

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Reopen session
    let reopened = host.open(id.as_str().into()).await.unwrap();
    let restored_usage = reopened
        .session_usage()
        .expect("reopened session usage present");

    // Must equal pre-close snapshot
    assert_eq!(restored_usage, pre_close);

    // Run one more work
    let receipt2 = reopened.submit(request("second-work")).unwrap();
    finished(&reopened, &receipt2.work).await;
    reopened.wait_saved().await.unwrap();

    let second_usage = reopened
        .session_usage()
        .expect("second session usage present");
    assert_eq!(second_usage.input_rounds, 2);
    assert_eq!(second_usage.conversation.calls.succeeded, 2);
    assert_eq!(second_usage.conversation.tokens.input_tokens, 150); // 100 + 50
    assert_eq!(second_usage.conversation.tokens.output_tokens, 30); // 20 + 10
    assert_eq!(second_usage.conversation.tokens.cache_read_tokens, 15); // unchanged
    assert_eq!(second_usage.conversation.tokens.cache_read_unreported, 1); // 1 call unreported
    assert_eq!(second_usage.conversation.tokens.cache_write_tokens, 5); // 0 + 5
    assert_eq!(second_usage.conversation.tokens.cache_write_unreported, 1); // first call unreported

    let new_last = second_usage.last_call.unwrap();
    assert_eq!(new_last.usage.unwrap().input_tokens, 50);
    assert_eq!(new_last.usage.unwrap().cache_read_tokens, None);
    assert_eq!(new_last.usage.unwrap().cache_write_tokens, Some(5));

    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

/// US08: compaction followed by save and reopen keeps totals without reset.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us08_compaction_followed_by_save_and_reopen_keeps_totals_without_reset() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    let gateway = CompactionGateway::new(SummaryBehavior::Text(
        "Goal: test compaction persistence.".into(),
    ));
    host.set_test_gateway(Some(gateway.clone()));

    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();

    let id = session.target().conversation_id.as_str().to_owned();

    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let usage_pre_close = session.session_usage().expect("session usage present");
    assert_eq!(usage_pre_close.compaction.calls.succeeded, 1);
    assert_eq!(usage_pre_close.conversation.calls.succeeded, 1);
    assert_eq!(usage_pre_close.compaction.tokens.input_tokens, 400);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let reopened = host.open(id.as_str().into()).await.unwrap();
    let usage_reopened = reopened.session_usage().expect("reopened usage present");
    assert_eq!(usage_reopened, usage_pre_close);
    assert_eq!(usage_reopened.compaction.calls.succeeded, 1);
    assert_eq!(usage_reopened.conversation.calls.succeeded, 1);
    assert_eq!(usage_reopened.compaction.tokens.input_tokens, 400);

    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

/// US08: newly created session is v2.1, v2 fails with UnsupportedFormat, export-materials yields Context,
/// and imported materials start with zero usage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn us08_format_v2_1_rejection_of_v2_and_material_export() {
    let store_dir = tempfile::tempdir().unwrap();
    let store_path: std::path::PathBuf = store_dir.path().into();
    let host = SessionHost::new(Some(store_path.clone()));
    normal_fixture(&host);

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id.as_str().to_owned();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Verify record on disk contains "format":"tars-session-v2.2"
    let record_file = store_path.join("sessions").join(format!("{id}.json"));
    let content = std::fs::read_to_string(&record_file).unwrap();
    assert!(content.contains(r#""format":"tars-session-v2.2""#));

    // A v2 record fails to open with UnsupportedFormat
    let v2_id = uuid::Uuid::now_v7().to_string();
    let v2_commit = uuid::Uuid::now_v7().to_string();
    let v2_record_path = store_path.join("sessions").join(format!("{v2_id}.json"));
    let v2_json = format!(
        r#"{{"format":"tars-session-v2","conversation_id":"{v2_id}","revision":1,"last_commit_id":"{v2_commit}","definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&v2_record_path, v2_json.as_bytes()).unwrap();

    let open_err = host.open(v2_id.as_str().into()).await.err().unwrap();
    match open_err {
        ApplicationError::Store(ref err) => match &**err {
            crate::store::StoreError::UnsupportedFormat(fmt) => {
                assert_eq!(fmt, "tars-session-v2");
            }
            other => panic!("expected StoreError::UnsupportedFormat, got {other:?}"),
        },
        other => panic!("expected ApplicationError::Store, got {other:?}"),
    }

    // export-materials of a v2 and v2.1 record yields Context
    let v2_ctx = crate::material::load_session_materials(&v2_record_path).unwrap();
    assert!(v2_ctx.blocks().is_empty());

    let v2_1_ctx = crate::material::load_session_materials(&record_file).unwrap();
    assert!(v2_1_ctx.blocks().is_empty());

    // Session created from imported materials starts with zero usage
    let import_session = host
        .create(PreparedCreate::persistent(definition(), v2_ctx))
        .await
        .unwrap();
    assert_eq!(
        import_session.session_usage(),
        Some(crate::usage::SessionUsage::default())
    );
    host.close(&import_session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}
