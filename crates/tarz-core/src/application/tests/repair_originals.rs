use super::support::*;
use causa::kernel::{
    CallControl, ModelGateway, ModelInvokeError, ModelItem, ModelOutput, ModelRequest,
    ModelResponse, ModelStopReason, TextPayload, Tool, ToolCallContext, ToolCallDraft,
    ToolDefinition, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::runtime::RunEvent;

// This fixture explicitly grants the private assembly provenance only to test
// the adoption contract. Product provenance is separately exercised through
// the unchanged public assembly and real HTTP/workspace-tool consumers.
#[derive(Clone, Copy)]
enum Shape {
    Pending,
    Mixed,
    Deadline,
    Complete,
    InvalidPayload,
    Media,
    Unknown,
}

struct Model(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl ModelGateway for Model {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        let mut items = vec![ModelItem::text(if first {
            "original tool request"
        } else {
            "new work done"
        })];
        if first {
            items.extend((0..2).map(|index| {
                ModelItem::tool_call({
                    let mut call =
                        ToolCallDraft::new("read_file", serde_json::json!({"index":index}));
                    call.provider_call_id = Some(format!("original-{index}"));
                    call
                })
            }));
        }
        Ok(ModelOutput::new(
            ModelResponse { items },
            if first {
                ModelStopReason::ToolUse
            } else {
                ModelStopReason::EndTurn
            },
        ))
    }
}
struct FixtureTool {
    shape: Shape,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Tool for FixtureTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "read_file",
            "deterministic contract fixture",
            serde_json::json!({"type":"object"}),
        )
    }
    async fn execute(&self, call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(self.shape, Shape::Mixed | Shape::Deadline)
            && call.input.arguments["index"] == 1
        {
            if matches!(self.shape, Shape::Mixed) {
                control.cancellation_token().cancel();
            }
            std::future::pending::<()>().await;
        }
        {
            let mut result = ToolResultPayload::new(
                if matches!(self.shape, Shape::InvalidPayload) {
                    BlockId::new(uuid::Uuid::now_v7())
                } else {
                    call.call_block_id
                },
                if matches!(self.shape, Shape::Unknown) {
                    ToolResultStatus::UnknownOutcome
                } else {
                    ToolResultStatus::Succeeded
                },
                ToolOutput::new(
                    serde_json::json!({"original":[1,2,3],"index":call.input.arguments["index"]}),
                ),
            );
            result.notes = vec![TextPayload::new("unchanged original note")];
            result.media = if matches!(self.shape, Shape::Media) {
                vec![causa::kernel::MediaRef::new(
                    "image/png",
                    "unsupported-original",
                )]
            } else {
                vec![]
            };
            result
        }
    }
}

async fn interrupted(
    shape: Shape,
    standard: bool,
) -> (
    SessionHost,
    ManagedSession,
    crate::session::WorkRef,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let host = SessionHost::new(None);
    let model_calls = Arc::new(AtomicUsize::new(0));
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(TurnRunner::new(
        Arc::new(Model(model_calls.clone())),
        Arc::new(
            ToolExecutor::new(
                vec![Arc::new(FixtureTool {
                    shape,
                    calls: tool_calls.clone(),
                })],
                ToolExecutorOptions::default(),
            )
            .unwrap(),
        ),
    ));
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        let runner = runner.clone();
        let options = definition.run_options();
        let owner = Session::new(
            id,
            context,
            move |_, control| {
                let runner = runner.clone();
                let mut options = options.clone();
                options.observer = Some(Arc::new(move |event| {
                    if (matches!(shape, Shape::Pending)
                        && matches!(event, RunEvent::ToolBatchReady { .. }))
                        || (!matches!(shape, Shape::Pending | Shape::Mixed | Shape::Deadline)
                            && matches!(event, RunEvent::ToolBatchReturned { .. }))
                    {
                        control.cancellation_token().cancel();
                    }
                }));
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        );
        Ok(if standard {
            owner.allow_readonly_repair()
        } else {
            owner
        })
    }));
    let mut def = definition();
    def.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    def.workspace_root = Some(std::env::current_dir().unwrap());
    def.max_tool_calls = 4;
    def.max_model_rounds = 4;
    if matches!(shape, Shape::Deadline) {
        def.work_timeout_secs = Some(1);
    }
    let session = host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();
    let work = session
        .submit(SubmitRequest {
            request_key: "original".into(),
            blocks: vec![text_input("start fixture")],
        })
        .unwrap()
        .work;
    finished(&session, &work).await;
    session.wait_saved().await.unwrap();
    (host, session, work, model_calls, tool_calls)
}

#[tokio::test]
async fn repair_original_pending_mixed_complete_and_unknown_keep_exact_facts_and_terminal() {
    for shape in [
        Shape::Pending,
        Shape::Mixed,
        Shape::Deadline,
        Shape::Complete,
        Shape::Unknown,
    ] {
        let (host, session, work, models, tools) = interrupted(shape, true).await;
        let WorkState::Finished(original_result) = session.observe(&work).unwrap().state else {
            panic!("terminal")
        };
        let inspection = session.inspect_repair(Some(&work)).unwrap();
        let original = inspection.original.unwrap();
        assert!(original.available);
        assert!(original.adoption_error.is_none());
        assert_eq!(
            original.completed_count,
            if matches!(shape, Shape::Pending) {
                0
            } else {
                2
            }
        );
        if matches!(shape, Shape::Mixed | Shape::Deadline) {
            assert_eq!(
                original
                    .results
                    .iter()
                    .filter(|r| r.status == ToolResultStatus::Succeeded)
                    .count(),
                1
            );
            assert_eq!(
                original
                    .results
                    .iter()
                    .filter(|r| r.status == ToolResultStatus::UnknownOutcome)
                    .count(),
                1
            );
        }
        let before = session.read_materials().unwrap();
        let discarded = session
            .plan_repair(RepairMode::AdoptOriginal(work.clone()))
            .unwrap();
        drop(discarded);
        assert!(
            session
                .inspect_repair(Some(&work))
                .unwrap()
                .original
                .unwrap()
                .available
        );
        let plan = session
            .plan_repair(RepairMode::AdoptOriginal(work.clone()))
            .unwrap();
        let expected: Vec<_> = session
            .entry
            .lock()
            .owner
            .as_ref()
            .unwrap()
            .inspect_uncommitted(&work, |_, _, batch| {
                batch
                    .unwrap()
                    .results()
                    .iter()
                    .map(|entry| {
                        let (id, payload) = entry.result().unwrap();
                        (*id, payload.clone())
                    })
                    .collect()
            })
            .unwrap();
        let retained: Vec<_> = before
            .context
            .blocks()
            .iter()
            .filter(|block| !plan.summary().removed.contains(&block.id()))
            .collect();
        assert!(
            plan.candidate()
                .blocks()
                .starts_with(&retained.into_iter().cloned().collect::<Vec<_>>())
        );
        for (id, payload) in &expected {
            let block = plan
                .candidate()
                .blocks()
                .iter()
                .find(|block| block.id() == *id)
                .unwrap();
            assert_eq!(block.content(), &BlockContent::ToolResult(payload.clone()));
        }
        let calls_before = tools.load(Ordering::SeqCst);
        session.apply_repair(&plan).unwrap();
        assert!(
            matches!(session.observe(&work).unwrap().state, WorkState::Finished(current) if Arc::ptr_eq(&original_result, &current))
        );
        let batch = host
            .take_uncommitted_tool_batch(&session.target(), &work)
            .unwrap()
            .unwrap();
        assert_eq!(batch.completed_len(), expected.len());
        assert!(
            host.take_uncommitted_tool_batch(&session.target(), &work)
                .unwrap()
                .is_none()
        );
        assert_eq!(tools.load(Ordering::SeqCst), calls_before);
        let next = session
            .submit(SubmitRequest {
                request_key: "new".into(),
                blocks: vec![text_input("continue with new work")],
            })
            .unwrap();
        finished(&session, &next.work).await;
        session.wait_saved().await.unwrap();
        assert_eq!(models.load(Ordering::SeqCst), 2);
        assert_eq!(tools.load(Ordering::SeqCst), calls_before);
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn repair_rejects_custom_assembly_invalid_payload_media_and_middle_exchange() {
    for (shape, standard) in [
        (Shape::Complete, false),
        (Shape::InvalidPayload, true),
        (Shape::Media, true),
    ] {
        let (host, session, work, models, tools) = interrupted(shape, standard).await;
        let before = session.read_materials().unwrap();
        let counts = (models.load(Ordering::SeqCst), tools.load(Ordering::SeqCst));
        let error = session
            .plan_repair(RepairMode::AdoptOriginal(work.clone()))
            .unwrap_err();
        assert!(matches!(
            error,
            ApplicationError::Repair(RepairError::UnsupportedSource | RepairError::Materials(_))
        ));
        assert_eq!(
            session.read_materials().unwrap().context.blocks(),
            before.context.blocks()
        );
        assert_eq!(session.read_materials().unwrap().version, before.version);
        assert_eq!(
            (models.load(Ordering::SeqCst), tools.load(Ordering::SeqCst)),
            counts
        );
        assert!(
            host.take_uncommitted_tool_batch(&session.target(), &work)
                .unwrap()
                .is_some()
        );
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
    let (host, session, work, _, _) = interrupted(Shape::Complete, true).await;
    let snapshot = session.read_materials().unwrap();
    let mut blocks = snapshot.context.into_blocks();
    blocks.push(text_input("interleaved later input"));
    session.edit(snapshot.version, blocks.clone()).unwrap();
    assert!(matches!(
        session.plan_repair(RepairMode::AdoptOriginal(work.clone())),
        Err(ApplicationError::Repair(RepairError::InvalidOriginal))
    ));
    assert_eq!(session.read_materials().unwrap().context.blocks(), blocks);
    assert!(
        host.take_uncommitted_tool_batch(&session.target(), &work)
            .unwrap()
            .is_some()
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn repair_claim_and_forget_races_linearize_with_apply_for_both_modes() {
    for remove in [false, true] {
        for forget in [false, true] {
            for _ in 0..4 {
                let (host, session, work, _, _) = interrupted(Shape::Complete, true).await;
                let plan = session
                    .plan_repair(if remove {
                        RepairMode::RemoveUnpaired
                    } else {
                        RepairMode::AdoptOriginal(work.clone())
                    })
                    .unwrap();
                let before = session.read_materials().unwrap();
                let barrier = std::sync::Barrier::new(2);
                let apply = std::thread::scope(|scope| {
                    let applied = scope.spawn(|| {
                        barrier.wait();
                        session.apply_repair(&plan)
                    });
                    let claimed = scope.spawn(|| {
                        barrier.wait();
                        if forget {
                            session.forget(&work)
                        } else {
                            host.take_uncommitted_tool_batch(&session.target(), &work)
                                .map(|batch| assert!(batch.is_some()))
                        }
                    });
                    claimed.join().unwrap().unwrap();
                    applied.join().unwrap()
                });
                let after = session.read_materials().unwrap();
                match apply {
                    Ok(_) => assert_eq!(after.context.blocks(), plan.candidate().blocks()),
                    Err(ApplicationError::Repair(RepairError::StaleEvidence)) => {
                        assert_eq!(after.context.blocks(), before.context.blocks());
                        assert_eq!(after.version, before.version);
                    }
                    other => panic!("unexpected race result: {other:?}"),
                }
                host.close(&session.target(), ClosePolicy::Save)
                    .await
                    .unwrap();
            }
        }
    }
}

struct FailingProcessor;
#[async_trait::async_trait]
impl causa::kernel::ToolBatchProcessor for FailingProcessor {
    async fn process(
        &self,
        _: &mut causa::kernel::ToolBatch,
        _: &causa::kernel::ProcessorContext<'_>,
    ) -> Result<(), causa::kernel::ProcessorError> {
        Err(causa::kernel::ProcessorError::Failed(
            "retained processor failure".into(),
        ))
    }
}

#[tokio::test]
async fn repair_processor_failures_and_commit_collisions_cannot_be_adopted_as_controlled_stops() {
    for phase in ["before", "after", "collision"] {
        let host = SessionHost::new(None);
        let existing = text_input("existing identity must not be overwritten");
        let mut execution = ToolExecutorOptions::default();
        match phase {
            "before" => execution.before.push(Arc::new(FailingProcessor)),
            "after" => execution.after.push(Arc::new(FailingProcessor)),
            _ => execution
                .before
                .push(Arc::new(CollisionBatch(existing.id()))),
        }
        let runner = Arc::new(TurnRunner::new(
            Arc::new(Model(Arc::new(AtomicUsize::new(0)))),
            Arc::new(
                ToolExecutor::new(
                    vec![Arc::new(FixtureTool {
                        shape: Shape::Complete,
                        calls: Arc::new(AtomicUsize::new(0)),
                    })],
                    execution,
                )
                .unwrap(),
            ),
        ));
        *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
            let runner = runner.clone();
            let options = definition.run_options();
            Ok(Session::new(
                id,
                context,
                move |_, _| {
                    let runner = runner.clone();
                    let options = options.clone();
                    async move { Ok((runner, options)) }
                },
                definition.session_config(),
            )
            .allow_readonly_repair())
        }));
        let mut def = definition();
        def.max_model_rounds = 4;
        def.max_tool_calls = 4;
        let session = host
            .create(PreparedCreate::transient(
                def,
                Context::from_blocks(vec![existing.clone()]).unwrap(),
            ))
            .await
            .unwrap();
        let work = session.submit(request("failing")).unwrap().work;
        finished(&session, &work).await;
        session.wait_saved().await.unwrap();
        let observation = session.observe(&work).unwrap();
        assert!(
            matches!(observation.state, WorkState::Finished(result) if matches!(result.as_ref(), WorkResult::Run(causa::runtime::TurnResult::Interrupted { cause: causa::runtime::TurnInterruption::ToolProcessingFailed { .. } | causa::runtime::TurnInterruption::ToolCommitFailed { .. } })))
        );
        let before = session.read_materials().unwrap();
        assert!(matches!(
            session.plan_repair(RepairMode::AdoptOriginal(work.clone())),
            Err(ApplicationError::Repair(RepairError::UnsupportedSource))
        ));
        assert_eq!(session.read_materials().unwrap().version, before.version);
        assert_eq!(
            session.read_materials().unwrap().context.blocks()[0],
            existing
        );
        assert!(
            host.take_uncommitted_tool_batch(&session.target(), &work)
                .unwrap()
                .is_some()
        );
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn repair_completed_claim_and_forget_always_invalidate_already_prepared_candidates() {
    for forget in [false, true] {
        let (host, session, work, _, _) = interrupted(Shape::Complete, true).await;
        let adopt = session
            .plan_repair(RepairMode::AdoptOriginal(work.clone()))
            .unwrap();
        let remove = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
        let before = session.read_materials().unwrap();
        if forget {
            session.forget(&work).unwrap();
        } else {
            host.take_uncommitted_tool_batch(&session.target(), &work)
                .unwrap()
                .unwrap();
        }
        for plan in [&adopt, &remove] {
            assert!(matches!(
                session.apply_repair(plan),
                Err(ApplicationError::Repair(RepairError::StaleEvidence))
            ));
        }
        assert_eq!(session.read_materials().unwrap().version, before.version);
        assert_eq!(
            session.read_materials().unwrap().context.blocks(),
            before.context.blocks()
        );
        let refreshed = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
        session.apply_repair(&refreshed).unwrap();
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}
