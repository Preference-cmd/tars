mod common;
use async_trait::async_trait;
use causa::{kernel::*, runtime::*};
use common::*;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tarz_core::{material, session::*};

struct BoundTool {
    work: WorkRef,
    calls: Arc<Mutex<Vec<WorkRef>>>,
}
#[async_trait]
impl Tool for BoundTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("echo", "fixture", serde_json::json!({"type":"object"}))
    }
    async fn execute(
        &self,
        call: &ToolCallContext,
        _: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        self.calls.lock().unwrap().push(self.work.clone());
        ToolResultPayload::new(
            call.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(call.input.arguments.clone()),
        )
        .with_notes(vec![TextPayload::new(self.work.turn_id.0.as_str())])
    }
}
struct BoundPreparer {
    work: WorkRef,
    seen: Arc<Mutex<Vec<WorkRef>>>,
}
#[async_trait]
impl ContextPreparer for BoundPreparer {
    async fn prepare(
        &self,
        context: &mut Context,
        round: &RoundInfo<'_>,
        _: &CallControl,
    ) -> Result<ContextFrame, PrepareError> {
        assert_eq!(round.invocation_id.turn_id, self.work.turn_id);
        assert_eq!(
            round.model.0,
            format!("model:{}", self.work.turn_id.0.as_str())
        );
        self.seen.lock().unwrap().push(self.work.clone());
        Ok(context.frame())
    }
}
struct BoundProcessor {
    work: WorkRef,
    seen: Arc<Mutex<Vec<WorkRef>>>,
}
#[async_trait]
impl ToolBatchProcessor for BoundProcessor {
    async fn process(
        &self,
        _: &mut ToolBatch,
        context: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        assert_eq!(context.turn_id, &self.work.turn_id);
        self.seen.lock().unwrap().push(self.work.clone());
        Ok(())
    }
}

#[tokio::test]
async fn public_consumer_runs_tools_summarizes_replays_and_rebuilds_only_materials() {
    let initial = vec![block("I"), block("A"), block("B")];
    let gateway = Gateway::new(vec![
        tool_output(&["echo"]),
        final_output("T1 final"),
        tool_output(&["echo"]),
        final_output("T2 final"),
        final_output("restored final"),
    ]);
    let assemblies = Arc::new(Mutex::new(vec![]));
    let tools = Arc::new(Mutex::new(vec![]));
    let prepared = Arc::new(Mutex::new(vec![]));
    let processors = Arc::new(Mutex::new(vec![]));
    let observed = Arc::new(Mutex::new(vec![]));
    let assemble = {
        let gateway = gateway.clone();
        let assemblies = assemblies.clone();
        let tools = tools.clone();
        let prepared = prepared.clone();
        let processors = processors.clone();
        let observed = observed.clone();
        move |work: WorkRef, _: CallControl| {
            assemblies.lock().unwrap().push(work.clone());
            let gateway = gateway.clone();
            let tools = tools.clone();
            let prepared = prepared.clone();
            let processors = processors.clone();
            let observed = observed.clone();
            async move {
                let executor = ToolExecutor::new(
                    vec![Arc::new(BoundTool {
                        work: work.clone(),
                        calls: tools,
                    })],
                    ToolExecutorOptions::default().with_before(vec![Arc::new(BoundProcessor {
                        work: work.clone(),
                        seen: processors,
                    })]),
                )
                .unwrap();
                let runner = Arc::new(TurnRunner::new(gateway, Arc::new(executor)));
                let options = TurnRunOptions::new(ModelRef::new(format!(
                    "model:{}",
                    work.turn_id.0.as_str()
                )))
                .with_preparer(Arc::new(BoundPreparer {
                    work: work.clone(),
                    seen: prepared,
                }))
                .with_observer(Arc::new(move |event| {
                    if let RunEvent::ModelRequestReady { request } = event {
                        assert_eq!(request.invocation_id.turn_id, work.turn_id);
                        observed.lock().unwrap().push(work.clone());
                    }
                }));
                Ok((runner, options))
            }
        }
    };
    let mut session = Session::new(
        "conversation".into(),
        Context::from_blocks(initial.clone()).unwrap(),
        assemble.clone(),
        config(3),
    );
    assert_eq!(context(&mut session), initial);
    assert!(assemblies.lock().unwrap().is_empty());
    let handle = session.handle();
    let input = vec![block("U1")];
    let first = handle.submit(request("k1", input.clone())).unwrap();
    let old = finished(&handle, &first.work).await;
    completed(&old, "T1 final");
    assert_eq!(*tools.lock().unwrap(), vec![first.work.clone()]);
    assert_eq!(*processors.lock().unwrap(), vec![first.work.clone()]);
    let actual = context(&mut session);
    let call = actual
        .iter()
        .find(|b| matches!(b.content(), BlockContent::ToolCall(_)))
        .unwrap();
    let result = actual
        .iter()
        .find_map(|b| match b.content() {
            BlockContent::ToolResult(r) => Some(r),
            _ => None,
        })
        .unwrap();
    assert_eq!(result.call_block_id, call.id());
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    let summary = block("S");
    session
        .with_context(|c| c.edit().replace(1..3, [summary.clone()]).commit())
        .unwrap()
        .unwrap();
    let summarized = context(&mut session);
    assert_eq!(handle.submit(request("k1", input)).unwrap(), first);
    assert_eq!(context(&mut session), summarized);
    assert_eq!(assemblies.lock().unwrap().len(), 1);
    assert_eq!(gateway.requests.lock().unwrap().len(), 2);
    let u2 = block("U2");
    let second = handle.submit(request("k2", vec![u2.clone()])).unwrap();
    completed(&*finished(&handle, &second.work).await, "T2 final");
    completed(&old, "T1 final");
    {
        let requests = gateway.requests.lock().unwrap();
        for request in requests.iter() {
            assert_eq!(request.tool_surface.definitions.len(), 1);
            let advertised = &request.tool_surface.definitions[0];
            assert_eq!(advertised.name, "echo");
            assert_eq!(advertised.description, "fixture");
            assert_eq!(advertised.parameters, serde_json::json!({"type":"object"}));
        }
        let frame = &requests[2].frame.blocks;
        assert!(frame.contains(&summary));
        assert!(frame.contains(&u2));
        assert!(!frame.contains(&initial[1]));
        assert!(!frame.contains(&initial[2]));
        assert_eq!(
            requests[2].model.0,
            format!("model:{}", second.work.turn_id.0.as_str())
        );
    }
    assert_eq!(
        *assemblies.lock().unwrap(),
        vec![first.work.clone(), second.work.clone()]
    );
    assert_eq!(
        *tools.lock().unwrap(),
        vec![first.work.clone(), second.work.clone()]
    );
    assert_eq!(
        *processors.lock().unwrap(),
        vec![first.work.clone(), second.work.clone()]
    );
    assert!(prepared.lock().unwrap().contains(&second.work));
    assert!(observed.lock().unwrap().contains(&second.work));
    let saved = session.with_context(|c| c.clone()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("materials.json");
    material::save(&path, &saved).unwrap();
    let restored = material::load(&path).unwrap();
    assert_eq!(restored.blocks(), saved.blocks());
    let mut rebuilt = Session::new("conversation".into(), restored, assemble, config(1));
    let rebuilt_handle = rebuilt.handle();
    assert_eq!(context(&mut rebuilt), saved.blocks());
    for work in [&first.work, &second.work] {
        assert!(matches!(
            rebuilt_handle.observe(work),
            Err(SessionError::NotFound { .. })
        ));
    }
    let next = rebuilt_handle.submit(request("k1", vec![])).unwrap();
    assert_ne!(next.work, first.work);
    assert_ne!(next.work, second.work);
    completed(
        &*finished(&rebuilt_handle, &next.work).await,
        "restored final",
    );
    assert_eq!(
        *tools.lock().unwrap(),
        vec![first.work.clone(), second.work.clone()]
    );
    assert_eq!(
        gateway.requests.lock().unwrap()[4].frame.blocks,
        saved.blocks()
    );
    assert_eq!(assemblies.lock().unwrap().len(), 3);
    completed(&old, "T1 final");
}

struct PrefixResult;
#[async_trait]
impl ToolBatchProcessor for PrefixResult {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        _: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        let call = batch.calls()[0].call().clone();
        batch
            .resolve_at(
                0,
                new_block_id(),
                ToolResultPayload::new(
                    call.call_block_id,
                    ToolResultStatus::Succeeded,
                    ToolOutput::new(serde_json::json!({"preserved":true})),
                )
                .with_notes(vec![TextPayload::new("completed before dispatch")]),
            )
            .unwrap();
        Ok(())
    }
}
struct StopTool {
    name: &'static str,
    started: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for StopTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.name, "fixture", serde_json::json!({"type":"object"}))
    }
    async fn execute(
        &self,
        _: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        self.started.fetch_add(1, Ordering::SeqCst);
        control.cancellation_token().cancel();
        std::future::pending().await
    }
}
#[tokio::test]
async fn cancelled_batch_preserves_completed_unknown_pending_and_owner_takes_it_once() {
    let unknown = Arc::new(AtomicUsize::new(0));
    let unstarted = Arc::new(AtomicUsize::new(0));
    let gateway = Gateway::new(vec![
        tool_output(&["completed", "unknown", "unstarted"]),
        final_output("after explicit repair"),
    ]);
    let executor = ToolExecutor::new(
        vec![
            Arc::new(StopTool {
                name: "completed",
                started: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(StopTool {
                name: "unknown",
                started: unknown.clone(),
            }),
            Arc::new(StopTool {
                name: "unstarted",
                started: unstarted.clone(),
            }),
        ],
        ToolExecutorOptions::default().with_before(vec![Arc::new(PrefixResult)]),
    )
    .unwrap();
    let model = Arc::new(TurnRunner::new(gateway.clone(), Arc::new(executor)));
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            let model = model.clone();
            async move { Ok((model, TurnRunOptions::new(ModelRef::new("fixture")))) }
        },
        config(2),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("interrupted", vec![])).unwrap();
    let old = finished(&handle, &receipt.work).await;
    let second_observer = finished(&handle, &receipt.work).await;
    assert!(Arc::ptr_eq(&old, &second_observer));
    assert!(
        matches!(
            &*old,
            WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. }
            })
        ),
        "{old:?}"
    );
    let batch = session
        .take_uncommitted_tool_batch(&receipt.work)
        .unwrap()
        .unwrap();
    assert_eq!(batch.completed_len(), 2);
    assert_eq!(batch.calls().len(), 1);
    assert_eq!(batch.calls()[0].call().input.tool_name, "unstarted");
    assert_eq!(
        batch.results()[0].result().unwrap().1.status,
        ToolResultStatus::Succeeded
    );
    assert_eq!(
        batch.results()[0].result().unwrap().1.output.content,
        serde_json::json!({"preserved":true})
    );
    assert_eq!(
        batch.results()[1].result().unwrap().1.status,
        ToolResultStatus::UnknownOutcome
    );
    assert_eq!(unknown.load(Ordering::SeqCst), 1);
    assert_eq!(unstarted.load(Ordering::SeqCst), 0);
    assert!(
        session
            .take_uncommitted_tool_batch(&receipt.work)
            .unwrap()
            .is_none()
    );
    // Repair is an explicit owner decision: remove the partial exchange before
    // selecting a new request. Session never executes the returned batch.
    session
        .with_context(|c| {
            let len = c.blocks().len();
            c.edit()
                .replace(0..len, [block("explicit repair")])
                .commit()
        })
        .unwrap()
        .unwrap();
    let next = handle.submit(request("next", vec![])).unwrap();
    completed(
        &*finished(&handle, &next.work).await,
        "after explicit repair",
    );
    assert_eq!(unknown.load(Ordering::SeqCst), 1);
    assert_eq!(unstarted.load(Ordering::SeqCst), 0);
    assert!(Arc::ptr_eq(&old, &finished(&handle, &receipt.work).await));
    assert_eq!(batch.declaration_ids().len(), 3);
}

#[tokio::test]
async fn forget_discards_unclaimed_batch_and_preserves_material_and_old_result() {
    let gateway = Gateway::new(vec![tool_output(&["echo"])]);
    let model = runner(gateway, vec![]);
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        move |_, _| {
            let model = model.clone();
            async move {
                let mut options = TurnRunOptions::new(ModelRef::new("fixture"));
                options.limits.max_tool_calls = 0;
                Ok((model, options))
            }
        },
        config(1),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![])).unwrap();
    let old = finished(&handle, &receipt.work).await;
    assert!(matches!(
        &*old,
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxToolCalls { limit: 0, .. }
        })
    ));
    let materials = context(&mut session);
    assert_eq!(materials.len(), 1);
    session.forget(&receipt.work).unwrap();
    assert_eq!(context(&mut session), materials);
    assert!(matches!(
        session.take_uncommitted_tool_batch(&receipt.work),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        handle.observe(&receipt.work),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        &*old,
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxToolCalls { limit: 0, .. }
        })
    ));
}
