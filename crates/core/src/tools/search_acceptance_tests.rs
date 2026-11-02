//! Real macOS Seatbelt acceptance for the bound `search_text` tool (RS01-RS03).
//!
//! These drive the tool, not the sandbox package, so the parameter decoding, the
//! read-only permission-free path, the assembled `rg` invocation and the result
//! payload are exercised together. The recording-backend tool contract,
//! including fail-closed probe and policy rejections, is cross-platform in
//! `search_tool_tests`.
//!
//! A real ripgrep is required. The tests skip with a printed reason when the
//! host has neither the managed copy nor a suitable `rg` on `PATH`, so a
//! passing run without `rg` is not evidence. Fixtures are private temporary
//! workspaces.

use super::search::SearchRuntime;
use super::{ToolResources, WorkspaceTools};
use crate::config::{Config, ConfigSource};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::external_tools::ExternalTools;
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, Tool, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultPayload, ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn find_rg() -> Option<PathBuf> {
    let managed = ExternalTools::resolve(&Config::load(ConfigSource::Disabled).ok()?);
    if let Some(path) = managed.rg().ready_path() {
        return Some(path.to_path_buf());
    }
    let path_variable = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path_variable) {
        let candidate = directory.join("rg");
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        if rg_major(&candidate).is_some_and(|major| major >= 14) {
            return Some(candidate);
        }
    }
    None
}

fn rg_major(path: &Path) -> Option<u64> {
    let output = Command::new(path)
        .arg("--version")
        .env_clear()
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .lines()
        .next()?
        .strip_prefix("ripgrep ")?
        .split_whitespace()
        .next()?;
    version.split('.').next()?.parse().ok()
}

/// The external-tool check used by the search runtime, or `None` with a notice.
fn tools() -> Option<ExternalTools> {
    let rg = find_rg()?;
    let directory = tempfile::tempdir().ok()?;
    let config_path = directory.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = 1\n[external_tools]\nrg = \"{}\"\n", rg.display()),
    )
    .ok()?;
    let config = Config::load(ConfigSource::File(config_path)).ok()?;
    let tools = ExternalTools::resolve(&config);
    tools.rg().is_ready().then_some(tools)
}

macro_rules! require_tools {
    () => {
        match tools() {
            Some(tools) => tools,
            None => {
                eprintln!(
                    "skipping real search acceptance: no managed ripgrep and no rg >= 14 on PATH"
                );
                return;
            }
        }
    };
}

fn workspace(root: &Path) -> Arc<Workspace> {
    Workspace::open(&root.canonicalize().unwrap()).unwrap()
}

fn bound_tool(
    root: &Path,
    tools: &ExternalTools,
    limits: WorkspaceToolLimits,
    calls: &[ToolCallContext],
) -> Arc<dyn Tool> {
    let workspace = workspace(root);
    let runtime =
        SearchRuntime::bind(workspace.clone(), root, Some(tools)).expect("search runtime binding");
    WorkspaceTools::with_search(
        workspace,
        limits,
        WriteToolLimits::default(),
        ToolResources::new(16),
        Some(runtime),
    )
    .bind_for_test(&[WorkspaceTool::SearchText], reporter(calls))
    .pop()
    .unwrap()
}

fn call(arguments: Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new("search_text", arguments),
    )
}

fn reporter(calls: &[ToolCallContext]) -> ToolReporter {
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "search-acceptance".into(),
            turn_id: turn_id.clone(),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    reporter.batch_ready(
        &InvocationId {
            turn_id,
            round_id: RoundId(0),
        },
        &ToolBatch::new(calls.to_vec()).unwrap(),
    );
    reporter
}

fn control(timeout: Duration) -> CallControl {
    CallControl::new(CancellationToken::new(), Some(Instant::now() + timeout))
}

async fn search(
    root: &Path,
    tools: &ExternalTools,
    arguments: Value,
    timeout: Duration,
) -> ToolResultPayload {
    search_with(
        root,
        tools,
        arguments,
        timeout,
        WorkspaceToolLimits::default(),
    )
    .await
}

async fn search_with(
    root: &Path,
    tools: &ExternalTools,
    arguments: Value,
    timeout: Duration,
    limits: WorkspaceToolLimits,
) -> ToolResultPayload {
    let invocation = call(arguments);
    let tool = bound_tool(root, tools, limits, std::slice::from_ref(&invocation));
    tool.execute(&invocation, &control(timeout), None).await
}

fn paths(payload: &ToolResultPayload) -> Vec<String> {
    payload.output.content["matches"]
        .as_array()
        .map(|matches| {
            matches
                .iter()
                .filter_map(|entry| entry["path"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn write(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
    path
}

fn unique_marker() -> String {
    format!("tars-search-{}", uuid::Uuid::now_v7().simple())
}

/// No process whose argument list contains `marker` is alive.
fn assert_no_process(marker: &str) {
    let output = Command::new("/usr/bin/pgrep")
        .args(["-f", marker])
        .output()
        .expect("pgrep must exist on macOS");
    let found = String::from_utf8_lossy(&output.stdout);
    assert!(
        found.trim().is_empty(),
        "a ripgrep process survived cleanup: {found}"
    );
}

fn ignore_fixture() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, ".gitignore", "target/\n*.log\n!keep.log\n");
    write(&root, ".ignore", "secret.txt\n");
    write(&root, "docs/a.txt", "needle in docs\n");
    write(&root, "docs/nested/.gitignore", "*.tmp\n");
    write(&root, "docs/nested/keep.txt", "needle nested\n");
    write(&root, "docs/nested/drop.tmp", "needle tmp\n");
    write(&root, "keep.log", "needle keep\n");
    write(&root, "drop.log", "needle drop\n");
    write(&root, "secret.txt", "needle secret\n");
    write(&root, "target/ignored.txt", "needle target\n");
    write(&root, ".git/config", "needle git\n");
    // The old in-process budget was 4000 entries; target/ alone exceeds it.
    std::fs::create_dir_all(root.join("target/bulk")).unwrap();
    for index in 0..4100 {
        std::fs::write(root.join(format!("target/bulk/f{index}")), "").unwrap();
    }
    (directory, root)
}

// ---------------------------------------------------------------- RS01

#[tokio::test]
async fn ignore_rules_keep_docs_findable_and_exclude_git_and_ignored_files() {
    let tools = require_tools!();
    let (_directory, root) = ignore_fixture();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    let content = &output.output.content;
    assert_eq!(content["complete"], true, "{content}");
    assert_eq!(content["truncated"], false);
    assert_eq!(content["limit_reason"], Value::Null);
    assert!(content["scanned_files"].as_u64().unwrap() > 0, "{content}");
    assert!(content["scanned_bytes"].as_u64().unwrap() > 0, "{content}");
    assert_eq!(content["scanned_entries"], Value::Null);
    assert_eq!(content["skipped"], 0);
    let found = paths(&output);
    assert!(
        found.iter().any(|path| path == "docs/a.txt"),
        "a docs match must be found even though target/ exceeds the old entry budget: {found:?}"
    );
    assert!(
        found.iter().any(|path| path == "docs/nested/keep.txt"),
        "{found:?}"
    );
    assert!(found.iter().any(|path| path == "keep.log"), "{found:?}");
    for absent in [
        "target/ignored.txt",
        "drop.log",
        "secret.txt",
        "docs/nested/drop.tmp",
    ] {
        assert!(
            !found.iter().any(|path| path == absent),
            "{absent} must stay ignored: {found:?}"
        );
    }
    assert!(
        !found.iter().any(|path| path.starts_with(".git/")),
        "version-control content must never appear: {found:?}"
    );
}

#[tokio::test]
async fn include_ignored_adds_files_but_never_version_control_content() {
    let tools = require_tools!();
    let (_directory, root) = ignore_fixture();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle", "include_ignored": true}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    let found = paths(&output);
    for expected in ["target/ignored.txt", "drop.log", "secret.txt"] {
        assert!(
            found.iter().any(|path| path == expected),
            "include_ignored must add {expected}: {found:?}"
        );
    }
    assert!(
        !found.iter().any(|path| path.starts_with(".git/")),
        "version-control content must never appear: {found:?}"
    );
}

#[tokio::test]
async fn a_workspace_inside_a_repository_honors_its_own_gitignore_only() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    let root = repository.join("workspace");
    std::fs::create_dir_all(repository.join(".git")).unwrap();
    std::fs::create_dir(&root).unwrap();
    // Parent-level rules must not leak into the workspace.
    write(&repository, ".gitignore", "*.txt\n");
    write(&root, ".gitignore", "*.log\n");
    write(&root, "kept.txt", "needle txt\n");
    write(&root, "ignored.log", "needle log\n");
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    assert_eq!(output.output.content["complete"], true);
    let found = paths(&output);
    assert!(
        found.iter().any(|path| path == "kept.txt"),
        "the parent ignore file must not affect the workspace: {found:?}"
    );
    assert!(
        !found.iter().any(|path| path == "ignored.log"),
        "the in-workspace .gitignore must apply even though .git is outside: {found:?}"
    );
}

/// Real measurement on this checkout. Run explicitly for the acceptance record:
/// `cargo test -p tars-core --lib repo_root_measurement -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "real repository measurement: run with --ignored --nocapture"]
async fn repo_root_measurement() {
    let tools = require_tools!();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert!(
        root.join("target").is_dir(),
        "the measurement expects a built target directory"
    );
    let started = Instant::now();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "workspace tools"}),
        Duration::from_secs(10),
    )
    .await;
    let elapsed = started.elapsed();
    let content = &output.output.content;
    println!(
        "repo_root={} status={:?} elapsed_ms={} docs_matches={} scanned_files={} scanned_bytes={} complete={} limit_reason={}",
        root.display(),
        output.status,
        elapsed.as_millis(),
        paths(&output)
            .iter()
            .filter(|path| path.starts_with("docs/"))
            .count(),
        content["scanned_files"],
        content["scanned_bytes"],
        content["complete"],
        content["limit_reason"],
    );
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    assert_eq!(content["complete"], true, "{content}");
    assert!(
        paths(&output).iter().any(|path| path.starts_with("docs/")),
        "the checkout's docs/ must be searched before any budget runs out: {:?}",
        paths(&output)
    );
    assert!(
        !paths(&output)
            .iter()
            .any(|path| path.starts_with("target/")),
        "the built target/ must stay ignored"
    );
}

// ---------------------------------------------------------------- RS02

#[tokio::test]
async fn literal_default_and_regex_opt_in_match_differently() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "queries.txt", "a.c\nabc\naxc\n");
    let literal = search(
        &root,
        &tools,
        json!({"path": ".", "query": "a.c"}),
        Duration::from_secs(10),
    )
    .await;
    let literal_matches = literal.output.content["matches"].as_array().unwrap();
    assert_eq!(literal_matches.len(), 1, "{literal:?}");
    assert_eq!(literal_matches[0]["text"], "a.c");
    let regex = search(
        &root,
        &tools,
        json!({"path": ".", "query": "a.c", "regex": true}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        regex.output.content["matches"].as_array().unwrap().len(),
        3,
        "{regex:?}"
    );
    let anchored = search(
        &root,
        &tools,
        json!({"path": ".", "query": "^abc$", "regex": true}),
        Duration::from_secs(10),
    )
    .await;
    let anchored_matches = anchored.output.content["matches"].as_array().unwrap();
    assert_eq!(anchored_matches.len(), 1, "{anchored:?}");
    assert_eq!(anchored_matches[0]["line"], 2);
    // One explicit regular file stays searchable.
    let single = search(
        &root,
        &tools,
        json!({"path": "queries.txt", "query": "abc"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(single.status, ToolResultStatus::Succeeded, "{single:?}");
    let single_matches = single.output.content["matches"].as_array().unwrap();
    assert_eq!(single_matches.len(), 1, "{single:?}");
    assert_eq!(single_matches[0]["path"], "queries.txt");
    assert_eq!(single_matches[0]["line"], 2);
}

#[tokio::test]
async fn ignore_case_glob_and_context_lines_take_effect() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "src/upper.rs", "Needle here\n");
    write(&root, "src/data.txt", "needle here\n");
    write(
        &root,
        "docs/note.md",
        "context before\nneedle context\nafter one\nafter two\n",
    );
    let insensitive = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle", "ignore_case": true}),
        Duration::from_secs(10),
    )
    .await;
    let found = paths(&insensitive);
    assert!(
        found.iter().any(|path| path == "src/upper.rs"),
        "ignore_case must match the upper-case line: {found:?}"
    );
    let sensitive = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        !paths(&sensitive).iter().any(|path| path == "src/upper.rs"),
        "the default is case-sensitive"
    );
    let filtered = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle", "ignore_case": true, "glob": "*.rs"}),
        Duration::from_secs(10),
    )
    .await;
    let filtered_paths = paths(&filtered);
    assert_eq!(
        filtered_paths,
        vec!["src/upper.rs".to_owned()],
        "{filtered:?}"
    );

    let context = search(
        &root,
        &tools,
        json!({"path": "docs", "query": "needle", "context_lines": 1}),
        Duration::from_secs(10),
    )
    .await;
    let matches = context.output.content["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{context:?}");
    let before = matches[0]["before"].as_array().unwrap();
    let after = matches[0]["after"].as_array().unwrap();
    assert_eq!(before.len(), 1, "{context:?}");
    assert_eq!(before[0]["line"], 1);
    assert_eq!(before[0]["text"], "context before");
    assert_eq!(after.len(), 1, "{context:?}");
    assert_eq!(after[0]["line"], 3);
    assert_eq!(after[0]["text"], "after one");

    // Under a `.` root rg reports `./docs/note.md`; the context still attaches.
    let rooted = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle context", "context_lines": 1}),
        Duration::from_secs(10),
    )
    .await;
    let matches = rooted.output.content["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{rooted:?}");
    assert_eq!(matches[0]["path"], "docs/note.md");
    assert_eq!(matches[0]["after"][0]["text"], "after one", "{rooted:?}");
}

#[tokio::test]
async fn a_pathological_regex_finishes_within_the_call_deadline() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "long.txt", &format!("{}b", "a".repeat(1_000_000)));
    let started = Instant::now();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "(a*)*b", "regex": true}),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a pathological regex must still finish inside the deadline"
    );
    assert!(
        output.status != ToolResultStatus::TimedOut,
        "the linear-time engine must finish: {output:?}"
    );
}

#[tokio::test]
async fn oversize_regex_newline_query_and_invalid_glob_are_rejected_with_a_reason() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "data.txt", "needle\n");
    let oversize = search(
        &root,
        &tools,
        json!({"path": ".", "query": "(a{1000}){1000}", "regex": true}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(oversize.status, ToolResultStatus::Rejected, "{oversize:?}");
    let reason = oversize.output.content["error"].as_str().unwrap();
    assert!(reason.contains("regex"), "{reason}");
    assert!(reason.contains("size limit"), "{reason}");

    let newline = search(
        &root,
        &tools,
        json!({"path": ".", "query": "first\nsecond"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(newline.status, ToolResultStatus::Rejected);
    assert!(!newline.output.content["error"].as_str().unwrap().is_empty());

    let glob = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle", "glob": "["}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(glob.status, ToolResultStatus::Rejected, "{glob:?}");
    let reason = glob.output.content["error"].as_str().unwrap();
    assert!(reason.contains("glob"), "{reason}");
}

#[tokio::test]
async fn dash_prefixed_queries_and_globs_are_only_data() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(
        &root,
        "data.txt",
        "--pre=sh -c touch pwned\n-e pattern line\n",
    );
    let pre = search(
        &root,
        &tools,
        json!({"path": ".", "query": "--pre=sh"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(pre.status, ToolResultStatus::Succeeded, "{pre:?}");
    assert_eq!(
        pre.output.content["matches"].as_array().unwrap().len(),
        1,
        "{pre:?}"
    );
    assert!(
        !root.join("pwned").exists(),
        "a dash-prefixed query must never run another program"
    );
    let flag = search(
        &root,
        &tools,
        json!({"path": ".", "query": "-e pattern"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(flag.status, ToolResultStatus::Succeeded, "{flag:?}");
    assert_eq!(flag.output.content["matches"].as_array().unwrap().len(), 1);
    let glob = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle", "glob": "-e"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(glob.status, ToolResultStatus::Succeeded, "{glob:?}");
}

// ---------------------------------------------------------------- RS03

#[tokio::test]
async fn outside_symlinks_never_appear_in_results() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    write(&outside, "secret.txt", "needle outside\n");
    write(&root, "inside.txt", "needle inside\n");
    std::os::unix::fs::symlink(&outside, root.join("out-directory")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("out-file")).unwrap();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    assert_eq!(output.output.content["complete"], true);
    let found = paths(&output);
    assert_eq!(found, vec!["inside.txt".to_owned()], "{found:?}");
    let linked = search(
        &root,
        &tools,
        json!({"path": "out-directory", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(linked.status, ToolResultStatus::Rejected, "{linked:?}");
}

#[tokio::test]
async fn fifo_and_outside_paths_are_rejected_without_starting_a_process() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    write(&root, "inside.txt", "needle\n");
    std::os::unix::fs::symlink(&outside, root.join("out-directory")).unwrap();
    let fifo = root.join("pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let marker = unique_marker();
    for path in ["pipe", "out-directory", "missing"] {
        let output = search(
            &root,
            &tools,
            json!({"path": path, "query": marker}),
            Duration::from_secs(5),
        )
        .await;
        assert_ne!(
            output.status,
            ToolResultStatus::Succeeded,
            "{path} must not be searched: {output:?}"
        );
        assert_no_process(&marker);
    }
}

#[tokio::test]
async fn max_matches_stops_the_search_and_leaves_no_process() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let marker = unique_marker();
    for index in 0..50 {
        write(
            &root,
            &format!("bulk/f{index}.txt"),
            &format!("{marker}\n{marker}\n"),
        );
    }
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": marker, "max_matches": 5}),
        Duration::from_secs(10),
    )
    .await;
    let content = &output.output.content;
    assert_eq!(output.status, ToolResultStatus::Succeeded, "{output:?}");
    assert_eq!(content["matches"].as_array().unwrap().len(), 5, "{content}");
    assert_eq!(content["complete"], false);
    assert_eq!(content["truncated"], true);
    assert_eq!(content["limit_reason"], "max_matches");
    assert_eq!(content["scanned_files"], Value::Null);
    assert_eq!(content["scanned_bytes"], Value::Null);
    assert_no_process(&marker);
}

#[tokio::test]
async fn read_only_mode_allows_search_without_a_prompt() {
    use crate::permission::{PermissionHub, PermissionMode, ToolPermissionClass};
    // The exhaustive `wm17` matrix already pins `KnownReadOnly` to direct
    // execution in read-only mode; here the bound tool must keep that class.
    let view = PermissionHub::new(PermissionMode::ReadOnly).view();
    assert_eq!(
        view.classify_tool("search_text"),
        Some(ToolPermissionClass::KnownReadOnly)
    );
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "file.txt", "needle\n");
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
}

#[tokio::test]
async fn rg_stderr_errors_become_issues_and_mark_the_search_incomplete() {
    use std::os::unix::fs::PermissionsExt;
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "readable.txt", "needle\n");
    write(&root, "denied/hidden.txt", "needle\n");
    let denied = root.join("denied");
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o0)).unwrap();
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
    let content = &output.output.content;
    assert_eq!(output.status, ToolResultStatus::Succeeded, "{output:?}");
    assert_eq!(content["complete"], false, "{content}");
    assert_eq!(content["truncated"], false);
    let issues = content["issues"].as_array().unwrap();
    assert!(!issues.is_empty(), "{content}");
    assert!(issues.len() <= 32);
    assert!(
        issues.iter().any(|issue| issue["path"] == "denied"),
        "{issues:?}"
    );
    assert_eq!(content["skipped"].as_u64().unwrap(), issues.len() as u64);
    assert!(
        paths(&output).iter().any(|path| path == "readable.txt"),
        "the readable match must still be returned"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_kills_the_search_and_returns_the_partial_result() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let marker = unique_marker();
    let bulk = root.join(&marker);
    std::fs::create_dir(&bulk).unwrap();
    // 768 MiB of digits with a Unicode-property regex scans for several hundred
    // milliseconds, longer than the 100 ms minimum search deadline.
    let block = "0123456789".repeat(64) + "\n";
    let mut file = std::fs::File::create(bulk.join("digits.txt")).unwrap();
    use std::io::Write;
    let blocks = (768 * 1024 * 1024) / block.len();
    for _ in 0..blocks {
        file.write_all(block.as_bytes()).unwrap();
    }
    drop(file);
    let limits = WorkspaceToolLimits {
        max_file_scan_bytes: 1024 * 1024 * 1024,
        ..Default::default()
    };
    let started = Instant::now();
    let output = search_with(
        &root,
        &tools,
        json!({"path": marker, "query": r"\p{Greek}", "regex": true}),
        Duration::from_millis(150),
        limits,
    )
    .await;
    let elapsed = started.elapsed();
    let content = &output.output.content;
    assert_eq!(output.status, ToolResultStatus::TimedOut, "{output:?}");
    assert_eq!(content["complete"], false);
    assert_eq!(content["scanned_files"], Value::Null);
    assert_eq!(content["scanned_bytes"], Value::Null);
    assert!(content["matches"].is_array(), "{content}");
    assert!(
        elapsed < Duration::from_secs(8),
        "a timed-out search must return promptly: {elapsed:?}"
    );
    assert_no_process(&marker);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_search_kills_the_process_tree() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let marker = unique_marker();
    let bulk = root.join(&marker);
    std::fs::create_dir(&bulk).unwrap();
    let block = "0123456789".repeat(64) + "\n";
    let mut file = std::fs::File::create(bulk.join("digits.txt")).unwrap();
    use std::io::Write;
    let blocks = (768 * 1024 * 1024) / block.len();
    for _ in 0..blocks {
        file.write_all(block.as_bytes()).unwrap();
    }
    drop(file);
    let limits = WorkspaceToolLimits {
        max_file_scan_bytes: 1024 * 1024 * 1024,
        ..Default::default()
    };
    let invocation = call(json!({"path": marker, "query": r"\p{Greek}", "regex": true}));
    let tool = bound_tool(&root, &tools, limits, std::slice::from_ref(&invocation));
    let cancellation = CancellationToken::new();
    let call_control = CallControl::new(cancellation.clone(), None);
    let task = tokio::spawn({
        let invocation = invocation.clone();
        async move { tool.execute(&invocation, &call_control, None).await }
    });
    tokio::time::sleep(Duration::from_millis(60)).await;
    cancellation.cancel();
    let output = tokio::time::timeout(Duration::from_secs(8), task)
        .await
        .expect("a cancelled search must return promptly")
        .unwrap();
    let content = &output.output.content;
    assert_eq!(output.status, ToolResultStatus::Cancelled, "{output:?}");
    assert_eq!(content["complete"], false);
    assert_eq!(content["scanned_bytes"], Value::Null);
    assert_no_process(&marker);
}

#[tokio::test]
async fn max_depth_and_max_filesize_bound_the_traversal_while_file_budgets_do_not() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(&root, "a.txt", "needle root\n");
    write(&root, "one/b.txt", "needle one\n");
    write(&root, "one/two/c.txt", "needle two\n");
    write(&root, "big.dat", &format!("needle{}", "x".repeat(1000)));
    // `max_search_files` and `max_search_scan_bytes` no longer bound a search;
    // the stored values stay accepted but must not cut the result.
    let shallow = WorkspaceToolLimits {
        max_search_depth: 0,
        max_file_scan_bytes: 40,
        max_search_files: 1,
        max_search_scan_bytes: 1,
        ..Default::default()
    };
    let output = search_with(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
        shallow,
    )
    .await;
    assert_eq!(output.status, ToolResultStatus::Succeeded, "{output:?}");
    let found = paths(&output);
    assert_eq!(found, vec!["a.txt".to_owned()], "{found:?}");
    assert_eq!(
        output.output.content["complete"], true,
        "the oversize file is skipped without an issue: {output:?}"
    );

    let deeper = WorkspaceToolLimits {
        max_search_depth: 1,
        max_file_scan_bytes: 40,
        max_search_files: 1,
        max_search_scan_bytes: 1,
        ..Default::default()
    };
    let output = search_with(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
        deeper,
    )
    .await;
    let mut found = paths(&output);
    found.sort();
    assert_eq!(
        found,
        vec!["a.txt".to_owned(), "one/b.txt".to_owned()],
        "one directory level is searched and the file budgets are unused: {found:?}"
    );
    assert!(
        !found.iter().any(|path| path == "one/two/c.txt"),
        "the second level stays outside the depth bound"
    );
}

#[tokio::test]
async fn a_long_line_becomes_a_bounded_snippet() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    write(
        &root,
        "long.txt",
        &format!("{}needle{}", "文".repeat(2000), "字".repeat(2000)),
    );
    let output = search(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
    )
    .await;
    let matches = output.output.content["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1, "{output:?}");
    let snippet = matches[0]["text"].as_str().unwrap();
    assert!(snippet.contains("needle"));
    assert!(snippet.len() <= 262, "snippet bytes: {}", snippet.len());
    assert_eq!(matches[0]["snippet_truncated"], true);
}

#[tokio::test]
async fn the_output_byte_limit_marks_the_result_incomplete() {
    let tools = require_tools!();
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    for index in 0..100 {
        write(&root, &format!("bulk/f{index}.txt"), "needle\n");
    }
    let limits = WorkspaceToolLimits {
        max_output_bytes: 2048,
        ..Default::default()
    };
    let output = search_with(
        &root,
        &tools,
        json!({"path": ".", "query": "needle"}),
        Duration::from_secs(10),
        limits,
    )
    .await;
    let content = &output.output.content;
    assert_eq!(output.status, ToolResultStatus::Succeeded, "{output:?}");
    assert_eq!(content["complete"], false);
    assert_eq!(content["truncated"], true);
    assert_eq!(content["limit_reason"], "output_bytes");
    let matches = content["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "matches that fit are kept: {content}");
    assert!(
        serde_json::to_vec(content).unwrap().len() <= 2048,
        "the serialized result stays inside the byte bound"
    );
}
