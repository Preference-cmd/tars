use super::*;
use causa::kernel::{BlockContent, BlockId, BlockMeta, Context, ContextBlock, TurnId};
use causa::runtime::{TurnRunOptions, TurnRunner};
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;

type AssemblyResult = Result<(Arc<TurnRunner>, TurnRunOptions), Box<dyn Error + Send + Sync>>;

fn failed_assembly() -> AssemblyResult {
    Err(Box::new(std::io::Error::other("assembly failed")))
}

fn config() -> SessionConfig {
    SessionConfig {
        retained_work_capacity: 8,
        work_timeout: None,
    }
}

fn request(key: &str) -> SubmitRequest {
    SubmitRequest {
        request_key: key.to_owned(),
        blocks: vec![ContextBlock::new(
            BlockId::new(uuid::Uuid::from_u128(17)),
            BlockContent::Message {
                role: causa::kernel::Role::User,
                parts: Vec::new(),
            },
            BlockMeta::from_source("test"),
        )],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_before_worker_poll_skips_assembly_and_returns_appended_material() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut session = Session::new(
        "pre-cancel".into(),
        Context::new(),
        {
            let calls = calls.clone();
            move |_, _| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { failed_assembly() }
            }
        },
        config(),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("cancelled")).unwrap();
    // This synchronous sequence cannot yield to the current-thread worker.
    handle.cancel(&receipt.work).unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(observation.state, WorkState::Finished(result) if matches!(&*result, WorkResult::StoppedBeforeRun(causa::kernel::ControlError::Cancelled)))
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn injected_identity_collision_rejects_before_append_and_replay_does_not_allocate() {
    let allocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut session = Session::new(
        "collision".into(),
        Context::new(),
        |_, _| async { failed_assembly() },
        config(),
    );
    let counter = allocations.clone();
    session.set_candidate(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        TurnId::new("fixed-candidate")
    });
    let handle = session.handle();
    let original = request("first");
    let receipt = handle.submit(original.clone()).unwrap();
    assert!(matches!(
        handle
            .wait(&receipt.work, Duration::from_secs(3))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
    assert_eq!(handle.submit(original).unwrap(), receipt);
    assert_eq!(allocations.load(std::sync::atomic::Ordering::SeqCst), 1);
    let next = request("second");
    let failure = handle.submit(next.clone()).unwrap_err();
    assert_eq!(
        failure.reason,
        SessionError::IdentityCollision { work: receipt.work }
    );
    assert_eq!(failure.request, next);
    assert_eq!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap(),
        1
    );
}

#[test]
fn absent_runtime_rejects_complete_input_without_edits_or_reserved_key() {
    let mut session = Session::new(
        "outside".into(),
        Context::new(),
        |_, _| async { failed_assembly() },
        config(),
    );
    let original = request("key");
    let rejection = session.handle().submit(original.clone()).unwrap_err();
    assert_eq!(rejection.reason, SessionError::RuntimeUnavailable);
    assert_eq!(rejection.request, original);
    assert_eq!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap(),
        0
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        let handle = session.handle();
        let receipt = handle.submit(original).unwrap();
        assert!(matches!(
            handle
                .wait(&receipt.work, Duration::from_secs(3))
                .await
                .unwrap()
                .state,
            WorkState::Finished(_)
        ));
    });
}

#[test]
fn runtime_without_time_driver_is_rejected_before_edit() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut session = Session::new(
            "no-time".into(),
            Context::new(),
            |_, _| async { failed_assembly() },
            config(),
        );
        let original = request("key");
        let rejection = session.handle().submit(original.clone()).unwrap_err();
        assert_eq!(rejection.reason, SessionError::RuntimeUnavailable);
        assert_eq!(rejection.request, original);
        assert_eq!(
            session
                .with_context(|context| context.blocks().len())
                .unwrap(),
            0
        );
    });
}

#[tokio::test]
async fn unrepresentable_deadline_rejects_before_edit() {
    let mut cfg = config();
    cfg.work_timeout = Some(Duration::MAX);
    let mut session = Session::new(
        "deadline".into(),
        Context::new(),
        |_, _| async { failed_assembly() },
        cfg,
    );
    let original = request("key");
    let rejection = session.handle().submit(original.clone()).unwrap_err();
    assert_eq!(rejection.reason, SessionError::DeadlineUnrepresentable);
    assert_eq!(rejection.request, original);
    assert_eq!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn supervised_assembly_panic_publishes_fault_and_never_fabricates_material() {
    let mut session = Session::new(
        "panic".into(),
        Context::new(),
        |_, _| async {
            panic!("injected worker failure");
            #[allow(unreachable_code)]
            failed_assembly()
        },
        config(),
    );
    let handle = session.handle();
    let original = request("first");
    let receipt = handle.submit(original.clone()).unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("supervisor must finish");
    };
    assert!(
        matches!(&*result, WorkResult::Faulted(WorkerFault::Panicked { message }) if message == "injected worker failure")
    );
    assert_eq!(handle.submit(original).unwrap(), receipt);
    assert_eq!(
        handle.submit(request("new")).unwrap_err().reason,
        SessionError::Faulted
    );
    assert_eq!(
        session.with_context(|_| ()).unwrap_err(),
        SessionError::ContextUnavailable
    );
    session.shutdown().await;
    assert!(matches!(
        handle.observe(&receipt.work).unwrap().state,
        WorkState::Finished(_)
    ));
}

#[tokio::test]
async fn owner_callback_panic_marks_fault_and_still_allows_retained_replay() {
    let mut session = Session::new(
        "poison".into(),
        Context::new(),
        |_, _| async { failed_assembly() },
        config(),
    );
    let handle = session.handle();
    let original = request("first");
    let receipt = handle.submit(original.clone()).unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = session.with_context(|context| {
                context.edit().replace(0..1, Vec::new()).commit().unwrap();
                panic!("owner callback failure");
            });
        }))
        .is_err()
    );
    assert_eq!(handle.submit(original).unwrap(), receipt);
    assert_eq!(
        handle.submit(request("new")).unwrap_err().reason,
        SessionError::Faulted
    );
    assert_eq!(
        session.with_context(|_| ()).unwrap_err(),
        SessionError::Faulted
    );
    session.shutdown().await;
}

#[tokio::test]
async fn abnormal_worker_abort_is_supervised_without_recreating_context() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let mut session = Session::new(
        "abort".into(),
        Context::new(),
        {
            let entered = entered.clone();
            move |_, _| {
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    std::future::pending::<AssemblyResult>().await
                }
            }
        },
        config(),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("active")).unwrap();
    tokio::time::timeout(Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    {
        let state = session.shared.lock();
        let record = session.shared.record(&state, &receipt.work).unwrap();
        record.abort.lock().unwrap().as_ref().unwrap().abort();
    }
    let observed = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(observed.state, WorkState::Finished(result) if matches!(&*result, WorkResult::Faulted(WorkerFault::Cancelled)))
    );
    assert_eq!(
        session.with_context(|_| ()).unwrap_err(),
        SessionError::ContextUnavailable
    );
    assert_eq!(
        handle.submit(request("next")).unwrap_err().reason,
        SessionError::Faulted
    );
    session.shutdown().await;
}

struct FinalGateway;

#[async_trait::async_trait]
impl causa::kernel::ModelGateway for FinalGateway {
    async fn invoke(
        &self,
        _: &causa::kernel::ModelRequest,
        _: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        use causa::kernel::{ModelItem, ModelOutput, ModelResponse, ModelStopReason};
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("finished")],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

#[tokio::test]
async fn cancellation_between_runner_return_and_publication_cannot_reclassify_result() {
    use causa::kernel::ModelRef;
    use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnResult};
    let runner = Arc::new(TurnRunner::new(
        Arc::new(FinalGateway),
        Arc::new(ToolExecutor::new(Vec::new(), ToolExecutorOptions::default()).unwrap()),
    ));
    let mut session = Session::new(
        "frozen".into(),
        Context::new(),
        move |_, _| {
            let runner = runner.clone();
            async move { Ok((runner, TurnRunOptions::new(ModelRef::new("fixture")))) }
        },
        config(),
    );
    let handle = session.handle();
    let receipt = handle.submit(request("once")).unwrap();
    let selected = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    {
        let state = session.shared.lock();
        let record = session.shared.record(&state, &receipt.work).unwrap();
        *record.before_publish.lock().unwrap() = Some((selected.clone(), release.clone()));
    }
    tokio::time::timeout(Duration::from_secs(3), selected.notified())
        .await
        .unwrap();
    assert!(matches!(
        handle.observe(&receipt.work).unwrap().state,
        WorkState::Running
    ));
    handle.cancel(&receipt.work).unwrap();
    release.notify_one();
    let observed = handle
        .wait(&receipt.work, Duration::from_secs(3))
        .await
        .unwrap();
    assert!(
        matches!(observed.state, WorkState::Finished(result) if matches!(&*result, WorkResult::Run(TurnResult::Completed { final_output }) if final_output.response.text() == "finished"))
    );
    assert_eq!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap(),
        2
    );
}
