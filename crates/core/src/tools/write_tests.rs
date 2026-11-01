use super::execute::TestGate;
use super::{DiffSummary, ToolResources, WorkspaceTools, compute_diff};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::ToolBatchProcessor;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultPayload, ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::sync::Arc;
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
    reporter.batch_ready(
        &InvocationId {
            turn_id,
            round_id: RoundId(0),
        },
        &batch,
    );
    reporter
}

fn tools(
    limits: WorkspaceToolLimits,
    write_limits: WriteToolLimits,
) -> (tempfile::TempDir, Arc<WorkspaceTools>) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    (
        dir,
        WorkspaceTools::with_write_limits(workspace, limits, write_limits, ToolResources::new(16)),
    )
}

fn control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

async fn invoke(
    shared: &Arc<WorkspaceTools>,
    kind: WorkspaceTool,
    arguments: Value,
) -> ToolResultPayload {
    invoke_with_control(shared, kind, arguments, control()).await
}

async fn invoke_with_control(
    shared: &Arc<WorkspaceTools>,
    kind: WorkspaceTool,
    arguments: Value,
    control: CallControl,
) -> ToolResultPayload {
    let call = call(kind.name(), arguments);
    let reporter = reporter(std::slice::from_ref(&call));
    let prepared_writes = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let tool = shared
        .bind_with_prepared(&[kind], reporter.clone(), prepared_writes.clone())
        .pop()
        .unwrap();

    if matches!(kind, WorkspaceTool::WriteFile | WorkspaceTool::EditFile) {
        let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
        let turn_id = TurnId::new("precheck");
        let declaration_order = [call.call_block_id];
        let ctx = causa::kernel::ProcessorContext::new(
            &turn_id,
            RoundId(0),
            &declaration_order,
            &control,
        );
        let precheck = crate::permission::PrecheckProcessor::new(
            shared.clone(),
            reporter.clone(),
            prepared_writes,
        );
        let _ = precheck.process(&mut batch, &ctx).await;
        if let Some((_, res)) = batch.results().first().and_then(|e| e.result()) {
            reporter.drain().await;
            return res.clone();
        }
    }

    let result = tool.execute(&call, &control, None).await;
    reporter.drain().await;
    result
}

fn has_temporary_files(dir: &std::path::Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with(".tars-write-") && name.ends_with(".tmp"))
            {
                return true;
            }
            if path.is_dir() && has_temporary_files(&path) {
                return true;
            }
        }
    }
    false
}

#[tokio::test]
async fn write_file_creates_new_file_and_intermediate_directories() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let path = "nested/deep/directory/test.txt";
    let content = "hello\nworld\n";
    let result = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": path, "content": content}),
    )
    .await;

    assert_eq!(result.status, ToolResultStatus::Succeeded);
    assert_eq!(result.output.content["path"], path);
    assert_eq!(result.output.content["action"], "created");
    assert_eq!(result.output.content["bytes_written"], content.len());
    assert_eq!(result.output.content["lines_added"], 2);
    assert_eq!(result.output.content["lines_removed"], 0);

    let written = std::fs::read_to_string(dir.path().join(path)).unwrap();
    assert_eq!(written, content);
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn write_file_validation_failure_creates_neither_target_nor_intermediate_dirs() {
    let (dir, shared) = tools(
        WorkspaceToolLimits::default(),
        WriteToolLimits {
            max_write_bytes: 10,
            ..Default::default()
        },
    );

    let result = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "newdir/child/file.txt", "content": "this content is longer than 10 bytes"}),
    )
    .await;
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert!(!dir.path().join("newdir").exists());

    let result = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "newdir/../forbidden.txt", "content": "hello"}),
    )
    .await;
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert!(!dir.path().join("newdir").exists());
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn write_file_counterexamples_are_all_rejected() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    std::fs::write(dir.path().join("real_target.txt"), "existing").unwrap();
    symlink("real_target.txt", dir.path().join("symlink_file")).unwrap();

    std::fs::create_dir(dir.path().join("real_dir")).unwrap();
    symlink("real_dir", dir.path().join("symlink_dir")).unwrap();

    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "/tmp/test.txt", "content": "foo"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "../outside.txt", "content": "foo"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "symlink_file", "content": "foo"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("real_target.txt")).unwrap(),
        "existing"
    );

    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "symlink_dir/file.txt", "content": "foo"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    assert!(
        std::process::Command::new("mkfifo")
            .arg(dir.path().join("fifo_file"))
            .status()
            .unwrap()
            .success()
    );
    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "fifo_file", "content": "foo"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    // Cross-device rejection (e.g. /dev if mounted on a different device than root)
    let root_dev = std::fs::metadata("/").unwrap().dev();
    let dev_dev = std::fs::metadata("/dev").unwrap().dev();
    if root_dev != dev_dev {
        let dev_ws = Workspace::open(std::path::Path::new("/")).unwrap();
        let dev_shared = WorkspaceTools::new(
            dev_ws,
            WorkspaceToolLimits::default(),
            ToolResources::new(16),
        );
        let res = invoke(
            &dev_shared,
            WorkspaceTool::WriteFile,
            json!({"path": "dev/nonexistent_device_test_file.txt", "content": "foo"}),
        )
        .await;
        assert_eq!(res.status, ToolResultStatus::Rejected);
        let err = res.output.content["error"].as_str().unwrap();
        assert!(err.contains("device boundaries"));
    } else {
        eprintln!(
            "skipping cross-device test: '/' and '/dev' are on the same filesystem device ({root_dev})"
        );
    }

    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn write_file_overwriting_existing_file_preserves_permission_bits() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("script.sh");
    std::fs::write(&file_path, "echo 'old'\n").unwrap();
    std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let read_res = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "script.sh"}),
    )
    .await;
    assert_eq!(read_res.status, ToolResultStatus::Succeeded);

    let write_res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "script.sh", "content": "echo 'new'\n"}),
    )
    .await;
    assert_eq!(write_res.status, ToolResultStatus::Succeeded);
    assert_eq!(write_res.output.content["action"], "updated");

    let perms = std::fs::metadata(&file_path).unwrap().permissions().mode();
    assert_eq!(perms & 0o777, 0o755);
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn edit_file_multiple_segments_apply_simultaneously() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("code.rs");
    let original = "fn alpha() -> i32 { 1 }\nfn beta() -> i32 { 2 }\nfn gamma() -> i32 { 3 }\n";
    std::fs::write(&file_path, original).unwrap();

    invoke(&shared, WorkspaceTool::ReadFile, json!({"path": "code.rs"})).await;

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "code.rs",
            "edits": [
                {"old_text": "fn beta() -> i32 { 2 }", "new_text": "fn beta() -> i32 { 20 }"},
                {"old_text": "fn alpha() -> i32 { 1 }", "new_text": "fn alpha() -> i32 { 10 }"}
            ]
        }),
    )
    .await;

    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(res.output.content["action"], "updated");
    assert_eq!(res.output.content["lines_added"], 2);
    assert_eq!(res.output.content["lines_removed"], 2);

    let updated = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(
        updated,
        "fn alpha() -> i32 { 10 }\nfn beta() -> i32 { 20 }\nfn gamma() -> i32 { 3 }\n"
    );
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn edit_file_error_cases_are_rejected_with_edits_index() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("sample.txt");
    let original = "apple\nbanana\napple\ncherry\n";
    std::fs::write(&file_path, original).unwrap();

    invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "sample.txt"}),
    )
    .await;

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "sample.txt",
            "edits": [
                {"old_text": "nonexistent", "new_text": "foo"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("edits[0]"));
    assert!(err.contains("not found"));

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "sample.txt",
            "edits": [
                {"old_text": "apple", "new_text": "orange"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("edits[0]"));
    assert!(err.contains("multiple"));

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "sample.txt",
            "edits": [
                {"old_text": "banana\n", "new_text": "pear\n"},
                {"old_text": "banana\napple", "new_text": "grape"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("edits[0]") && err.contains("edits[1]"));
    assert!(err.contains("overlap"));

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "sample.txt",
            "edits": [
                {"old_text": "", "new_text": "something"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("edits[0]"));
    assert!(err.contains("must not be empty"));

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "sample.txt",
            "edits": [
                {"old_text": "cherry", "new_text": "cherry"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("edits[0]"));
    assert!(err.contains("identical"));

    let unmod = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(unmod, original);
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn edit_file_limits_segments_and_sizes() {
    let (dir, shared) = tools(
        WorkspaceToolLimits::default(),
        WriteToolLimits {
            max_edit_file_bytes: 50,
            max_write_bytes: 40,
            max_edit_segments: 2,
        },
    );

    let big_path = dir.path().join("big.txt");
    std::fs::write(&big_path, "a".repeat(100)).unwrap();
    invoke(&shared, WorkspaceTool::ReadFile, json!({"path": "big.txt"})).await;

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "big.txt",
            "edits": [{"old_text": "a", "new_text": "b"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let small_path = dir.path().join("small.txt");
    std::fs::write(&small_path, "1\n2\n3\n4\n").unwrap();
    invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "small.txt"}),
    )
    .await;

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "small.txt",
            "edits": [
                {"old_text": "1", "new_text": "x"},
                {"old_text": "2", "new_text": "y"},
                {"old_text": "3", "new_text": "z"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "small.txt",
            "edits": [
                {"old_text": "1", "new_text": "x".repeat(50)}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn edit_file_crlf_file_remains_crlf_after_editing() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("crlf.txt");
    let original = "first line\r\nsecond line\r\nthird line\r\n";
    std::fs::write(&file_path, original).unwrap();

    invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "crlf.txt"}),
    )
    .await;

    // A lone \n in a replacement is converted to \r\n for a CRLF file.
    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "crlf.txt",
            "edits": [
                {"old_text": "second line\n", "new_text": "modified second\nnew line\n"}
            ]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);

    let updated = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(
        updated,
        "first line\r\nmodified second\r\nnew line\r\nthird line\r\n"
    );
    assert!(!updated.replace("\r\n", "").contains('\n'));
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn in_instance_observation_and_stale_detection() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("observed.txt");
    std::fs::write(&file_path, "version 1\n").unwrap();

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "observed.txt",
            "edits": [{"old_text": "version 1", "new_text": "version 2"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "observed.txt", "content": "version 2\n"}),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);

    let read_res = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "observed.txt"}),
    )
    .await;
    assert_eq!(read_res.status, ToolResultStatus::Succeeded);

    std::fs::write(&file_path, "version 1 external modification\n").unwrap();
    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "observed.txt",
            "edits": [{"old_text": "version 1", "new_text": "version 2"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    assert!(
        res.output.content["error"]
            .as_str()
            .unwrap()
            .contains("changed")
    );

    invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "observed.txt"}),
    )
    .await;

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "observed.txt",
            "edits": [{"old_text": "external modification", "new_text": "internal edit 1"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);

    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "observed.txt",
            "edits": [{"old_text": "internal edit 1", "new_text": "internal edit 2"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);

    let partial_path = dir.path().join("multiline.txt");
    std::fs::write(&partial_path, "line 1\nline 2\nline 3\nline 4\n").unwrap();
    let partial_read = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "multiline.txt", "start_line": 1, "max_lines": 1}),
    )
    .await;
    assert_eq!(partial_read.status, ToolResultStatus::Succeeded);

    // A partial read observes the whole file, so an unread line can be edited.
    let res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "multiline.txt",
            "edits": [{"old_text": "line 4", "new_text": "line four"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);

    let new_workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let new_shared = WorkspaceTools::new(
        new_workspace,
        WorkspaceToolLimits::default(),
        ToolResources::new(16),
    );
    let res = invoke(
        &new_shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "observed.txt",
            "edits": [{"old_text": "internal edit 2", "new_text": "edit in new session"}]
        }),
    )
    .await;
    assert_eq!(res.status, ToolResultStatus::Rejected);
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn hints_for_all_five_tools_match_specification() {
    let (_dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let reporter = reporter(&[]);
    let bound = shared.bind_for_test(
        &[
            WorkspaceTool::ListDirectory,
            WorkspaceTool::ReadFile,
            WorkspaceTool::SearchText,
            WorkspaceTool::WriteFile,
            WorkspaceTool::EditFile,
        ],
        reporter,
    );

    let list_hints = bound[0].hints();
    assert_eq!(list_hints.read_only, Some(true));
    assert_eq!(list_hints.destructive, None);
    assert_eq!(list_hints.idempotent, None);
    assert_eq!(list_hints.open_world, Some(false));

    let read_hints = bound[1].hints();
    assert_eq!(read_hints.read_only, Some(true));
    assert_eq!(read_hints.destructive, None);
    assert_eq!(read_hints.idempotent, None);
    assert_eq!(read_hints.open_world, Some(false));

    let search_hints = bound[2].hints();
    assert_eq!(search_hints.read_only, Some(true));
    assert_eq!(search_hints.destructive, None);
    assert_eq!(search_hints.idempotent, None);
    assert_eq!(search_hints.open_world, Some(false));

    let write_hints = bound[3].hints();
    assert_eq!(write_hints.read_only, Some(false));
    assert_eq!(write_hints.destructive, Some(true));
    assert_eq!(write_hints.idempotent, Some(true));
    assert_eq!(write_hints.open_world, Some(false));

    let edit_hints = bound[4].hints();
    assert_eq!(edit_hints.read_only, Some(false));
    assert_eq!(edit_hints.destructive, Some(true));
    assert_eq!(edit_hints.idempotent, Some(false));
    assert_eq!(edit_hints.open_world, Some(false));
}

#[test]
fn unified_diff_and_stats_match_expected_output() {
    let path = "src/main.rs";
    let old = "fn main() {\n    println!(\"hello\");\n}\n";
    let new = "fn main() {\n    println!(\"hello world\");\n    println!(\"goodbye\");\n}\n";

    let summary: DiffSummary = compute_diff(path, old, new);
    assert_eq!(summary.lines_added, 2);
    assert_eq!(summary.lines_removed, 1);

    let expected = concat!(
        "--- a/src/main.rs\n",
        "+++ b/src/main.rs\n",
        "@@ -1,3 +1,4 @@\n",
        " fn main() {\n",
        "-    println!(\"hello\");\n",
        "+    println!(\"hello world\");\n",
        "+    println!(\"goodbye\");\n",
        " }\n",
    );

    assert_eq!(summary.diff, expected);
}

#[tokio::test]
async fn cancellation_before_rename_leaves_file_intact_and_removes_tempfile() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("cancel_target.txt");
    std::fs::write(&file_path, "original content\n").unwrap();

    invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "cancel_target.txt"}),
    )
    .await;

    let gate = TestGate::new(false);
    shared.set_write_gate(Some(gate.clone()));

    let cancel_token = CancellationToken::new();
    let cancel_control = CallControl::new(cancel_token.clone(), None);

    let shared_task = shared.clone();
    let task = tokio::spawn(async move {
        invoke_with_control(
            &shared_task,
            WorkspaceTool::WriteFile,
            json!({"path": "cancel_target.txt", "content": "cancelled write\n"}),
            cancel_control,
        )
        .await
    });

    // Stop the worker right before the rename, once the temp file is fsynced.
    gate.reached.notified().await;

    assert!(has_temporary_files(dir.path()));

    cancel_token.cancel();
    gate.release();

    let result = task.await.unwrap();
    assert_eq!(result.status, ToolResultStatus::Cancelled);

    let content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "original content\n");

    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn crlf_with_chinese_and_emoji_multibyte_utf8_edit_and_overwrite() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("crlf_multibyte.txt");
    let original = "你好，世界！\r\n测试🚀表情符号\r\n第三行内容\r\n";
    std::fs::write(&file_path, original).unwrap();

    let read_res = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "crlf_multibyte.txt"}),
    )
    .await;
    assert_eq!(read_res.status, ToolResultStatus::Succeeded);

    let edit_res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "crlf_multibyte.txt",
            "edits": [
                {
                    "old_text": "测试🚀表情符号\n",
                    "new_text": "替换后的中文🎉以及🌟表情\n"
                }
            ]
        }),
    )
    .await;
    assert_eq!(edit_res.status, ToolResultStatus::Succeeded);

    let expected_edit = "你好，世界！\r\n替换后的中文🎉以及🌟表情\r\n第三行内容\r\n";
    let edited_content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(edited_content, expected_edit);
    assert_eq!(std::fs::read(&file_path).unwrap(), expected_edit.as_bytes());
    assert!(!has_temporary_files(dir.path()));

    let overwrite_content = "全新第一行👋欢迎\n第二行🌟包含emoji和汉字\n";
    let write_res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "crlf_multibyte.txt", "content": overwrite_content}),
    )
    .await;
    assert_eq!(write_res.status, ToolResultStatus::Succeeded);

    let expected_overwrite = "全新第一行👋欢迎\r\n第二行🌟包含emoji和汉字\r\n";
    let overwritten_content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(overwritten_content, expected_overwrite);
    assert_eq!(
        std::fs::read(&file_path).unwrap(),
        expected_overwrite.as_bytes()
    );
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn scan_limit_rejection_and_max_lines_partial_read() {
    let limits = WorkspaceToolLimits {
        max_file_scan_bytes: 80,
        ..Default::default()
    };
    let (dir, shared) = tools(limits, WriteToolLimits::default());
    let file_path = dir.path().join("large_scan.txt");
    let large_content = "a".repeat(150);
    std::fs::write(&file_path, &large_content).unwrap();

    // A scan-limited read is not recorded, so a later edit cannot rely on it.
    let read_res = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "large_scan.txt"}),
    )
    .await;
    assert_eq!(read_res.status, ToolResultStatus::Succeeded);
    assert_eq!(read_res.output.content["truncated"], true);
    assert_eq!(read_res.output.content["limit_reason"], "file_scan_bytes");

    let edit_res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "large_scan.txt",
            "edits": [{"old_text": "aaa", "new_text": "bbb"}]
        }),
    )
    .await;
    assert_eq!(edit_res.status, ToolResultStatus::Rejected);
    let err = edit_res.output.content["error"].as_str().unwrap();
    assert!(err.contains("exceeds maximum readable size") || err.contains("must be read"));

    let write_res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "large_scan.txt", "content": "replacement"}),
    )
    .await;
    assert_eq!(write_res.status, ToolResultStatus::Rejected);

    let multiline_path = dir.path().join("multiline_lines.txt");
    std::fs::write(&multiline_path, "line1\nline2\nline3\n").unwrap();

    let partial_read = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "multiline_lines.txt", "max_lines": 1}),
    )
    .await;
    assert_eq!(partial_read.status, ToolResultStatus::Succeeded);
    assert_eq!(partial_read.output.content["truncated"], true);
    assert_eq!(partial_read.output.content["limit_reason"], "max_lines");

    let edit_res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "multiline_lines.txt",
            "edits": [{"old_text": "line3", "new_text": "line three"}]
        }),
    )
    .await;
    assert_eq!(edit_res.status, ToolResultStatus::Succeeded);
    assert_eq!(
        std::fs::read_to_string(&multiline_path).unwrap(),
        "line1\nline2\nline three\n"
    );
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn write_file_new_target_conflict_before_rename_is_rejected() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let target_path = dir.path().join("conflict_target.txt");

    let gate = TestGate::new(false);
    shared.set_write_gate(Some(gate.clone()));

    let shared_task = shared.clone();
    let task = tokio::spawn(async move {
        invoke(
            &shared_task,
            WorkspaceTool::WriteFile,
            json!({"path": "conflict_target.txt", "content": "from write_file\n"}),
        )
        .await
    });

    // Pause the worker right before the rename, once the temp file is fsynced.
    gate.reached.notified().await;
    assert!(has_temporary_files(dir.path()));

    std::fs::write(&target_path, "externally created\n").unwrap();

    gate.release();
    let res = task.await.unwrap();

    assert_eq!(res.status, ToolResultStatus::Rejected);
    let err = res.output.content["error"].as_str().unwrap();
    assert!(err.contains("already exists") && err.contains("must be read with read_file"));

    let content = std::fs::read_to_string(&target_path).unwrap();
    assert_eq!(content, "externally created\n");

    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn deleted_after_reading_returns_rejected_for_edit_and_creates_for_write() {
    let (dir, shared) = tools(WorkspaceToolLimits::default(), WriteToolLimits::default());
    let file_path = dir.path().join("read_then_delete.txt");
    std::fs::write(&file_path, "initial content\n").unwrap();

    let read_res = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "read_then_delete.txt"}),
    )
    .await;
    assert_eq!(read_res.status, ToolResultStatus::Succeeded);

    std::fs::remove_file(&file_path).unwrap();

    let edit_res = invoke(
        &shared,
        WorkspaceTool::EditFile,
        json!({
            "path": "read_then_delete.txt",
            "edits": [{"old_text": "initial content", "new_text": "updated"}]
        }),
    )
    .await;
    assert_eq!(edit_res.status, ToolResultStatus::Rejected);
    let err = edit_res.output.content["error"].as_str().unwrap();
    assert!(err.contains("changed"));

    let write_res = invoke(
        &shared,
        WorkspaceTool::WriteFile,
        json!({"path": "read_then_delete.txt", "content": "recreated\n"}),
    )
    .await;
    assert_eq!(write_res.status, ToolResultStatus::Succeeded);
    assert_eq!(write_res.output.content["action"], "created");
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "recreated\n");
    assert!(!has_temporary_files(dir.path()));
}

#[tokio::test]
async fn execute_without_precheck_returns_rejected() {
    let (_dir, shared) = tools(Default::default(), Default::default());
    let call = call(
        "write_file",
        serde_json::json!({
            "path": "test.txt",
            "content": "hello"
        }),
    );
    let reporter = reporter(std::slice::from_ref(&call));
    let tool = shared
        .bind_for_test(&[WorkspaceTool::WriteFile], reporter.clone())
        .pop()
        .unwrap();
    let result = tool.execute(&call, &control(), None).await;
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert_eq!(
        result.output.content["error"],
        "write operation was not prepared by precheck"
    );
}

#[tokio::test]
async fn confirmation_reread_stops_at_the_edit_limit_after_external_growth() {
    let write_limits = WriteToolLimits {
        max_edit_file_bytes: 16,
        max_write_bytes: 1024,
        max_edit_segments: 32,
    };
    let (dir, shared) = tools(WorkspaceToolLimits::default(), write_limits);
    let path = dir.path().join("bounded.txt");
    std::fs::write(&path, "0123456789").unwrap();

    let read = invoke(
        &shared,
        WorkspaceTool::ReadFile,
        json!({"path": "bounded.txt"}),
    )
    .await;
    assert_eq!(read.status, ToolResultStatus::Succeeded);

    let call = call(
        "write_file",
        json!({"path": "bounded.txt", "content": "replacement"}),
    );
    let reporter = reporter(std::slice::from_ref(&call));
    let prepared_writes = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let tool = shared
        .bind_with_prepared(
            &[WorkspaceTool::WriteFile],
            reporter.clone(),
            prepared_writes.clone(),
        )
        .pop()
        .unwrap();
    let control = control();
    let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
    let turn_id = TurnId::new("bounded-precheck");
    let declaration_order = [call.call_block_id];
    let ctx =
        causa::kernel::ProcessorContext::new(&turn_id, RoundId(0), &declaration_order, &control);
    let precheck = crate::permission::PrecheckProcessor::new(
        shared.clone(),
        reporter.clone(),
        prepared_writes,
    );
    let _ = precheck.process(&mut batch, &ctx).await;
    assert!(batch.results().is_empty(), "precheck accepted the write");

    // Grow the file after the successful precheck. The confirmation reread may
    // consume one byte past the configured limit, never the grown file.
    let grown = "x".repeat(1024);
    std::fs::write(&path, &grown).unwrap();
    let result = tool.execute(&call, &control, None).await;
    assert_eq!(result.status, ToolResultStatus::Failed);
    let error = result.output.content["error"].as_str().unwrap();
    assert!(error.contains("file changed after confirmation"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), grown);
    assert!(!has_temporary_files(dir.path()));
}
