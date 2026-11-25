use super::find::find_files_with_visited_limit;
use super::operations::{OperationControl, OperationError};
use super::parameters::FindFilesParams;
use super::resources::Counters;
use super::{ToolResources, WorkspaceTools};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits};
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::{EntryKind, Workspace};
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
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

fn setup_tools(
    limits: WorkspaceToolLimits,
) -> (tempfile::TempDir, Arc<Workspace>, Arc<WorkspaceTools>) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let tools = WorkspaceTools::new(workspace.clone(), limits, ToolResources::new(16));
    (dir, workspace, tools)
}

async fn invoke_find(
    shared: &Arc<WorkspaceTools>,
    arguments: Value,
) -> causa::kernel::ToolResultPayload {
    let call = call(WorkspaceTool::FindFiles.name(), arguments);
    let reporter = reporter(std::slice::from_ref(&call));
    let tool = shared
        .bind_for_test(&[WorkspaceTool::FindFiles], reporter.clone())
        .pop()
        .unwrap();
    let control = CallControl::new(CancellationToken::new(), None);
    let result = tool.execute(&call, &control, None).await;
    reporter.drain().await;
    result
}

#[tokio::test]
async fn find_files_matches_globs_brace_case_and_sorted_bfs() {
    let (dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let root = dir.path();
    std::fs::create_dir_all(root.join("crates/core/src/tools")).unwrap();
    std::fs::write(root.join("z_last.rs"), "").unwrap();
    std::fs::write(root.join("a_first.rs"), "").unwrap();
    std::fs::write(root.join("README.md"), "").unwrap();
    std::fs::write(root.join("Cargo.toml"), "").unwrap();
    std::fs::write(root.join("crates/core/src/lib.rs"), "").unwrap();
    std::fs::write(root.join("crates/core/src/tools/find.rs"), "").unwrap();
    std::fs::write(root.join("crates/core/src/tools/READ.RS"), "").unwrap();

    // Omitted arguments -> `{}` lists all regular files in deterministic sorted BFS.
    let all = invoke_find(&shared, json!({})).await;
    assert_eq!(all.status, ToolResultStatus::Succeeded);
    assert_eq!(all.output.content["path"], ".");
    assert_eq!(all.output.content["pattern"], Value::Null);
    assert_eq!(all.output.content["complete"], true);
    assert_eq!(all.output.content["truncated"], false);
    assert_eq!(all.output.content["limit_reason"], Value::Null);
    assert_eq!(all.output.content["issues"], json!([]));
    assert_eq!(
        all.output.content["files"],
        json!([
            "Cargo.toml",
            "README.md",
            "a_first.rs",
            "z_last.rs",
            "crates/core/src/lib.rs",
            "crates/core/src/tools/READ.RS",
            "crates/core/src/tools/find.rs",
        ])
    );

    // Basename glob (case-sensitive by default).
    let rs = invoke_find(&shared, json!({"pattern": "*.rs"})).await;
    assert_eq!(
        rs.output.content["files"],
        json!([
            "a_first.rs",
            "z_last.rs",
            "crates/core/src/lib.rs",
            "crates/core/src/tools/find.rs",
        ])
    );

    // Case-insensitive glob.
    let rs_ci = invoke_find(&shared, json!({"pattern": "*.rs", "ignore_case": true})).await;
    assert_eq!(
        rs_ci.output.content["files"],
        json!([
            "a_first.rs",
            "z_last.rs",
            "crates/core/src/lib.rs",
            "crates/core/src/tools/READ.RS",
            "crates/core/src/tools/find.rs",
        ])
    );

    // Path glob with `**` relative to `path`, returning workspace-relative paths.
    let sub = invoke_find(
        &shared,
        json!({"path": "crates/core", "pattern": "src/**/*.rs"}),
    )
    .await;
    assert_eq!(sub.output.content["path"], "crates/core");
    assert_eq!(
        sub.output.content["files"],
        json!(["crates/core/src/lib.rs", "crates/core/src/tools/find.rs",])
    );

    // Single-level path glob (`*` does not cross `/`).
    let single_level = invoke_find(
        &shared,
        json!({"path": "crates/core", "pattern": "src/*.rs"}),
    )
    .await;
    assert_eq!(
        single_level.output.content["files"],
        json!(["crates/core/src/lib.rs"])
    );

    // Leading `./` anchors relative to `path`.
    let anchored_root = invoke_find(&shared, json!({"pattern": "./*.rs"})).await;
    assert_eq!(
        anchored_root.output.content["files"],
        json!(["a_first.rs", "z_last.rs"])
    );

    // Brace expansion and `?` wildcard.
    let braces = invoke_find(&shared, json!({"pattern": "*.{toml,md}"})).await;
    assert_eq!(
        braces.output.content["files"],
        json!(["Cargo.toml", "README.md"])
    );
    let qmark = invoke_find(&shared, json!({"pattern": "?_first.rs"})).await;
    assert_eq!(qmark.output.content["files"], json!(["a_first.rs"]));
}

#[tokio::test]
async fn find_files_honors_gitignore_ancestor_negation_dotignore_and_vcs() {
    let (dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let root = dir.path();

    std::fs::create_dir_all(root.join("target/debug")).unwrap();
    std::fs::create_dir_all(root.join("build")).unwrap();
    std::fs::create_dir_all(root.join("sub/build")).unwrap();
    std::fs::create_dir_all(root.join("crates/app")).unwrap();
    for vcs in [".git", ".hg", ".svn", ".jj"] {
        std::fs::create_dir_all(root.join(vcs)).unwrap();
        std::fs::write(root.join(vcs).join("meta.txt"), "vcs").unwrap();
    }

    std::fs::write(
        root.join(".gitignore"),
        "# comment\r\n*.log   \r\n!important.log\r\ntarget/\r\n/build/out.txt\r\n\\#hash.txt\r\n\\!bang.txt\r\nspaced\\   \r\n",
    )
    .unwrap();
    std::fs::write(
        root.join("crates/app/.gitignore"),
        "local.tmp\n*.generated.rs\n",
    )
    .unwrap();
    std::fs::write(root.join("crates/app/.ignore"), "!keep.generated.rs\n").unwrap();

    std::fs::write(root.join("app.log"), "").unwrap();
    std::fs::write(root.join("important.log"), "").unwrap();
    std::fs::write(root.join("#hash.txt"), "").unwrap();
    std::fs::write(root.join("!bang.txt"), "").unwrap();
    std::fs::write(root.join("spaced "), "").unwrap();
    std::fs::write(root.join("keep.txt"), "").unwrap();
    std::fs::write(root.join("target/debug/app"), "").unwrap();
    // Regular file named `target` is NOT matched by directory-only `target/`.
    std::fs::write(root.join("sub/target"), "").unwrap();
    std::fs::write(root.join("build/out.txt"), "").unwrap();
    std::fs::write(root.join("sub/build/out.txt"), "").unwrap();
    std::fs::write(root.join("crates/app/main.rs"), "").unwrap();
    std::fs::write(root.join("crates/app/trace.log"), "").unwrap();
    std::fs::write(root.join("crates/app/local.tmp"), "").unwrap();
    std::fs::write(root.join("crates/app/gen.generated.rs"), "").unwrap();
    std::fs::write(root.join("crates/app/keep.generated.rs"), "").unwrap();

    let default_res = invoke_find(&shared, json!({})).await;
    assert_eq!(default_res.status, ToolResultStatus::Succeeded);
    assert_eq!(
        default_res.output.content["files"],
        json!([
            ".gitignore",
            "important.log",
            "keep.txt",
            "sub/target",
            "crates/app/.gitignore",
            "crates/app/.ignore",
            "crates/app/keep.generated.rs",
            "crates/app/main.rs",
            "sub/build/out.txt",
        ])
    );

    // Starting inside a nested directory inherits root `.gitignore` (`*.log`) and honors `.ignore`.
    let nested = invoke_find(&shared, json!({"path": "crates/app"})).await;
    assert_eq!(nested.status, ToolResultStatus::Succeeded);
    assert_eq!(
        nested.output.content["files"],
        json!([
            "crates/app/.gitignore",
            "crates/app/.ignore",
            "crates/app/keep.generated.rs",
            "crates/app/main.rs",
        ])
    );

    // Starting inside an ignored directory (`target`) yields empty unless `include_ignored = true`.
    let ignored_root = invoke_find(&shared, json!({"path": "target"})).await;
    assert_eq!(ignored_root.status, ToolResultStatus::Succeeded);
    assert_eq!(ignored_root.output.content["files"], json!([]));
    assert_eq!(ignored_root.output.content["complete"], true);

    // `include_ignored = true` reveals ignored files while keeping `.git`/`.hg`/`.svn`/`.jj` excluded.
    let with_ignored = invoke_find(&shared, json!({"include_ignored": true})).await;
    let files = with_ignored.output.content["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(files.contains(&"target/debug/app".to_owned()));
    assert!(files.contains(&"app.log".to_owned()));
    assert!(files.contains(&"crates/app/gen.generated.rs".to_owned()));
    assert!(files.contains(&"#hash.txt".to_owned()));
    assert!(files.contains(&"!bang.txt".to_owned()));
    assert!(files.contains(&"spaced ".to_owned()));
    for vcs in [".git", ".hg", ".svn", ".jj"] {
        assert!(
            !files
                .iter()
                .any(|f| f.split('/').any(|comp| comp.eq_ignore_ascii_case(vcs))),
            "leaked {vcs}"
        );
    }
}

#[tokio::test]
async fn find_files_rejects_bad_paths_vcs_and_never_follows_symlinks() {
    let (dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let root = dir.path();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    std::fs::write(outside.path().join("ignore_all"), "*\n").unwrap();

    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("real.rs"), "fn main() {}\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("dir_link")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), root.join("file_link.rs"))
        .unwrap();
    std::os::unix::fs::symlink(outside.path().join("ignore_all"), root.join(".gitignore")).unwrap();

    for bad_path in [
        "../outside",
        "/etc",
        "",
        "sub\\file",
        "sub\0file",
        ".git",
        ".GIT",
        "sub/.git",
        "sub/.hg",
        "sub/.svn",
        "sub/.jj",
    ] {
        let res = invoke_find(&shared, json!({"path": bad_path})).await;
        assert_eq!(
            res.status,
            ToolResultStatus::Rejected,
            "path {bad_path:?} must be rejected"
        );
    }

    // Passing a regular file or symlinked directory as `path` is Rejected.
    let file_as_dir = invoke_find(&shared, json!({"path": "real.rs"})).await;
    assert_eq!(file_as_dir.status, ToolResultStatus::Rejected);
    let link_as_dir = invoke_find(&shared, json!({"path": "dir_link"})).await;
    assert_eq!(link_as_dir.status, ToolResultStatus::Rejected);

    // Missing directory is Failed.
    let missing = invoke_find(&shared, json!({"path": "no_such_dir"})).await;
    assert_eq!(missing.status, ToolResultStatus::Failed);

    // Traversal skips dir_link, file_link.rs, and symlinked `.gitignore` safely.
    let res = invoke_find(&shared, json!({})).await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(res.output.content["files"], json!(["real.rs"]));
}

#[tokio::test]
async fn find_files_enforces_limits_issues_and_cancellation() {
    let (dir, ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::write(root.join("f1.txt"), "").unwrap();
    std::fs::write(root.join("f2.txt"), "").unwrap();
    std::fs::write(root.join("a/f3.txt"), "").unwrap();
    std::fs::write(root.join("a/b/f4.txt"), "").unwrap();
    std::fs::write(root.join("a/b/c/f5.txt"), "").unwrap();

    // `max_results`: exact fit is complete, exceeding sets `limit_reason = "max_results"`.
    let exact = invoke_find(&shared, json!({"max_results": 5})).await;
    assert_eq!(exact.output.content["complete"], true);
    assert_eq!(exact.output.content["truncated"], false);
    assert_eq!(exact.output.content["limit_reason"], Value::Null);
    assert_eq!(exact.output.content["files"].as_array().unwrap().len(), 5);

    let capped = invoke_find(&shared, json!({"max_results": 3})).await;
    assert_eq!(capped.output.content["complete"], false);
    assert_eq!(capped.output.content["truncated"], true);
    assert_eq!(capped.output.content["limit_reason"], "max_results");
    assert_eq!(
        capped.output.content["files"],
        json!(["f1.txt", "f2.txt", "a/f3.txt"])
    );

    // `max_depth`: depth 1 = immediate children only; depth 2 = one subdir down.
    let d1 = invoke_find(&shared, json!({"max_depth": 1})).await;
    assert_eq!(d1.output.content["files"], json!(["f1.txt", "f2.txt"]));
    let d2 = invoke_find(&shared, json!({"max_depth": 2})).await;
    assert_eq!(
        d2.output.content["files"],
        json!(["f1.txt", "f2.txt", "a/f3.txt"])
    );

    // `max_output_bytes`: enforces serialized output budget with `limit_reason = "output_bytes"`.
    let small_limits = WorkspaceToolLimits {
        max_output_bytes: 1024,
        ..WorkspaceToolLimits::default()
    };
    let (dir2, _ws2, small_tools) = setup_tools(small_limits);
    for i in 0..30 {
        std::fs::write(
            dir2.path()
                .join(format!("long_file_name_for_budget_{i:02}.rs")),
            "",
        )
        .unwrap();
    }
    let budgeted = invoke_find(&small_tools, json!({})).await;
    assert_eq!(budgeted.status, ToolResultStatus::Succeeded);
    assert_eq!(budgeted.output.content["complete"], false);
    assert_eq!(budgeted.output.content["truncated"], true);
    assert_eq!(budgeted.output.content["limit_reason"], "output_bytes");
    assert!(
        !budgeted.output.content["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // `visited_entries` hard ceiling.
    let token = CancellationToken::new();
    let op_control = OperationControl {
        call: CallControl::new(token.clone(), None),
        dropstop: CancellationToken::new(),
        counters: Arc::new(Counters::default()),
        host_counters: Arc::new(Counters::default()),
    };
    let visited_res = find_files_with_visited_limit(
        &ws,
        &FindFilesParams {
            path: None,
            pattern: None,
            max_results: None,
            max_depth: None,
            ignore_case: None,
            include_ignored: None,
        },
        &WorkspaceToolLimits::default(),
        &op_control,
        3,
    )
    .unwrap();
    assert_eq!(visited_res["complete"], false);
    assert_eq!(visited_res["truncated"], true);
    assert_eq!(visited_res["limit_reason"], "visited_entries");

    // Pre-cancelled CallControl returns Cancelled before starting filesystem work.
    let pre_token = CancellationToken::new();
    pre_token.cancel();
    let call_ctx = call(WorkspaceTool::FindFiles.name(), json!({}));
    let rep = reporter(std::slice::from_ref(&call_ctx));
    let tool = shared
        .bind_for_test(&[WorkspaceTool::FindFiles], rep.clone())
        .pop()
        .unwrap();
    let before_started = shared.metrics().started;
    let cancelled = tool
        .execute(&call_ctx, &CallControl::new(pre_token, None), None)
        .await;
    rep.drain().await;
    assert_eq!(cancelled.status, ToolResultStatus::Cancelled);
    assert_eq!(shared.metrics().started, before_started);

    // Pre-cancelled inside `OperationControl` returns `OperationError::Control`.
    token.cancel();
    let op_err = find_files_with_visited_limit(
        &ws,
        &FindFilesParams {
            path: None,
            pattern: None,
            max_results: None,
            max_depth: None,
            ignore_case: None,
            include_ignored: None,
        },
        &WorkspaceToolLimits::default(),
        &op_control,
        100,
    )
    .unwrap_err();
    assert!(matches!(op_err, OperationError::Control(_)));
}

#[tokio::test]
async fn find_files_records_permission_issue_and_keeps_going() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let (dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let blocked = dir.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(dir.path().join("visible.rs"), "").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let res = invoke_find(&shared, json!({})).await;
    // Restore permissions so TempDir cleans up cleanly.
    let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));

    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(res.output.content["files"], json!(["visible.rs"]));
    assert_eq!(res.output.content["complete"], false);
    assert_eq!(res.output.content["truncated"], false);
    let issues = res.output.content["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["path"], "blocked");
}

#[tokio::test]
async fn find_files_rejects_invalid_inputs_before_starting_fs_jobs() {
    let (_dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    for invalid in [
        json!({"extra": true}),
        json!({"path": 1}),
        json!({"path": null}),
        json!({"pattern": ""}),
        json!({"pattern": null}),
        json!({"pattern": "x".repeat(257)}),
        json!({"pattern": "*.rs\0"}),
        json!({"pattern": "*.rs\n"}),
        json!({"pattern": "[unclosed"}),
        json!({"pattern": "./"}),
        json!({"max_results": 0}),
        json!({"max_results": 201}),
        json!({"max_results": null}),
        json!({"max_depth": 0}),
        json!({"max_depth": 33}),
        json!({"ignore_case": "yes"}),
        json!({"include_ignored": 1}),
    ] {
        let res = invoke_find(&shared, invalid.clone()).await;
        assert_eq!(
            res.status,
            ToolResultStatus::Rejected,
            "input {invalid} must be rejected"
        );
    }
    assert_eq!(shared.metrics().started, 0);
}

#[test]
fn entry_kind_nofollow_handles_unknown_d_type_without_following_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    std::fs::write(dir.path().join("regular.txt"), "hi").unwrap();
    std::fs::create_dir(dir.path().join("subdir")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("regular.txt"), dir.path().join("sym.txt")).unwrap();

    let root_fd = ws.directory(".").unwrap();
    assert_eq!(
        ws.entry_kind_nofollow(
            &root_fd,
            &CString::new("regular.txt").unwrap(),
            rustix::fs::FileType::Unknown,
        )
        .unwrap(),
        EntryKind::RegularFile
    );
    assert_eq!(
        ws.entry_kind_nofollow(
            &root_fd,
            &CString::new("subdir").unwrap(),
            rustix::fs::FileType::Unknown,
        )
        .unwrap(),
        EntryKind::Directory
    );
    assert_eq!(
        ws.entry_kind_nofollow(
            &root_fd,
            &CString::new("sym.txt").unwrap(),
            rustix::fs::FileType::Unknown,
        )
        .unwrap(),
        EntryKind::Other
    );
}

#[tokio::test]
async fn find_files_distinguishes_single_and_double_backslash_gitignore_escapes() {
    let (dir, _ws, shared) = setup_tools(WorkspaceToolLimits::default());
    let root = dir.path();

    // `\#hash` ignores `#hash`; `\\#lit` ignores `\ #lit` (i.e. `\#lit`), NOT `#lit`.
    // `\!bang` ignores `!bang`; `\\!lit` ignores `\!lit`, NOT `!lit`.
    std::fs::write(
        root.join(".gitignore"),
        "\\#hash\n\\\\#lit\n\\!bang\n\\\\!lit\n",
    )
    .unwrap();
    std::fs::write(root.join("#hash"), "").unwrap();
    std::fs::write(root.join("#lit"), "").unwrap();
    std::fs::write(root.join(std::ffi::OsStr::new("\\#lit")), "").unwrap();
    std::fs::write(root.join("!bang"), "").unwrap();
    std::fs::write(root.join("!lit"), "").unwrap();
    std::fs::write(root.join(std::ffi::OsStr::new("\\!lit")), "").unwrap();

    let res = invoke_find(&shared, json!({})).await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(
        res.output.content["files"],
        json!(["!lit", "#lit", ".gitignore"])
    );
}
