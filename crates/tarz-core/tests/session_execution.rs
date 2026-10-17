mod common;
use async_trait::async_trait;
use causa::{kernel::*, runtime::*};
use common::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tarz_core::session::*;
use tokio::sync::Notify;

#[derive(Debug)]
struct AssemblyError(u32);
impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "assembly {}", self.0)
    }
}
impl std::error::Error for AssemblyError {}

#[tokio::test]
async fn assembly_failure_retains_original_error_input_and_receipt() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Err(Box::new(AssemblyError(42)) as Box<dyn std::error::Error + Send + Sync>) }
        },
        config(2),
    );
    let handle = session.handle();
    let input = vec![block("input")];
    let receipt = handle.submit(request("key", input.clone())).unwrap();
    let result = finished(&handle, &receipt.work).await;
    let WorkResult::AssemblyFailed(error) = &*result else {
        panic!("{result:?}")
    };
    assert_eq!(error.downcast_ref::<AssemblyError>().unwrap().0, 42);
    assert_eq!(context(&mut session), input);
    handle.cancel(&receipt.work).unwrap();
    let again = finished(&handle, &receipt.work).await;
    assert!(Arc::ptr_eq(&result, &again));
    assert_eq!(handle.submit(request("key", input)).unwrap(), receipt);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = handle.submit(request("next", vec![])).unwrap();
    finished(&handle, &next.work).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancellation_during_assembly_recovers_context_and_never_calls_model() {
    let entered = Arc::new(Notify::new());
    let notice = entered.clone();
    let gateway = Gateway::new(vec![]);
    let model = runner(gateway.clone(), vec![]);
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, control| {
            let entered = notice.clone();
            let model = model.clone();
            async move {
                entered.notify_one();
                control.cancellation_token().cancelled().await;
                Ok((model, TurnRunOptions::new(ModelRef::new("unused"))))
            }
        },
        config(1),
    );
    let handle = session.handle();
    let input = vec![block("kept")];
    let receipt = handle.submit(request("key", input.clone())).unwrap();
    tokio::time::timeout(BOUND, entered.notified())
        .await
        .unwrap();
    let snapshot = handle.wait(&receipt.work, Duration::ZERO).await.unwrap();
    assert!(!matches!(snapshot.state, WorkState::Finished(_)));
    let bounded = handle
        .wait(&receipt.work, Duration::from_millis(1))
        .await
        .unwrap();
    assert!(!matches!(bounded.state, WorkState::Finished(_)));
    handle.cancel(&receipt.work).unwrap();
    handle.cancel(&receipt.work).unwrap();
    let result = finished(&handle, &receipt.work).await;
    assert!(matches!(
        &*result,
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
    ));
    assert_eq!(context(&mut session), input);
    assert!(gateway.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn control_wins_when_assembly_cancels_and_returns_an_error() {
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        |_, control| async move {
            control.cancellation_token().cancel();
            Err(Box::new(AssemblyError(7)) as Box<dyn std::error::Error + Send + Sync>)
        },
        config(1),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![block("input")])).unwrap();
    assert!(matches!(
        &*finished(&handle, &receipt.work).await,
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
    ));
    assert_eq!(context(&mut session).len(), 1);
}

#[tokio::test]
async fn zero_deadline_is_accepted_and_stops_before_model() {
    let gateway = Gateway::new(vec![]);
    let model = runner(gateway.clone(), vec![]);
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            let model = model.clone();
            async move { Ok((model, TurnRunOptions::new(ModelRef::new("unused")))) }
        },
        SessionConfig {
            retained_work_capacity: 1,
            work_timeout: Some(Duration::ZERO),
        },
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![block("input")])).unwrap();
    assert!(matches!(
        &*finished(&handle, &receipt.work).await,
        WorkResult::StoppedBeforeRun(ControlError::TimedOut)
    ));
    assert_eq!(context(&mut session).len(), 1);
    assert!(gateway.requests.lock().unwrap().is_empty());
}

struct ParkedGateway {
    entered: Arc<Notify>,
    finished: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelGateway for ParkedGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.entered.notify_one();
        control.cancellation_token().cancelled().await;
        self.finished.fetch_add(1, Ordering::SeqCst);
        Err(ModelInvokeError::new(
            ModelInvokeErrorKind::Cancelled,
            "cooperative finish",
        ))
    }
}

#[tokio::test]
async fn shutdown_and_owner_drop_allow_handles_to_observe_normal_material_return() {
    for drop_owner in [false, true] {
        let entered = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let model = runner(
            Arc::new(ParkedGateway {
                entered: entered.clone(),
                finished: calls.clone(),
            }),
            vec![],
        );
        let mut session = Session::new(
            "conversation".into(),
            Context::new(),
            move |_, _| {
                let model = model.clone();
                async move { Ok((model, TurnRunOptions::new(ModelRef::new("parked")))) }
            },
            config(1),
        );
        let handle = session.handle();
        let input = vec![block("input")];
        let receipt = handle.submit(request("key", input.clone())).unwrap();
        tokio::time::timeout(BOUND, entered.notified())
            .await
            .unwrap();
        if drop_owner {
            drop(session);
        } else {
            let mut shutdown = Box::pin(session.shutdown());
            assert!(futures_util::poll!(&mut shutdown).is_pending());
            let candidate = vec![block("during shutdown")];
            let refused = handle
                .submit(request("during shutdown", candidate.clone()))
                .unwrap_err();
            assert!(matches!(refused.reason, SessionError::Closed));
            assert_eq!(refused.request.blocks, candidate);
            tokio::time::timeout(BOUND, shutdown).await.unwrap();
            assert_eq!(context(&mut session), input.clone());
            session.shutdown().await;
        }
        let result = finished(&handle, &receipt.work).await;
        assert!(matches!(
            &*result,
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. }
                    | TurnInterruption::ModelCallFailed { .. }
            })
        ));
        // The runner may select cancellation before the gateway returns. It must still
        // return a typed runner result rather than being aborted with the owner.
        assert!(calls.load(Ordering::SeqCst) <= 1);
        assert_eq!(
            handle.submit(request("key", input.clone())).unwrap(),
            receipt
        );
        let rejection = handle.submit(request("new", input.clone())).unwrap_err();
        assert!(matches!(rejection.reason, SessionError::Closed));
        assert_eq!(rejection.request.blocks, input);
    }
}

#[tokio::test]
async fn completed_result_stays_stable_and_old_cancel_does_not_stop_new_work() {
    let gateway = Gateway::new(vec![final_output("first"), final_output("second")]);
    let mut session = simple_session(Context::new(), 2, gateway);
    let handle = session.handle();
    let first = handle.submit(request("first", vec![])).unwrap();
    let old = finished(&handle, &first.work).await;
    let second = handle.submit(request("second", vec![])).unwrap();
    handle.cancel(&first.work).unwrap();
    completed(&*finished(&handle, &second.work).await, "second");
    completed(&old, "first");
    assert!(Arc::ptr_eq(&old, &finished(&handle, &first.work).await));
    assert_eq!(context(&mut session).len(), 2);
    let foreign = WorkRef {
        conversation_id: "other".into(),
        turn_id: first.work.turn_id.clone(),
    };
    assert!(matches!(
        handle.observe(&foreign),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        handle.cancel(&foreign),
        Err(SessionError::NotFound { .. })
    ));
}

#[tokio::test]
async fn worker_panic_faults_without_inventing_context_and_replay_still_works() {
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        |_, _| async {
            panic!("assembly fault");
            #[allow(unreachable_code)]
            Ok((
                runner(Gateway::new(vec![]), vec![]),
                TurnRunOptions::new(ModelRef::new("unused")),
            ))
        },
        config(1),
    );
    let handle = session.handle();
    let original = vec![block("input")];
    let receipt = handle.submit(request("key", original.clone())).unwrap();
    assert!(matches!(
        &*finished(&handle, &receipt.work).await,
        WorkResult::Faulted(_)
    ));
    assert_eq!(
        handle.submit(request("key", original.clone())).unwrap(),
        receipt
    );
    let rejection = handle.submit(request("new", original.clone())).unwrap_err();
    let conflict = handle
        .submit(request("key", vec![block("changed after fault")]))
        .unwrap_err();
    assert!(matches!(conflict.reason, SessionError::Conflict { work } if work == receipt.work));
    assert!(matches!(rejection.reason, SessionError::Faulted));
    assert_eq!(rejection.request.blocks, original);
    assert!(matches!(
        session.with_context(|_| ()),
        Err(SessionError::ContextUnavailable)
    ));
}

struct EditingFailure;
#[async_trait]
impl ContextPreparer for EditingFailure {
    async fn prepare(
        &self,
        context: &mut Context,
        _: &RoundInfo<'_>,
        _: &CallControl,
    ) -> Result<ContextFrame, PrepareError> {
        context
            .edit()
            .append([block("prepare edit")])
            .commit()
            .unwrap();
        Err(PrepareError::new("original preparation error"))
    }
}
#[tokio::test]
async fn runner_failures_retain_typed_values_and_successful_prepare_edits() {
    let gateway = Gateway::new(vec![]);
    let model = runner(gateway.clone(), vec![]);
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            let model = model.clone();
            async move {
                let options = TurnRunOptions::new(ModelRef::new("fixture"))
                    .with_preparer(Arc::new(EditingFailure));
                Ok((model, options))
            }
        },
        config(1),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![block("input")])).unwrap();
    let result = finished(&handle, &receipt.work).await;
    assert!(
        matches!(&*result, WorkResult::Run(TurnResult::Interrupted { cause: TurnInterruption::PrepareFailed { error, .. } }) if error.message == "original preparation error")
    );
    assert_eq!(context(&mut session).len(), 2);
    assert!(gateway.requests.lock().unwrap().is_empty());

    let gateway = Gateway::results(vec![Err(ModelInvokeError::new(
        ModelInvokeErrorKind::UnknownOutcome,
        "original gateway error",
    ))]);
    let mut session = simple_session(Context::new(), 1, gateway);
    let handle = session.handle();
    let input = vec![block("input")];
    let receipt = handle.submit(request("key", input.clone())).unwrap();
    assert!(
        matches!(&*finished(&handle, &receipt.work).await, WorkResult::Run(TurnResult::Interrupted { cause: TurnInterruption::ModelCallFailed { error, .. } }) if error.kind == ModelInvokeErrorKind::UnknownOutcome && error.message == "original gateway error")
    );
    assert_eq!(context(&mut session), input);
}

#[tokio::test]
async fn malformed_output_is_returned_as_raw_typed_failure_without_material_commit() {
    let mut malformed = tool_output(&[" "]);
    malformed
        .response
        .items
        .insert(0, ModelItem::text("original body"));
    let gateway = Gateway::new(vec![malformed.clone()]);
    let mut session = simple_session(Context::new(), 1, gateway);
    let handle = session.handle();
    let input = vec![block("input")];
    let receipt = handle.submit(request("key", input.clone())).unwrap();
    let result = finished(&handle, &receipt.work).await;
    let WorkResult::Run(TurnResult::Interrupted {
        cause:
            TurnInterruption::ModelConversionFailed {
                error,
                output,
                block_ids,
                ..
            },
    }) = &*result
    else {
        panic!("{result:?}")
    };
    assert!(matches!(
        error,
        ModelBlockError::EmptyToolName { tool_index: 0, .. }
    ));
    assert_eq!(output.response.text(), "original body");
    let calls: Vec<_> = output.response.tool_calls().collect();
    let malformed_calls: Vec<_> = malformed.response.tool_calls().collect();
    assert_eq!(calls[0].tool_name, " ");
    assert_eq!(
        calls[0].provider_call_id,
        malformed_calls[0].provider_call_id
    );
    assert_eq!(block_ids.len(), 2);
    assert_eq!(context(&mut session), input);
}

#[tokio::test]
async fn deadline_covers_a_pending_assembly_and_returns_accepted_input() {
    let entered = Arc::new(Notify::new());
    let notice = entered.clone();
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, control| {
            let notice = notice.clone();
            async move {
                assert!(control.deadline().is_some());
                notice.notify_one();
                std::future::pending::<
                    Result<
                        (Arc<TurnRunner>, TurnRunOptions),
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                >()
                .await
            }
        },
        SessionConfig {
            retained_work_capacity: 1,
            work_timeout: Some(Duration::from_millis(50)),
        },
    );
    let handle = session.handle();
    let input = vec![block("retained")];
    let receipt = handle.submit(request("key", input.clone())).unwrap();
    tokio::time::timeout(BOUND, entered.notified())
        .await
        .unwrap();
    let result = finished(&handle, &receipt.work).await;
    assert!(matches!(
        &*result,
        WorkResult::StoppedBeforeRun(ControlError::TimedOut)
    ));
    assert_eq!(context(&mut session), input);
}

struct CollisionResult(BlockId);
#[async_trait]
impl ToolBatchProcessor for CollisionResult {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        _: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        let call = batch.calls()[0].call().clone();
        batch
            .resolve_at(
                0,
                self.0,
                ToolResultPayload::new(
                    call.call_block_id,
                    ToolResultStatus::Rejected,
                    ToolOutput::new(serde_json::json!({"raw":true})),
                )
                .with_media(vec![MediaRef::new("image/png", "opaque asset")])
                .with_notes(vec![TextPayload::new("original note")]),
            )
            .unwrap();
        Ok(())
    }
}
#[tokio::test]
async fn tool_commit_failure_keeps_raw_edit_input_and_owned_batch() {
    let existing = block("existing");
    let id = existing.id();
    // A bound target is needed so binding does not reject the call before the
    // processor supplies its result.
    let target = Arc::new(UnknownResultTool);
    let model = Arc::new(TurnRunner::new(
        Gateway::new(vec![tool_output(&["unknown"])]),
        Arc::new(
            ToolExecutor::new(
                vec![target],
                ToolExecutorOptions::default().with_before(vec![Arc::new(CollisionResult(id))]),
            )
            .unwrap(),
        ),
    ));
    let mut session = Session::new(
        "conversation".into(),
        Context::from_blocks(vec![existing.clone()]).unwrap(),
        move |_, _| {
            let model = model.clone();
            async move { Ok((model, TurnRunOptions::new(ModelRef::new("fixture")))) }
        },
        config(1),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![])).unwrap();
    let old = finished(&handle, &receipt.work).await;
    let WorkResult::Run(TurnResult::Interrupted {
        cause: TurnInterruption::ToolCommitFailed { error, .. },
    }) = &*old
    else {
        panic!("{old:?}")
    };
    assert!(matches!(error.reason, EditError::BlockIdentityMismatch(actual) if actual == id));
    assert_eq!(error.appended.len(), 1);
    assert_eq!(error.appended[0].id(), id);
    let BlockContent::ToolResult(raw) = error.appended[0].content() else {
        panic!("raw result input lost")
    };
    assert_eq!(raw.output.content, serde_json::json!({"raw":true}));
    assert_eq!(raw.notes[0].0, "original note");
    let batch = session
        .take_uncommitted_tool_batch(&receipt.work)
        .unwrap()
        .unwrap();
    assert_eq!(batch.completed_len(), 1);
    assert_eq!(
        batch.results()[0].result().unwrap().1.media[0].reference,
        "opaque asset"
    );
    assert!(
        session
            .take_uncommitted_tool_batch(&receipt.work)
            .unwrap()
            .is_none()
    );
    // The runner records the bound tools before the model call, then the
    // declaration; the uncommitted result is the only thing left out.
    let materials = context(&mut session);
    assert_eq!(materials[0], existing);
    assert!(matches!(
        materials[1].content(),
        BlockContent::ToolChange(_)
    ));
    assert!(matches!(materials[2].content(), BlockContent::ToolCall(_)));
    assert_eq!(materials.len(), 3);
    assert!(Arc::ptr_eq(&old, &finished(&handle, &receipt.work).await));
}

struct UnknownResultTool;
#[async_trait]
impl Tool for UnknownResultTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("unknown", "fixture", serde_json::json!({"type":"object"}))
    }
    async fn execute(
        &self,
        call: &ToolCallContext,
        _: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        ToolResultPayload::new(
            call.call_block_id,
            ToolResultStatus::UnknownOutcome,
            ToolOutput::new(serde_json::json!({"raw":true})),
        )
    }
}
#[tokio::test]
async fn unknown_tool_outcome_commits_result_without_leaving_a_second_batch() {
    let model = runner(
        Gateway::new(vec![tool_output(&["unknown"])]),
        vec![Arc::new(UnknownResultTool)],
    );
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            let model = model.clone();
            async move { Ok((model, TurnRunOptions::new(ModelRef::new("fixture")))) }
        },
        config(1),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![])).unwrap();
    let result = finished(&handle, &receipt.work).await;
    assert!(matches!(
        &*result,
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::UnknownToolOutcome { .. }
        })
    ));
    let materials = context(&mut session);
    assert_eq!(materials.len(), 3);
    assert!(matches!(
        materials[0].content(),
        BlockContent::ToolChange(_)
    ));
    let BlockContent::ToolResult(raw) = materials[2].content() else {
        panic!("missing actual result")
    };
    assert_eq!(raw.status, ToolResultStatus::UnknownOutcome);
    assert_eq!(raw.call_block_id, materials[1].id());
    assert!(
        session
            .take_uncommitted_tool_batch(&receipt.work)
            .unwrap()
            .is_none()
    );
}

struct DeadlineGateway {
    expected: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    received: Arc<AtomicUsize>,
}
#[async_trait]
impl ModelGateway for DeadlineGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let expected = *self.expected.lock().unwrap();
        assert!(expected.is_some());
        assert_eq!(control.deadline(), expected);
        self.received.fetch_add(1, Ordering::SeqCst);
        Ok(final_output("same absolute deadline"))
    }
}
#[tokio::test]
async fn assembly_and_actual_model_receive_the_same_absolute_deadline() {
    let expected = Arc::new(std::sync::Mutex::new(None));
    let received = Arc::new(AtomicUsize::new(0));
    let model = runner(
        Arc::new(DeadlineGateway {
            expected: expected.clone(),
            received: received.clone(),
        }),
        vec![],
    );
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, control| {
            let expected = expected.clone();
            let model = model.clone();
            async move {
                *expected.lock().unwrap() = control.deadline();
                tokio::task::yield_now().await;
                Ok((model, TurnRunOptions::new(ModelRef::new("fixture"))))
            }
        },
        SessionConfig {
            retained_work_capacity: 1,
            work_timeout: Some(Duration::from_secs(2)),
        },
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![])).unwrap();
    completed(
        &*finished(&handle, &receipt.work).await,
        "same absolute deadline",
    );
    assert_eq!(received.load(Ordering::SeqCst), 1);
    assert_eq!(context(&mut session).len(), 1);
}
