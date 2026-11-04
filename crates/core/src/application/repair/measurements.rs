//! Opt-in isolated measurements; no network or model invocation is performed.
use super::{RepairMode, inspect};
use crate::application::{ClosePolicy, PreparedCreate, SaveStatus, SessionHost, text_input};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
use std::{sync::atomic::Ordering, time::Instant};

fn elapsed(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "resource probe; run each TARS_REPAIR_PROBE case in an isolated process"]
async fn repair_resource_probe() {
    let case = std::env::var("TARS_REPAIR_PROBE").unwrap_or_else(|_| "small".into());
    let (count, size) = match case.as_str() {
        "small" => (32, 128),
        "medium" => (1024, 1024),
        "large" => (8192, 1024),
        _ => panic!("unknown repair probe"),
    };
    let mut blocks: Vec<_> = (0..count).map(|_| text_input("x".repeat(size))).collect();
    blocks.extend((0..8).map(|index| {
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolCall(ToolCallPayload::new(
                "read_file",
                serde_json::json!({"path":format!("file-{index}")}),
            )),
            BlockMeta::default().with_provider_call_id(format!("probe-{index}")),
        )
    }));
    let context = Context::from_blocks(blocks).unwrap();
    let material_bytes = serde_json::to_vec(&context).unwrap().len();
    let start = Instant::now();
    let scan = inspect::scan(&context, &[]);
    let scan_ms = elapsed(start);
    assert_eq!(scan.unpaired.len(), 8);
    let start = Instant::now();
    let copy = context.clone();
    let copy_ms = elapsed(start);
    assert_eq!(copy.blocks(), context.blocks());
    drop(copy);
    let definition = SessionDefinition {
        provider: Provider::Anthropic,
        model: "local-resource-probe".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 1,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
    };
    let directory = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(directory.path().into()));
    let session = host
        .create(PreparedCreate::persistent(definition.clone(), context))
        .await
        .unwrap();
    let snapshots_before = host.inner.snapshot_clones.load(Ordering::Relaxed);
    let start = Instant::now();
    let inspection = session.inspect_repair(None).unwrap();
    let inspect_ms = elapsed(start);
    assert_eq!(inspection.diagnosis.unpaired_count, 8);
    let start = Instant::now();
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let plan_ms = elapsed(start);
    let candidate_bytes = serde_json::to_vec(plan.candidate()).unwrap().len();
    let start = Instant::now();
    inspect::render(plan.candidate(), &definition).unwrap();
    let render_ms = elapsed(start);
    let start = Instant::now();
    session.apply_repair(&plan).unwrap();
    let apply_ms = elapsed(start);
    let start = Instant::now();
    assert!(matches!(
        session.wait_saved().await.unwrap(),
        SaveStatus::Saved { revision: 2, .. }
    ));
    let save_ms = elapsed(start);
    let snapshots_after = host.inner.snapshot_clones.load(Ordering::Relaxed);
    let start = Instant::now();
    drop(plan);
    let plan_drop_ms = elapsed(start);
    assert!(session.status().active_work.is_none());
    println!(
        "{}",
        serde_json::json!({
            "case":case,"material_blocks":count+8,"material_json_bytes":material_bytes,
            "candidate_json_bytes":candidate_bytes,"scan_ms":scan_ms,"copy_ms":copy_ms,
            "inspect_ms":inspect_ms,"plan_ms":plan_ms,"render_ms":render_ms,
            "apply_ms":apply_ms,"save_wait_ms":save_ms,"plan_drop_ms":plan_drop_ms,
            "counted_host_snapshot_copies":snapshots_after-snapshots_before,
            "work_started":false,"saved_revision":2,
            "notes":"Snapshot counter excludes protocol frame copies, original prepared input and store baselines; JSON byte counts are not memory budgets. Plan drop does not imply allocator pages returned to the OS."
        })
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}
