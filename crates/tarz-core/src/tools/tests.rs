use super::execute::TestGate;
use super::{ToolResources, WorkspaceTools};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits};
use crate::session::{ToolPhase, ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultStatus, TurnId,
};
use causa::runtime::RunEvent;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn call(name: &str, arguments: Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new(name, arguments),
    )
}
fn reporter(calls: &[ToolCallContext]) -> ToolReporter {
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "test".into(),
            turn_id: turn_id.clone(),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    let batch = ToolBatch::new(calls.to_vec()).unwrap();
    reporter.observe(&RunEvent::ToolBatchReady {
        invocation_id: &InvocationId {
            turn_id,
            round_id: RoundId(0),
        },
        batch: &batch,
    });
    reporter
}
fn tools(limits: WorkspaceToolLimits) -> (tempfile::TempDir, Arc<WorkspaceTools>) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    (
        dir,
        WorkspaceTools::new(workspace, limits, ToolResources::new(16)),
    )
}
fn control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}
async fn invoke(
    shared: &Arc<WorkspaceTools>,
    kind: WorkspaceTool,
    arguments: Value,
) -> causa::kernel::ToolResultPayload {
    let call = call(kind.name(), arguments);
    let reporter = reporter(std::slice::from_ref(&call));
    let tool = shared.bind(&[kind], reporter.clone()).pop().unwrap();
    let result = tool.execute(&call, &control(), None).await;
    reporter.drain().await;
    result
}

#[tokio::test]
async fn real_directory_read_and_search_report_paths_and_lines() {
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/data"),
        "first\n恢复: retry_save\nlast\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("empty"), "").unwrap();
    let listed = invoke(&shared, WorkspaceTool::ListDirectory, json!({"path":"."})).await;
    assert_eq!(listed.status, ToolResultStatus::Succeeded);
    assert_eq!(
        listed.output.content["entries"].as_array().unwrap().len(),
        2
    );
    let read = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path":"src/data","start_line":2,"max_lines":1}),
    )
    .await;
    assert_eq!(read.output.content["text"], "恢复: retry_save\n");
    assert_eq!(read.output.content["end_line"], 2);
    assert_eq!(read.output.content["limit_reason"], "max_lines");
    let search = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"retry_save"}),
    )
    .await;
    assert_eq!(search.output.content["complete"], true);
    assert_eq!(search.output.content["matches"][0]["path"], "src/data");
    assert_eq!(search.output.content["matches"][0]["line"], 2);
    let empty = invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"empty"})).await;
    assert_eq!(empty.output.content["text"], "");
    assert_eq!(empty.output.content["end_line"], Value::Null);
    assert_eq!(shared.metrics().active, 0);
    assert_eq!(shared.metrics().started, shared.metrics().completed);
}

#[tokio::test]
async fn invalid_inputs_do_not_start_filesystem_jobs() {
    let (_dir, shared) = tools(WorkspaceToolLimits::default());
    for input in [
        json!({"path":"../escape"}),
        json!({"path":"/etc"}),
        json!({"path":".","extra":true}),
        json!({"path":1}),
        json!({"path":".","limit":0}),
        json!({"path":".","limit":201}),
        json!({"path":".","limit":null}),
        json!({"path":"x".repeat(4097)}),
    ] {
        assert_eq!(
            invoke(&shared, WorkspaceTool::ListDirectory, input)
                .await
                .status,
            ToolResultStatus::Rejected
        );
    }
    for input in [
        json!({"path":".","query":""}),
        json!({"path":".","query":"first\nsecond"}),
        json!({"path":".","query":"first\rsecond"}),
        json!({"path":".","query":"x".repeat(1025)}),
        json!({"path":".","query":"x","max_matches":201}),
    ] {
        assert_eq!(
            invoke(&shared, WorkspaceTool::SearchText, input)
                .await
                .status,
            ToolResultStatus::Rejected
        );
    }
    assert_eq!(shared.metrics().started, 0);
}

#[tokio::test]
async fn links_binary_missing_files_and_no_matches_are_distinct() {
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::write(dir.path().join("data"), "safe").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
    assert_eq!(
        invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"link"}))
            .await
            .status,
        ToolResultStatus::Rejected
    );
    assert_eq!(
        invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"missing"}))
            .await
            .status,
        ToolResultStatus::Failed
    );
    for bytes in [vec![0, 65], vec![255]] {
        std::fs::write(dir.path().join("binary"), bytes).unwrap();
        assert_eq!(
            invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"binary"}))
                .await
                .status,
            ToolResultStatus::Failed
        );
    }
    let search = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"not here"}),
    )
    .await;
    assert_eq!(search.output.content["complete"], false);
    assert!(search.output.content["skipped"].as_u64().unwrap() >= 2);
    std::fs::remove_file(dir.path().join("binary")).unwrap();
    std::fs::remove_file(dir.path().join("link")).unwrap();
    let complete = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"not here"}),
    )
    .await;
    assert_eq!(complete.output.content["complete"], true);
    assert!(
        complete.output.content["matches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn budgets_include_json_escaping_skipped_lines_and_partial_searches() {
    let limits = WorkspaceToolLimits {
        max_output_bytes: 2048,
        max_file_scan_bytes: 64,
        max_search_scan_bytes: 80,
        max_directory_entries: 2,
        max_search_matches: 2,
        ..Default::default()
    };
    let (dir, shared) = tools(limits);
    for n in 0..5 {
        std::fs::write(dir.path().join(format!("f{n}")), "\"\t\\\n".repeat(1000)).unwrap();
    }
    let list = invoke(&shared, WorkspaceTool::ListDirectory, json!({"path":"."})).await;
    assert_eq!(list.output.content["entries"].as_array().unwrap().len(), 2);
    assert_eq!(list.output.content["truncated"], true);
    let read = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path":"f0","start_line":1000}),
    )
    .await;
    assert_eq!(read.output.content["scanned_bytes"], 64);
    assert_eq!(read.output.content["text"], "");
    assert_eq!(read.output.content["truncated"], true);
    let search = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"no match"}),
    )
    .await;
    assert_eq!(search.output.content["complete"], false);
    assert!(search.output.content["scanned_bytes"].as_u64().unwrap() <= 80);
    assert!(serde_json::to_vec(&search).unwrap().len() <= 2048);
    let long = "\"".repeat(100_000);
    std::fs::write(dir.path().join("long"), long).unwrap();
    let read = invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"long"})).await;
    assert!(serde_json::to_vec(&read).unwrap().len() <= 2048);
}

#[tokio::test]
async fn waiting_cancellation_and_deadline_never_start_operations() {
    let (_dir, shared) = tools(WorkspaceToolLimits {
        max_concurrent_operations: 1,
        ..Default::default()
    });
    let permit = shared.local.clone().acquire_owned().await.unwrap();
    let call = call("list_directory", json!({"path":"."}));
    let reporter = reporter(std::slice::from_ref(&call));
    let tool = shared
        .bind(&[WorkspaceTool::ListDirectory], reporter.clone())
        .pop()
        .unwrap();
    let cancellation = CancellationToken::new();
    let control = CallControl::new(cancellation.clone(), None);
    let task = tokio::spawn({
        let call = call.clone();
        let tool = tool.clone();
        async move { tool.execute(&call, &control, None).await }
    });
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert_eq!(task.await.unwrap().status, ToolResultStatus::Cancelled);
    assert_eq!(shared.metrics().started, 0);
    let deadline = CallControl::new(CancellationToken::new(), Some(Instant::now()));
    assert_eq!(
        tool.execute(&call, &deadline, None).await.status,
        ToolResultStatus::TimedOut
    );
    assert_eq!(shared.metrics().started, 0);
    reporter.drain().await;
    drop(permit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_future_retains_permits_until_actual_tracked_worker_cleanup() {
    let (dir, shared) = tools(WorkspaceToolLimits {
        max_concurrent_operations: 1,
        ..Default::default()
    });
    std::fs::write(dir.path().join("file"), "text").unwrap();
    let gate = TestGate::new(false);
    *shared.gate.lock().unwrap() = Some(gate.clone());
    let call = call("read_file", json!({"path":"file"}));
    let report = reporter(std::slice::from_ref(&call));
    let tool = shared
        .bind(&[WorkspaceTool::ReadFile], report.clone())
        .pop()
        .unwrap();
    let task = tokio::spawn(async move { tool.execute(&call, &control(), None).await });
    tokio::time::timeout(Duration::from_secs(3), gate.reached.notified())
        .await
        .unwrap();
    assert_eq!(report.snapshot().calls[0].phase, ToolPhase::Running);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(shared.metrics().active, 1);
    assert_eq!(shared.local.available_permits(), 0);
    let drain = tokio::spawn({
        let report = report.clone();
        async move { report.drain().await }
    });
    tokio::task::yield_now().await;
    assert!(!drain.is_finished());
    gate.release();
    tokio::time::timeout(Duration::from_secs(3), drain)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shared.metrics().active, 0);
    assert_eq!(shared.local.available_permits(), 1);
    assert_eq!(shared.metrics().scanned_bytes, 0);
    *shared.gate.lock().unwrap() = None;
    assert_eq!(
        invoke(&shared, WorkspaceTool::ReadFile, json!({"path":"file"}))
            .await
            .status,
        ToolResultStatus::Succeeded
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn returned_result_and_duplicate_declaration_identities_are_independent() {
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::write(dir.path().join("file"), "text").unwrap();
    let first = call("read_file", json!({"path":"file"}));
    let second = call("read_file", json!({"path":"file"}));
    let report = reporter(&[first.clone(), second.clone()]);
    let tool = shared
        .bind(&[WorkspaceTool::ReadFile], report.clone())
        .pop()
        .unwrap();
    let control = control();
    let (one, two) = tokio::join!(
        tool.execute(&first, &control, None),
        tool.execute(&second, &control, None)
    );
    assert_eq!(one.call_block_id, first.call_block_id);
    assert_eq!(two.call_block_id, second.call_block_id);
    assert_ne!(one.call_block_id, two.call_block_id);
    report.drain().await;
    assert_eq!(shared.metrics().started, 2);
    assert!(
        report
            .snapshot()
            .calls
            .iter()
            .all(|c| c.phase == ToolPhase::Returned)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn started_search_checks_cancellation_and_deadline_before_scanning() {
    for deadline in [false, true] {
        let (dir, shared) = tools(WorkspaceToolLimits::default());
        std::fs::write(dir.path().join("file"), "real data").unwrap();
        let gate = TestGate::new(false);
        *shared.gate.lock().unwrap() = Some(gate.clone());
        let call = call("search_text", json!({"path":".","query":"data"}));
        let report = reporter(std::slice::from_ref(&call));
        let tool = shared
            .bind(&[WorkspaceTool::SearchText], report.clone())
            .pop()
            .unwrap();
        let cancellation = CancellationToken::new();
        let end = Instant::now() + Duration::from_millis(100);
        let control = CallControl::new(cancellation.clone(), deadline.then_some(end));
        let task = tokio::spawn(async move { tool.execute(&call, &control, None).await });
        tokio::time::timeout(Duration::from_secs(3), gate.reached.notified())
            .await
            .unwrap();
        if deadline {
            tokio::time::sleep_until(end.into()).await;
        } else {
            cancellation.cancel();
        }
        assert_eq!(shared.metrics().active, 1);
        gate.release();
        let output = task.await.unwrap();
        assert_eq!(
            output.status,
            if deadline {
                ToolResultStatus::TimedOut
            } else {
                ToolResultStatus::Cancelled
            }
        );
        report.drain().await;
        assert_eq!(shared.metrics().scanned_bytes, 0);
        assert_eq!(shared.metrics().active, 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_at_return_boundary_preserves_observed_success() {
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::write(dir.path().join("file"), "result already read").unwrap();
    let gate = TestGate::new(true);
    *shared.gate.lock().unwrap() = Some(gate.clone());
    let call = call("read_file", json!({"path":"file"}));
    let report = reporter(std::slice::from_ref(&call));
    let tool = shared
        .bind(&[WorkspaceTool::ReadFile], report.clone())
        .pop()
        .unwrap();
    let cancellation = CancellationToken::new();
    let control = CallControl::new(cancellation.clone(), None);
    let task = tokio::spawn(async move { tool.execute(&call, &control, None).await });
    tokio::time::timeout(Duration::from_secs(3), gate.reached.notified())
        .await
        .unwrap();
    assert_eq!(shared.metrics().scanned_bytes, 19);
    cancellation.cancel();
    gate.release();
    let output = task.await.unwrap();
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    assert_eq!(output.output.content["text"], "result already read");
    report.drain().await;
    assert_eq!(shared.metrics().completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_host_slots_bound_actual_operations_across_instances() {
    let host = ToolResources::new(1);
    let (dir, _) = tools(WorkspaceToolLimits::default());
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let first = WorkspaceTools::new(
        workspace.clone(),
        WorkspaceToolLimits::default(),
        host.clone(),
    );
    let second = WorkspaceTools::new(workspace, WorkspaceToolLimits::default(), host.clone());
    std::fs::write(dir.path().join("file"), "data").unwrap();
    let gate = TestGate::new(false);
    *first.gate.lock().unwrap() = Some(gate.clone());
    let call = call("read_file", json!({"path":"file"}));
    let report = reporter(std::slice::from_ref(&call));
    let tool = first
        .bind(&[WorkspaceTool::ReadFile], report.clone())
        .pop()
        .unwrap();
    let task = tokio::spawn({
        let call = call.clone();
        async move { tool.execute(&call, &control(), None).await }
    });
    tokio::time::timeout(Duration::from_secs(3), gate.reached.notified())
        .await
        .unwrap();
    let report_second = reporter(std::slice::from_ref(&call));
    let tool = second
        .bind(&[WorkspaceTool::ReadFile], report_second.clone())
        .pop()
        .unwrap();
    let cancellation = CancellationToken::new();
    let control = CallControl::new(cancellation.clone(), None);
    let waiting = tokio::spawn(async move { tool.execute(&call, &control, None).await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while second.local.available_permits() == 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(report_second.snapshot().calls[0].phase, ToolPhase::Waiting);
    assert_eq!(first.metrics().active + second.metrics().active, 1);
    assert_eq!(host.slots.available_permits(), 0);
    cancellation.cancel();
    assert_eq!(waiting.await.unwrap().status, ToolResultStatus::Cancelled);
    assert_eq!(second.metrics().started, 0);
    gate.release();
    assert_eq!(task.await.unwrap().status, ToolResultStatus::Succeeded);
    report.drain().await;
    report_second.drain().await;
    assert_eq!(host.slots.available_permits(), 1);
}

#[tokio::test]
async fn depth_and_file_limits_make_no_match_searches_incomplete() {
    let limits = WorkspaceToolLimits {
        max_search_depth: 1,
        max_search_files: 1,
        ..Default::default()
    };
    let (dir, shared) = tools(limits);
    std::fs::create_dir_all(dir.path().join("one/two")).unwrap();
    std::fs::write(dir.path().join("one/two/deep"), "match").unwrap();
    let depth = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"match"}),
    )
    .await;
    assert_eq!(depth.output.content["complete"], false);
    assert_eq!(depth.output.content["issues"][0]["reason"], "search_depth");
    assert_eq!(depth.output.content["scanned_files"], 0);
    std::fs::write(dir.path().join("one/a"), "first").unwrap();
    std::fs::write(dir.path().join("one/b"), "second").unwrap();
    let files = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":"one","query":"absent"}),
    )
    .await;
    assert_eq!(files.output.content["complete"], false);
    assert_eq!(files.output.content["limit_reason"], "search_files");
    assert_eq!(files.output.content["scanned_files"], 1);
}

#[tokio::test]
async fn directory_only_trees_cannot_bypass_search_work_bounds() {
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    for index in 0..4001 {
        std::fs::create_dir(dir.path().join(format!("d{index}"))).unwrap();
    }
    let output = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"absent"}),
    )
    .await;
    assert_eq!(output.output.content["limit_reason"], "search_entries");
    assert_eq!(output.output.content["scanned_entries"], 4000);
    assert_eq!(output.output.content["scanned_files"], 0);
    assert_eq!(output.output.content["complete"], false);
}

#[tokio::test]
async fn long_line_snippets_include_the_match_and_permission_errors_remain_partial() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::write(
        dir.path().join("long"),
        format!("{}needle{}", "文".repeat(2000), "字".repeat(2000)),
    )
    .unwrap();
    let output = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"needle"}),
    )
    .await;
    let snippet = output.output.content["matches"][0]["text"]
        .as_str()
        .unwrap();
    assert!(snippet.contains("needle"));
    assert!(snippet.len() <= 262);
    assert_eq!(
        output.output.content["matches"][0]["snippet_truncated"],
        true
    );
    std::fs::create_dir(dir.path().join("denied")).unwrap();
    std::fs::write(dir.path().join("denied/private"), "hidden").unwrap();
    std::fs::set_permissions(
        dir.path().join("denied"),
        std::fs::Permissions::from_mode(0o0),
    )
    .unwrap();
    let denied = invoke(
        &shared,
        WorkspaceTool::ListDirectory,
        json!({"path":"denied"}),
    )
    .await;
    // The fixtures run as the ordinary developer account. Restore permissions
    // before assertions so failed checks never leave an unremovable fixture.
    let partial = invoke(
        &shared,
        WorkspaceTool::SearchText,
        json!({"path":".","query":"absent"}),
    )
    .await;
    std::fs::set_permissions(
        dir.path().join("denied"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert_eq!(denied.status, ToolResultStatus::Failed);
    assert_eq!(partial.output.content["complete"], false);
    assert!(
        partial.output.content["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["reason"] == "directory_access")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "resource probe: run the test binary under /usr/bin/time -l"]
async fn resource_probe() {
    fn option(name: &str, fallback: usize, cap: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(fallback)
            .min(cap)
    }
    let files = option("TARZ_TOOL_PROBE_FILES", 100, 2000);
    let bytes = option("TARZ_TOOL_PROBE_BYTES", 8192, 4 * 1024 * 1024);
    let sessions = option("TARZ_TOOL_PROBE_SESSIONS", 1, 32).max(1);
    let depth = option("TARZ_TOOL_PROBE_DEPTH", 0, 40);
    let dense = option("TARZ_TOOL_PROBE_DENSE", 0, 1) == 1;
    let dir = tempfile::tempdir().unwrap();
    let mut folder = dir.path().to_path_buf();
    for _ in 0..depth {
        folder = folder.join("d");
        std::fs::create_dir(&folder).unwrap();
    }
    let line = if dense {
        "needle\n"
    } else {
        "ordinary fixture text\n"
    };
    let mut data = line.repeat(bytes / line.len() + 1);
    data.truncate(bytes);
    for index in 0..files {
        std::fs::write(folder.join(format!("f{index}")), &data).unwrap();
    }
    drop(data);
    let host = ToolResources::new(16);
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let first_file = folder
        .join("f0")
        .strip_prefix(dir.path())
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let start = Instant::now();
    let mut jobs = Vec::new();
    for _ in 0..sessions {
        let first_file = first_file.clone();
        let shared = WorkspaceTools::new(
            workspace.clone(),
            WorkspaceToolLimits::default(),
            host.clone(),
        );
        jobs.push(tokio::spawn(async move {
            let (output, listed, read) = tokio::join!(
                invoke(
                    &shared,
                    WorkspaceTool::SearchText,
                    json!({"path":".","query":"needle"})
                ),
                invoke(&shared, WorkspaceTool::ListDirectory, json!({"path":"."})),
                invoke(&shared, WorkspaceTool::ReadFile, json!({"path":first_file}))
            );
            (
                shared.metrics(),
                [
                    serde_json::to_vec(&output).unwrap().len(),
                    serde_json::to_vec(&listed).unwrap().len(),
                    serde_json::to_vec(&read).unwrap().len(),
                ],
                output.output.content,
            )
        }));
    }
    for (index, job) in jobs.into_iter().enumerate() {
        let (metrics, size, content) = job.await.unwrap();
        println!(
            "probe_session={index} metrics={metrics:?} serialized_result_bytes={size:?} content_stats={}",
            json!({"files":content["scanned_files"],"entries":content["scanned_entries"],"bytes":content["scanned_bytes"],"complete":content["complete"],"reason":content["limit_reason"]})
        );
        assert_eq!(metrics.started, metrics.completed);
        assert_eq!(metrics.active, 0);
        assert!(metrics.peak_active <= 4);
        assert!(size.iter().all(|size| *size <= 64 * 1024));
    }
    println!(
        "probe files={files} file_bytes={bytes} depth={depth} dense={dense} sessions={sessions} elapsed_ms={}",
        start.elapsed().as_millis()
    );
    let host_metrics = host.counters.snapshot();
    println!("probe_host metrics={host_metrics:?}");
    assert!(host_metrics.peak_active <= 16);
    assert_eq!(host_metrics.active, 0);
}

#[tokio::test]
#[ignore = "compact actual tool observation sizing probe"]
async fn observation_probe() {
    use crate::session::{ProgressUpdate, ToolProgress, ToolProgressSnapshot, WorkProgress};
    let (dir, shared) = tools(WorkspaceToolLimits::default());
    std::fs::write(dir.path().join("file"), "actual file data\n").unwrap();
    let declarations = [
        (WorkspaceTool::ListDirectory, json!({"path":"."})),
        (WorkspaceTool::ReadFile, json!({"path":"file"})),
        (
            WorkspaceTool::SearchText,
            json!({"path":".","query":"界".repeat(341)}),
        ),
    ];
    let mut total_events = 0;
    let mut max_event = 0;
    for (kind, args) in declarations {
        let call = call(kind.name(), args);
        let (sender, mut receiver) = tokio::sync::broadcast::channel(128);
        let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
        let work = WorkRef {
            conversation_id: "observation-probe".into(),
            turn_id: turn_id.clone(),
        };
        let report = ToolReporter::new(work, sender);
        report.observe(&RunEvent::ToolBatchReady {
            invocation_id: &InvocationId {
                turn_id,
                round_id: RoundId(0),
            },
            batch: &ToolBatch::new(vec![call.clone()]).unwrap(),
        });
        let tool = shared.bind(&[kind], report.clone()).pop().unwrap();
        let output = tool.execute(&call, &control(), None).await;
        report.drain().await;
        let snapshot = report.snapshot();
        let mut events = 0;
        while let Ok(event) = receiver.try_recv() {
            let mut strings = event.work.conversation_id.as_str().len()
                + event.work.turn_id.0.len()
                + event.invocation_id.turn_id.0.len();
            if let ProgressUpdate::Tool(item) = &event.update {
                strings += item.tool_name.len()
                    + item.arguments_summary.len()
                    + item.invocation_id.turn_id.0.len();
            }
            let estimate =
                std::mem::size_of::<WorkProgress>() + 2 * std::mem::size_of::<usize>() + strings;
            max_event = max_event.max(estimate);
            events += 1;
        }
        let snapshot_strings: usize = snapshot
            .calls
            .iter()
            .map(|row| {
                row.invocation_id.turn_id.0.len()
                    + row.tool_name.len()
                    + row.arguments_summary.len()
            })
            .sum();
        let snapshot_estimate = std::mem::size_of::<ToolProgressSnapshot>()
            + snapshot.calls.len() * std::mem::size_of::<ToolProgress>()
            + 2 * std::mem::size_of::<usize>()
            + snapshot_strings;
        println!(
            "observation tool={} events={events} snapshot_rows={} snapshot_struct_plus_payload_bytes={snapshot_estimate} result_bytes={} argument_summary_bytes={}",
            kind.name(),
            snapshot.calls.len(),
            serde_json::to_vec(&output).unwrap().len(),
            snapshot.calls[0].arguments_summary.len()
        );
        assert_eq!(output.status, ToolResultStatus::Succeeded);
        assert!(events <= 3);
        assert!(snapshot.calls[0].arguments_summary.len() <= 256);
        total_events += events;
    }
    println!(
        "observation total_events={total_events} max_event_struct_plus_payload_bytes={max_event} WorkProgress_size={} ToolProgress_size={} note=includes_Arc_header_and_string_lengths_excludes_allocator_and_spare_capacity",
        std::mem::size_of::<WorkProgress>(),
        std::mem::size_of::<ToolProgress>()
    );
    assert_eq!(shared.metrics().started, 3);
}
