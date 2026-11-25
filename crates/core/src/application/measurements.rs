//! Opt-in resource probe and multi-agent quota / cycle tests of managed host and SQLite store operations.

use super::coordinator::{
    CoordinatorConfig, CoordinatorError, MessageAgentRequest, SpawnAgentOutcome, SpawnAgentRequest,
};
use super::{ClosePolicy, HostOptions, PreparedCreate, SaveStatus, SessionHost};
use crate::definition::{ChildNarrowingRequest, CredentialRef, Provider, SessionDefinition};
use crate::events::{AgentInteraction, OnParentComplete, OperationExecutionMode, OperationStatus};
use crate::session::{Session, SubmitRequest, WorkResult, WorkState};
use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelInvokeErrorKind, ModelItem,
    ModelOutput, ModelRequest, ModelResponse, ModelStopReason,
};
use causa::runtime::{ToolExecutor, TurnResult, TurnRunner};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

struct ProbeGateway(Arc<AtomicU64>);
#[async_trait]
impl ModelGateway for ProbeGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("measured completion")],
            },
            ModelStopReason::EndTurn,
        ))
    }
}
fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiResponses,
        model: "local-resource-probe".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARS_PROBE_KEY_REF".into()),
        work_timeout_secs: Some(60),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
        code_execution: None,
    }
}
fn fixture(host: &SessionHost, calls: Arc<AtomicU64>, assemblies: Arc<AtomicU64>) {
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        assemblies.fetch_add(1, Ordering::Relaxed);
        let runner = Arc::new(TurnRunner::new(
            Arc::new(ProbeGateway(calls.clone())),
            Arc::new(ToolExecutor::new(vec![], Default::default()).unwrap()),
        ));
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
        ))
    }));
}
async fn complete(view: &super::ManagedSession, key: &str) {
    let receipt = view
        .submit(SubmitRequest {
            request_key: key.into(),
            blocks: vec![super::text_input(key)],
        })
        .unwrap();
    let observation = view
        .wait(&receipt.work, Duration::from_secs(60))
        .await
        .unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("unfinished probe work")
    };
    assert!(matches!(
        &*result,
        WorkResult::Run(TurnResult::Completed { .. })
    ));
    view.wait_saved().await.unwrap();
}
fn millis(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Run only through the reproduction script, one isolated child per size case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "resource measurement; use the local reproduction script"]
async fn managed_resource_probe() {
    let case = std::env::var("TARS_MEASURE_CASE").unwrap_or_else(|_| "small".into());
    let (block_count, text_size, concurrent) = match case.as_str() {
        "small" => (16, 128, 1),
        "medium" => (1024, 1024, 1),
        "large" => (8192, 1024, 1),
        "concurrent" => (1024, 1024, 8),
        other => panic!("unknown measurement case: {other}"),
    };
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let calls = Arc::new(AtomicU64::new(0));
    let assemblies = Arc::new(AtomicU64::new(0));
    fixture(&host, calls.clone(), assemblies.clone());
    let context = Context::from_blocks(
        (0..block_count)
            .map(|_| super::text_input("x".repeat(text_size)))
            .collect(),
    )
    .unwrap();
    let material_bytes = serde_json::to_vec(&context).unwrap().len();
    let start = Instant::now();
    let mut jobs = Vec::new();
    for _ in 0..concurrent {
        let host = host.clone();
        let context = context.clone();
        jobs.push(tokio::spawn(async move {
            host.create(PreparedCreate::persistent(definition(), context))
                .await
                .unwrap()
        }));
    }
    let mut views = Vec::new();
    for job in jobs {
        views.push(job.await.unwrap());
    }
    let create_ms = millis(start);
    let start = Instant::now();
    let mut jobs = Vec::new();
    for view in &views {
        let view = view.clone();
        jobs.push(tokio::spawn(async move { complete(&view, "T1").await }));
    }
    for job in jobs {
        job.await.unwrap();
    }
    let t1_run_save_ms = millis(start);
    let start = Instant::now();
    let listed = host.list().await.unwrap();
    assert_eq!(listed.len(), concurrent);
    let list_ms = millis(start);
    let identities: Vec<_> = views
        .iter()
        .map(|view| view.target().conversation_id().clone())
        .collect();
    let start = Instant::now();
    for view in &views {
        host.close(&view.target(), ClosePolicy::Save).await.unwrap();
    }
    let close_ms = millis(start);
    views.clear();
    let start = Instant::now();
    for id in identities {
        views.push(host.open(id).await.unwrap());
    }
    let reopen_ms = millis(start);
    let start = Instant::now();
    for view in &views {
        complete(view, "T2").await;
    }
    let t2_run_save_ms = millis(start);
    let start = Instant::now();
    for index in 0..3 {
        for view in &views {
            let snapshot = view.read_materials().unwrap();
            let mut blocks = snapshot.context.blocks().to_vec();
            blocks.push(super::text_input(format!("edit-{index}")));
            view.edit(snapshot.version, blocks).unwrap();
            view.wait_saved().await.unwrap();
        }
    }
    let consecutive_edit_save_ms = millis(start);
    let final_materials = views[0].read_materials().unwrap();
    let final_blocks = final_materials.context.blocks().len();
    let final_material_bytes = serde_json::to_vec(&final_materials.context).unwrap().len();
    drop(final_materials);
    for view in &views {
        host.close(&view.target(), ClosePolicy::Save).await.unwrap();
    }
    let metrics = host.inner.store.as_ref().unwrap().metrics();
    let disk_bytes: u64 = std::fs::read_dir(directory.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.metadata().unwrap().len())
        .sum();
    println!(
        "TARS_MEASUREMENT {}",
        serde_json::json!({
            "case": case, "profile": "debug", "initial_blocks": block_count,
            "text_bytes_per_block": text_size, "initial_material_bytes": material_bytes,
            "final_blocks": final_blocks, "final_material_bytes": final_material_bytes,
            "simultaneous_sessions": concurrent, "create_ms": create_ms,
            "t1_run_save_ms": t1_run_save_ms, "list_ms": list_ms,
            "close_ms": close_ms, "reopen_ms": reopen_ms,
            "t2_run_save_ms": t2_run_save_ms, "consecutive_edit_save_ms": consecutive_edit_save_ms,
            "model_calls": calls.load(Ordering::Relaxed), "assemblies": assemblies.load(Ordering::Relaxed),
            "host_snapshot_clones": host.inner.snapshot_clones.load(Ordering::Relaxed),
            "store_snapshot_clones": metrics.snapshot_clones,
            "store_serializations": metrics.serialization_count, "store_writes": metrics.writes,
            "store_bytes_written": metrics.bytes_written, "final_disk_bytes": disk_bytes,
            "consumer_context_clones": concurrent, "consumer_block_vector_clones": concurrent * 3,
            "host_capacity": HostOptions::default().max_active_sessions,
            "storage_operation_capacity": HostOptions::default().max_storage_operations,
            "record_byte_capacity": 64 * 1024 * 1024,
        })
    );
    assert_eq!(calls.load(Ordering::Relaxed), (concurrent * 2) as u64);
    assert_eq!(metrics.writes, (concurrent * 6) as u64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_capacity_refuses_before_assembly_or_store_side_effects() {
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let calls = Arc::new(AtomicU64::new(0));
    let assemblies = Arc::new(AtomicU64::new(0));
    fixture(&host, calls.clone(), assemblies.clone());
    let mut views = Vec::new();
    for _ in 0..HostOptions::default().max_active_sessions {
        views.push(
            host.create(PreparedCreate::persistent(definition(), Context::default()))
                .await
                .unwrap(),
        );
    }
    let before = host.inner.store.as_ref().unwrap().metrics();
    let candidate = PreparedCreate::persistent(definition(), Context::default());
    let id = candidate.conversation_id().clone();
    assert!(matches!(
        host.create(candidate).await,
        Err(super::ApplicationError::Capacity { capacity: 32 })
    ));
    assert!(host.lookup(&id).is_none());
    assert!(
        !directory
            .path()
            .join("locks")
            .join(format!("{}.lock", id.as_str()))
            .exists()
    );
    assert!(
        crate::store::SessionStore::new(directory.path().into())
            .read_saved(id.as_str())
            .unwrap()
            .is_none()
    );
    assert_eq!(assemblies.load(Ordering::Relaxed), 32);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let after = host.inner.store.as_ref().unwrap().metrics();
    assert_eq!(after.writes, before.writes);
    assert_eq!(after.serialization_count, before.serialization_count);
    assert_eq!(after.bytes_written, before.bytes_written);
    for view in views {
        host.close(&view.target(), ClosePolicy::Save).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_capacity_bounds_actual_blocking_operations_to_four() {
    let host = SessionHost::new(None);
    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let running = Arc::new(AtomicU64::new(0));
    let maximum = Arc::new(AtomicU64::new(0));
    let (started, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut jobs = Vec::new();
    for _ in 0..8 {
        let inner = host.inner.clone();
        let gate = gate.clone();
        let running = running.clone();
        let maximum = maximum.clone();
        let started = started.clone();
        jobs.push(tokio::spawn(async move {
            super::saving::io(&inner, move || {
                let active = running.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(active, Ordering::SeqCst);
                started.send(()).unwrap();
                let (lock, changed) = &*gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                running.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        }));
    }
    for _ in 0..4 {
        tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(running.load(Ordering::SeqCst), 4);
    assert_eq!(host.inner.io.available_permits(), 0);
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    {
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_all();
    }
    for job in jobs {
        job.await.unwrap();
    }
    assert_eq!(maximum.load(Ordering::SeqCst), 4);
    assert_eq!(running.load(Ordering::SeqCst), 0);
    assert_eq!(host.inner.io.available_permits(), 4);
}

struct ControlledChildGateway {
    calls: Arc<AtomicU64>,
    hold: std::sync::Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
    cancel_entered: Option<Arc<Notify>>,
    wait_for_cancel: bool,
    response: String,
}

impl ControlledChildGateway {
    fn instant(calls: Arc<AtomicU64>, response: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls,
            hold: std::sync::Mutex::new(None),
            cancel_entered: None,
            wait_for_cancel: false,
            response: response.into(),
        })
    }

    fn held(
        calls: Arc<AtomicU64>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        response: impl Into<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls,
            hold: std::sync::Mutex::new(Some((entered, release))),
            cancel_entered: None,
            wait_for_cancel: false,
            response: response.into(),
        })
    }

    fn cancellable(calls: Arc<AtomicU64>, entered: Arc<Notify>) -> Arc<Self> {
        Arc::new(Self {
            calls,
            hold: std::sync::Mutex::new(None),
            cancel_entered: Some(entered),
            wait_for_cancel: true,
            response: String::new(),
        })
    }
}

#[async_trait]
impl ModelGateway for ControlledChildGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(entered) = &self.cancel_entered {
            entered.notify_one();
        }
        if self.wait_for_cancel {
            control.cancellation_token().cancelled().await;
            return Err(ModelInvokeError::new(
                ModelInvokeErrorKind::Cancelled,
                "cancelled",
            ));
        }
        let hold = self.hold.lock().unwrap().take();
        if let Some((entered, release)) = hold {
            entered.notify_one();
            tokio::select! {
                _ = release.notified() => {}
                _ = control.cancellation_token().cancelled() => {
                    return Err(ModelInvokeError::new(
                        ModelInvokeErrorKind::Cancelled,
                        "cancelled",
                    ));
                }
            }
        }
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(self.response.clone())],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

fn spawn_request(
    caller_agent_id: &str,
    name: &str,
    prompt: &str,
    interaction: AgentInteraction,
    execution_mode: OperationExecutionMode,
) -> SpawnAgentRequest {
    SpawnAgentRequest {
        caller_agent_id: caller_agent_id.to_owned(),
        caller_operation_id: None,
        source_block_id: None,
        name: name.to_owned(),
        preset_name: Some("general".to_owned()),
        role_prompt: Some(format!("role for {name}")),
        prompt: prompt.to_owned(),
        narrowing: ChildNarrowingRequest::default(),
        interaction,
        execution_mode,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: None,
        max_rounds: None,
        coalesce_key: None,
    }
}

fn message_request(
    caller_agent_id: &str,
    target_agent_id: &str,
    prompt: &str,
    execution_mode: OperationExecutionMode,
) -> MessageAgentRequest {
    MessageAgentRequest {
        caller_agent_id: caller_agent_id.to_owned(),
        caller_operation_id: None,
        source_block_id: None,
        target_agent_id: target_agent_id.to_owned(),
        prompt: prompt.to_owned(),
        execution_mode,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: None,
        max_rounds: None,
        coalesce_key: None,
    }
}

fn sample_rss_kib() -> Option<u64> {
    #[cfg(unix)]
    {
        let pid = std::process::id().to_string();
        let output = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &pid])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn file_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

async fn wait_weak_released(
    entry_weak: &Weak<super::Entry>,
    coord_weak: &Weak<super::coordinator::SessionCoordinator>,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while (entry_weak.strong_count() > 0 || coord_weak.strong_count() > 0)
        && Instant::now() < deadline
    {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_agent_quotas_release_and_no_arc_cycles_after_close() {
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let calls = Arc::new(AtomicU64::new(0));

    let a_entered = Arc::new(Notify::new());
    let a_release = Arc::new(Notify::new());
    let b_entered = Arc::new(Notify::new());
    let b2_entered = Arc::new(Notify::new());
    let b2_release = Arc::new(Notify::new());
    let d1_entered = Arc::new(Notify::new());
    let d1_release = Arc::new(Notify::new());
    let d2_entered = Arc::new(Notify::new());
    let d2_release = Arc::new(Notify::new());

    let gw_main = Arc::new(ProbeGateway(calls.clone()));
    let gw_a = ControlledChildGateway::held(
        calls.clone(),
        a_entered.clone(),
        a_release.clone(),
        "a-done",
    );
    let gw_b = ControlledChildGateway::cancellable(calls.clone(), b_entered.clone());
    let gw_b2 = ControlledChildGateway::held(
        calls.clone(),
        b2_entered.clone(),
        b2_release.clone(),
        "b2-done",
    );
    let gw_d1 = ControlledChildGateway::held(
        calls.clone(),
        d1_entered.clone(),
        d1_release.clone(),
        "d1-done",
    );
    let gw_d2 = ControlledChildGateway::held(
        calls.clone(),
        d2_entered.clone(),
        d2_release.clone(),
        "d2-done",
    );
    let gw_fast = ControlledChildGateway::instant(calls.clone(), "fast-done");

    host.set_test_gateway(Some(gw_main));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "child-a" => Some(gw_a.clone()),
        "child-b" => Some(gw_b.clone()),
        "child-b2" => Some(gw_b2.clone()),
        "grandchild-1" => Some(gw_d1.clone()),
        "grandchild-2" => Some(gw_d2.clone()),
        _ => Some(gw_fast.clone()),
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::default()))
        .await
        .unwrap();
    // Keep default concurrency quotas (2 per agent, 4 per session) while enabling depth=2
    // so we can exhaust both the per-agent limit and the session-wide limit deterministically.
    session.set_coordinator_config(CoordinatorConfig {
        max_delegation_depth: 2,
        ..CoordinatorConfig::default()
    });
    let root_id = session.target().conversation_id().as_str().to_owned();

    // Sync child execution + Reusable follow-up.
    let mut sync_req = spawn_request(
        &root_id,
        "child-sync",
        "sync initial",
        AgentInteraction::Reusable,
        OperationExecutionMode::Sync,
    );
    sync_req.source_block_id = Some("blk-sync-1".into());
    let SpawnAgentOutcome::Completed(sync_out) = session.spawn_agent(sync_req).await.unwrap()
    else {
        panic!("expected sync completion");
    };
    assert_eq!(sync_out.status, OperationStatus::Completed);

    let mut reuse_req = message_request(
        &root_id,
        &sync_out.agent_id,
        "follow-up reuse",
        OperationExecutionMode::Sync,
    );
    reuse_req.source_block_id = Some("blk-reuse-1".into());
    let SpawnAgentOutcome::Completed(reuse_out) = session.message_agent(reuse_req).await.unwrap()
    else {
        panic!("expected sync reuse completion");
    };
    assert_eq!(reuse_out.status, OperationStatus::Completed);

    // Fill per-agent limit (2) from root with two background ops.
    let mut req_a = spawn_request(
        &root_id,
        "child-a",
        "hold a",
        AgentInteraction::Reusable,
        OperationExecutionMode::Background,
    );
    req_a.source_block_id = Some("blk-a".into());
    let SpawnAgentOutcome::Background(rec_a) = session.spawn_agent(req_a).await.unwrap() else {
        panic!("expected background a");
    };
    a_entered.notified().await;

    let mut req_b = spawn_request(
        &root_id,
        "child-b",
        "cancel me",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Background,
    );
    req_b.source_block_id = Some("blk-b".into());
    let SpawnAgentOutcome::Background(rec_b) = session.spawn_agent(req_b).await.unwrap() else {
        panic!("expected background b");
    };
    b_entered.notified().await;

    let mut req_rejected = spawn_request(
        &root_id,
        "child-rejected",
        "over agent limit",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Background,
    );
    req_rejected.source_block_id = Some("blk-agent-limit".into());
    assert!(matches!(
        session.spawn_agent(req_rejected).await,
        Err(CoordinatorError::AgentConcurrencyLimit { limit: 2 })
    ));

    // Cancel child-b -> per-agent quota returns immediately and root can admit child-b2.
    let cancelled = session.cancel_agent(&rec_b.operation_id).await.unwrap();
    assert_eq!(cancelled.status, OperationStatus::Cancelled);

    let mut req_b2 = spawn_request(
        &root_id,
        "child-b2",
        "hold b2",
        AgentInteraction::Reusable,
        OperationExecutionMode::Background,
    );
    req_b2.source_block_id = Some("blk-b2".into());
    let SpawnAgentOutcome::Background(rec_b2) = session.spawn_agent(req_b2).await.unwrap() else {
        panic!("expected background b2");
    };
    b2_entered.notified().await;

    // Fill remaining 2 session slots via child-a (depth 1 -> depth 2), reaching 4/4 session limit.
    let mut req_d1 = spawn_request(
        &rec_a.agent_id,
        "grandchild-1",
        "d1",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Background,
    );
    req_d1.caller_operation_id = Some(rec_a.operation_id.clone());
    req_d1.source_block_id = Some("blk-d1".into());
    let SpawnAgentOutcome::Background(rec_d1) = session.spawn_agent(req_d1).await.unwrap() else {
        panic!("expected background d1");
    };
    d1_entered.notified().await;

    let mut req_d2 = spawn_request(
        &rec_a.agent_id,
        "grandchild-2",
        "d2",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Background,
    );
    req_d2.caller_operation_id = Some(rec_a.operation_id.clone());
    req_d2.source_block_id = Some("blk-d2".into());
    let SpawnAgentOutcome::Background(_rec_d2) = session.spawn_agent(req_d2).await.unwrap() else {
        panic!("expected background d2");
    };
    d2_entered.notified().await;

    // child-b2 has 0 active delegated ops of its own, so hitting 4 total session ops triggers SessionConcurrencyLimit.
    let mut req_sess_limit = spawn_request(
        &rec_b2.agent_id,
        "grandchild-3",
        "over session limit",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Sync,
    );
    req_sess_limit.caller_operation_id = Some(rec_b2.operation_id.clone());
    req_sess_limit.source_block_id = Some("blk-sess-limit".into());
    assert!(matches!(
        session.spawn_agent(req_sess_limit).await,
        Err(CoordinatorError::SessionConcurrencyLimit { limit: 4 })
    ));

    // Complete grandchild-1 -> session quota is immediately released and child-b2 can delegate.
    d1_release.notify_one();
    super::tests::support::until(|| {
        session
            .agent_snapshot(&rec_d1.agent_id)
            .is_some_and(|snap| snap.active_operation_id.is_none())
    })
    .await;

    let mut req_d3 = spawn_request(
        &rec_b2.agent_id,
        "grandchild-3",
        "after quota release",
        AgentInteraction::SingleWork,
        OperationExecutionMode::Sync,
    );
    req_d3.caller_operation_id = Some(rec_b2.operation_id.clone());
    req_d3.source_block_id = Some("blk-d3".into());
    let SpawnAgentOutcome::Completed(out_d3) = session.spawn_agent(req_d3).await.unwrap() else {
        panic!("expected sync grandchild-3 completion after quota release");
    };
    assert_eq!(out_d3.status, OperationStatus::Completed);

    // Release remaining held operations and let background + root wakeup settle.
    d2_release.notify_one();
    a_release.notify_one();
    b2_release.notify_one();
    session.wait_background_tasks_settled().await;
    super::tests::support::until(|| !session.has_active_execution()).await;
    session.wait_saved().await.unwrap();

    // Verify SQLite WAL & lock-free read-only queries while the session is open.
    let db_path = directory.path().join("tars.db");
    assert!(db_path.exists());
    let target = session.target();
    let conv_id = target.conversation_id().clone();
    let inspected_open = host.inspect(&conv_id).await.unwrap();
    assert_eq!(inspected_open.agents.len(), 8);
    let agent_inspected_open = host
        .inspect_agent(&conv_id, &sync_out.agent_id)
        .await
        .unwrap();
    assert_eq!(agent_inspected_open.operations.len(), 2);
    let _history_open = host
        .read_saved_history(&conv_id, Some(&sync_out.agent_id), None)
        .await
        .unwrap();

    // Capture Weak handles to Entry and SessionCoordinator before releasing ManagedSession and closing.
    let entry_weak = Arc::downgrade(&session.entry);
    let coord_weak = Arc::downgrade(
        session
            .entry
            .coordinator
            .get()
            .expect("coordinator attached"),
    );
    assert!(entry_weak.strong_count() >= 1);
    assert!(coord_weak.strong_count() >= 1);

    drop(session);
    let close_outcome = host.close(&target, ClosePolicy::Save).await.unwrap();
    assert!(matches!(close_outcome.save, SaveStatus::Saved { .. }));

    wait_weak_released(&entry_weak, &coord_weak).await;
    assert_eq!(
        entry_weak.strong_count(),
        0,
        "Entry must not have Arc cycles after close"
    );
    assert_eq!(
        coord_weak.strong_count(),
        0,
        "SessionCoordinator must not have Arc cycles after close"
    );

    // Post-close lock-free WAL read-only queries remain healthy.
    let inspected_closed = host.inspect(&conv_id).await.unwrap();
    assert_eq!(inspected_closed.agents.len(), 8);
    let listed = host.list().await.unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "multi-agent resource measurement"]
async fn multi_agent_resource_probe() {
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let calls = Arc::new(AtomicU64::new(0));

    host.set_test_gateway(Some(Arc::new(ProbeGateway(calls.clone()))));
    let archived_reply = "archive-payload:".to_owned() + &"z".repeat(1024);
    host.set_test_agent_gateway(Some(Arc::new(move |_, _| {
        Some(ControlledChildGateway::instant(
            calls.clone(),
            archived_reply.clone(),
        ))
    })));

    // Root carries 64 blocks x 1 KiB.
    let root_context = Context::from_blocks(
        (0..64)
            .map(|i| super::text_input(format!("root-block-{i:02}: {}", "r".repeat(1024))))
            .collect(),
    )
    .unwrap();
    let session = host
        .create(PreparedCreate::persistent(definition(), root_context))
        .await
        .unwrap();
    session.wait_saved().await.unwrap();

    session.set_coordinator_config(CoordinatorConfig {
        max_active_ops_per_agent: 4,
        max_active_child_ops_per_session: 4,
        ..CoordinatorConfig::default()
    });

    let db_path = directory.path().join("tars.db");
    let wal_path = directory.path().join("tars.db-wal");
    let db_bytes_initial = file_len(&db_path);
    let root_id = session.target().conversation_id().as_str().to_owned();

    let mut admit_outbox_ms = Vec::new();

    // Round 1: Spawn 4 concurrent Reusable child agents in Background mode.
    let mut bg_handles = Vec::new();
    for idx in 0..4 {
        let s = session.clone();
        let rid = root_id.clone();
        bg_handles.push(tokio::spawn(async move {
            let mut req = spawn_request(
                &rid,
                &format!("worker-{idx}"),
                &format!("initial task {idx}"),
                AgentInteraction::Reusable,
                OperationExecutionMode::Background,
            );
            req.source_block_id = Some(format!("probe-spawn-{idx}"));
            let started = Instant::now();
            let outcome = s.spawn_agent(req).await.unwrap();
            let elapsed = millis(started);
            (outcome, elapsed)
        }));
    }
    let mut agent_ids = Vec::new();
    let mut first_ops = Vec::new();
    for handle in bg_handles {
        let (outcome, _admit_ms) = handle.await.unwrap();
        let SpawnAgentOutcome::Background(receipt) = outcome else {
            panic!("expected background spawn");
        };
        agent_ids.push(receipt.agent_id);
        first_ops.push(receipt.operation_id);
    }
    session.wait_background_tasks_settled().await;
    super::tests::support::until(|| !session.has_active_execution()).await;
    session.wait_saved().await.unwrap();

    // Populate each of the 4 child agents with 64 blocks x 1 KiB of real Context material.
    for (idx, agent_id) in agent_ids.iter().enumerate() {
        let snap = session.read_agent_materials(agent_id).unwrap();
        let mut blocks = snap.context.blocks().to_vec();
        for b in 0..64 {
            blocks.push(super::text_input(format!(
                "child-{idx}-block-{b:02}: {}",
                "c".repeat(1024)
            )));
        }
        session.edit_agent_materials(snap.version, blocks).unwrap();
    }
    session.wait_saved().await.unwrap();

    // Round 2: Concurrent Sync message_agent rounds across all 4 child agents (measures admit + outbox).
    for round in 0..2 {
        let mut sync_handles = Vec::new();
        for (idx, agent_id) in agent_ids.iter().enumerate() {
            let s = session.clone();
            let rid = root_id.clone();
            let tid = agent_id.clone();
            sync_handles.push(tokio::spawn(async move {
                let mut req = message_request(
                    &rid,
                    &tid,
                    &format!("sync-round-{round}-{idx}"),
                    OperationExecutionMode::Sync,
                );
                req.source_block_id = Some(format!("probe-msg-{round}-{idx}"));
                let started = Instant::now();
                let outcome = s.message_agent(req).await.unwrap();
                let elapsed = millis(started);
                (outcome, elapsed)
            }));
        }
        for handle in sync_handles {
            let (outcome, elapsed) = handle.await.unwrap();
            let SpawnAgentOutcome::Completed(out) = outcome else {
                panic!("expected sync completion");
            };
            assert_eq!(out.status, OperationStatus::Completed);
            admit_outbox_ms.push(elapsed);
            let page = session.read_agent(&out.operation_id, 0, 512).await.unwrap();
            assert!(!page.page.content.is_empty());
        }
    }

    // Verify archived output from initial background operations is readable too.
    for op_id in &first_ops {
        let page = session.read_agent(op_id, 0, 512).await.unwrap();
        assert_eq!(page.status, OperationStatus::Completed);
        assert!(page.page.total_bytes > 512);
    }

    // Total serialized Context bytes across root + 4 child agents.
    let mut total_context_bytes = serde_json::to_vec(&session.read_materials().unwrap().context)
        .unwrap()
        .len();
    for agent_id in &agent_ids {
        let snap = session.read_agent_materials(agent_id).unwrap();
        assert!(snap.context.blocks().len() >= 64);
        total_context_bytes += serde_json::to_vec(&snap.context).unwrap().len();
    }

    let db_bytes_after_agents = file_len(&db_path);
    let wal_bytes_after_agents = file_len(&wal_path);
    let rss_kib = sample_rss_kib();

    // Measure lock-free WAL read-only inspect / inspect_agent / read_saved_history.
    let target = session.target();
    let conv_id = target.conversation_id().clone();
    let mut wal_inspect_ms = Vec::new();
    for agent_id in &agent_ids {
        let t0 = Instant::now();
        let summary = host.inspect(&conv_id).await.unwrap();
        assert_eq!(summary.agents.len(), 5);
        let agent_info = host.inspect_agent(&conv_id, agent_id).await.unwrap();
        assert_eq!(agent_info.operations.len(), 3);
        let _hist = host
            .read_saved_history(&conv_id, Some(agent_id), Some(16))
            .await
            .unwrap();
        wal_inspect_ms.push(millis(t0));
    }

    let admit_and_outbox_avg_ms =
        admit_outbox_ms.iter().sum::<f64>() / admit_outbox_ms.len() as f64;
    let admit_and_outbox_max_ms = admit_outbox_ms.iter().copied().fold(0.0_f64, f64::max);
    let wal_readonly_inspect_avg_ms =
        wal_inspect_ms.iter().sum::<f64>() / wal_inspect_ms.len() as f64;
    let wal_readonly_inspect_max_ms = wal_inspect_ms.iter().copied().fold(0.0_f64, f64::max);

    let entry_weak = Arc::downgrade(&session.entry);
    let coord_weak = Arc::downgrade(
        session
            .entry
            .coordinator
            .get()
            .expect("coordinator attached"),
    );

    drop(session);
    let close_outcome = host.close(&target, ClosePolicy::Save).await.unwrap();
    assert!(matches!(close_outcome.save, SaveStatus::Saved { .. }));
    wait_weak_released(&entry_weak, &coord_weak).await;

    let entry_strong_count_after_close = entry_weak.strong_count();
    let coord_strong_count_after_close = coord_weak.strong_count();
    assert_eq!(entry_strong_count_after_close, 0);
    assert_eq!(coord_strong_count_after_close, 0);

    println!(
        "TARS_MULTI_AGENT_MEASUREMENT {}",
        serde_json::json!({
            "agents": 5,
            "blocks_per_agent_min": 64,
            "total_context_bytes": total_context_bytes,
            "rss_kib": rss_kib,
            "admit_and_outbox_avg_ms": admit_and_outbox_avg_ms,
            "admit_and_outbox_max_ms": admit_and_outbox_max_ms,
            "wal_readonly_inspect_avg_ms": wal_readonly_inspect_avg_ms,
            "wal_readonly_inspect_max_ms": wal_readonly_inspect_max_ms,
            "db_bytes_initial": db_bytes_initial,
            "db_bytes_after_agents": db_bytes_after_agents,
            "wal_bytes_after_agents": wal_bytes_after_agents,
            "entry_strong_count_after_close": entry_strong_count_after_close,
            "coord_strong_count_after_close": coord_strong_count_after_close,
        })
    );
}
