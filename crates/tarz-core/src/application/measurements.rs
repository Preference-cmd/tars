//! Opt-in resource probe of actual managed host and JSON store operations.

use super::{ClosePolicy, HostOptions, PreparedCreate, SessionHost};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::session::{Session, SubmitRequest, WorkResult, WorkState};
use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelItem, ModelOutput, ModelRequest,
    ModelResponse, ModelStopReason,
};
use causa::runtime::{ToolExecutor, TurnResult, TurnRunner};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

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
        credential: CredentialRef::Environment("TARZ_PROBE_KEY_REF".into()),
        work_timeout_secs: Some(60),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
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
    let case = std::env::var("TARZ_MEASURE_CASE").unwrap_or_else(|_| "small".into());
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
    let disk_bytes: u64 = std::fs::read_dir(directory.path().join("sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    println!(
        "TARZ_MEASUREMENT {}",
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
            .join("sessions")
            .join(format!("{}.json", id.as_str()))
            .exists()
    );
    assert!(
        !directory
            .path()
            .join("sessions")
            .join(format!("{}.lock", id.as_str()))
            .exists()
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
