//! Cross-platform tests for the bound `search_text` tool.
//!
//! These bind the recording backend, which performs no isolation and records
//! the launches it receives, so they observe the tool contract, the sandbox
//! policy and the assembled argument list, and whether a process launch was
//! attempted. They run on Linux, macOS and Windows CI; the real Seatbelt
//! acceptance stays in `search_acceptance_tests` and only runs on macOS when a
//! local ripgrep is available.
//!
//! Windows has no workspace tools, so tests that need a workspace handle use
//! the detached handle; the search runtime resolves its own root and the file
//! tools are never touched.

use super::search::{SearchCheckError, SearchRuntime};
use super::{ToolResources, WorkspaceTools};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::sandbox::test_support::RecordingBackend;
#[cfg(unix)]
use crate::sandbox::test_support::{
    launch_args, launch_environment, launch_program, launch_working_directory, policy_paths,
};
use crate::sandbox::{BackendId, BackendReport, Capability, Limitation, Sandbox};
#[cfg(unix)]
use crate::sandbox::{Launch, NetworkAccess, ReadScope, SandboxPolicy};
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, Tool, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
type RecordedLaunches = Arc<Mutex<Vec<(SandboxPolicy, Launch)>>>;

fn full_report() -> BackendReport {
    BackendReport::new(
        BackendId::RECORDING,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    )
}

fn report(
    file_read: Capability,
    file_write: Capability,
    network: Capability,
    signal_isolation: Capability,
    descendant_lifecycle: Capability,
) -> BackendReport {
    BackendReport::new(
        BackendId::RECORDING,
        file_read,
        file_write,
        network,
        signal_isolation,
        descendant_lifecycle,
    )
}

fn workspace(root: &Path) -> Arc<Workspace> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        Workspace::open(&root.canonicalize().unwrap()).unwrap()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        Workspace::detached_for_command_tests()
    }
}

/// Bind a runtime whose recording backend records every wrapped launch.
#[cfg(unix)]
fn recording_runtime_with_report(
    root: &Path,
    rg: Result<PathBuf, String>,
    report: BackendReport,
) -> (Arc<SearchRuntime>, RecordedLaunches) {
    let backend = RecordingBackend::available(report);
    let launches = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let runtime = SearchRuntime::bind_with_parts(workspace(root), root, rg, Ok(sandbox)).unwrap();
    (runtime, launches)
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
            conversation_id: "search-tool-tests".into(),
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

fn control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

fn bound(
    root: &Path,
    runtime: Option<Arc<SearchRuntime>>,
    calls: &[ToolCallContext],
) -> Arc<dyn Tool> {
    let shared = WorkspaceTools::with_search(
        workspace(root),
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        runtime,
    );
    shared
        .bind_for_test(&[WorkspaceTool::SearchText], reporter(calls))
        .pop()
        .unwrap()
}

fn params(query: &str) -> super::parameters::SearchParams {
    super::parameters::SearchParams {
        path: ".".into(),
        query: query.into(),
        max_matches: None,
        regex: None,
        ignore_case: None,
        glob: None,
        context_lines: None,
        include_ignored: None,
    }
}

fn allowed_arguments() -> Value {
    json!({
        "path": ".",
        "query": "needle",
        "regex": true,
        "ignore_case": true,
        "glob": "*.rs",
        "context_lines": 3,
        "include_ignored": true,
        "max_matches": 10,
    })
}

fn unexecutable_rg(root: &Path) -> PathBuf {
    let directory = root.join("rg-bin");
    std::fs::create_dir_all(&directory).unwrap();
    directory.join("rg")
}

#[test]
fn search_definition_declares_the_sandbox_and_the_new_parameters() {
    let directory = tempfile::tempdir().unwrap();
    let call = call(allowed_arguments());
    let tool = bound(directory.path(), None, std::slice::from_ref(&call));
    let definition = tool.definition();
    assert_eq!(definition.name, "search_text");
    let description = definition.description.to_lowercase();
    for expected in [
        "ripgrep",
        "sandboxed",
        ".gitignore",
        "literal",
        "regex",
        "read-only",
    ] {
        assert!(
            description.contains(expected),
            "description must mention {expected:?}: {}",
            definition.description
        );
    }
    let properties = &definition.parameters["properties"];
    assert_eq!(properties["query"]["maxLength"], 1024);
    assert_eq!(properties["max_matches"]["maximum"], 200);
    assert_eq!(properties["regex"]["type"], "boolean");
    assert_eq!(properties["ignore_case"]["type"], "boolean");
    assert_eq!(properties["include_ignored"]["type"], "boolean");
    assert_eq!(properties["glob"]["maxLength"], 256);
    assert_eq!(properties["context_lines"]["minimum"], 0);
    assert_eq!(properties["context_lines"]["maximum"], 3);
    assert_eq!(definition.parameters["required"], json!(["path", "query"]));
    let hints = tool.hints();
    assert_eq!(hints.read_only, Some(true));
    assert_eq!(hints.destructive, None);
    assert_eq!(hints.idempotent, None);
    assert_eq!(hints.open_world, Some(false));
}

#[tokio::test]
async fn unready_ripgrep_rejects_every_call_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let backend = RecordingBackend::available(full_report());
    let launches = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let runtime = SearchRuntime::bind_with_parts(
        workspace(directory.path()),
        directory.path(),
        Err("managed rg is not installed; run `tars tools install rg`".into()),
        Ok(sandbox),
    )
    .unwrap();
    let error = runtime.check(&params("needle"), &WorkspaceToolLimits::default());
    let Err(SearchCheckError::Rejected(reason)) = error else {
        panic!("an unready ripgrep must reject the call");
    };
    assert!(reason.contains("requires ripgrep"), "{reason}");
    assert!(reason.contains("tars tools install rg"), "{reason}");
    let invocation = call(json!({"path": ".", "query": "needle"}));
    let tool = bound(
        directory.path(),
        Some(runtime),
        std::slice::from_ref(&invocation),
    );
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Rejected);
    assert!(
        output.output.content["error"]
            .as_str()
            .unwrap()
            .contains("requires ripgrep"),
        "{}",
        output.output.content
    );
    assert!(
        launches.lock().unwrap().is_empty(),
        "an unready ripgrep must not launch a process"
    );
}

#[tokio::test]
async fn a_failed_sandbox_probe_rejects_every_call_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let backend = RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = Sandbox::open_with(Box::new(backend)).unwrap_err();
    let runtime = SearchRuntime::bind_with_parts(
        workspace(directory.path()),
        directory.path(),
        Ok(unexecutable_rg(directory.path())),
        Err(error),
    )
    .unwrap();
    let invocation = call(json!({"path": ".", "query": "needle"}));
    let tool = bound(
        directory.path(),
        Some(runtime.clone()),
        std::slice::from_ref(&invocation),
    );
    for _ in 0..2 {
        let output = tool.execute(&invocation, &control(), None).await;
        assert_eq!(output.status, ToolResultStatus::Rejected);
        let reason = output.output.content["error"].as_str().unwrap();
        assert!(reason.contains("requires a platform sandbox"), "{reason}");
        assert!(reason.contains("probe failed"), "{reason}");
    }
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
}

fn report_of_dimension(index: usize, capability: Capability) -> (BackendReport, &'static str) {
    let full = || Capability::Full;
    let (file_read, file_write, network, signal_isolation, descendant_lifecycle, dimension) =
        match index {
            0 => (capability, full(), full(), full(), full(), "file read"),
            1 => (full(), capability, full(), full(), full(), "file write"),
            2 => (full(), full(), capability, full(), full(), "network"),
            3 => (
                full(),
                full(),
                full(),
                capability,
                full(),
                "signal isolation",
            ),
            _ => (
                full(),
                full(),
                full(),
                full(),
                capability,
                "descendant lifecycle",
            ),
        };
    (
        report(
            file_read,
            file_write,
            network,
            signal_isolation,
            descendant_lifecycle,
        ),
        dimension,
    )
}

#[tokio::test]
async fn unenforceable_policy_dimensions_reject_every_call_without_launching() {
    let unsupported = [
        Capability::Unsupported("no read boundary".into()),
        Capability::Unsupported("no write boundary".into()),
        Capability::Unsupported("no network boundary".into()),
        Capability::Unsupported("no signal boundary".into()),
        Capability::Unsupported("no descendant control".into()),
    ];
    let partials = [
        Capability::Partial(Limitation::Other("best effort read".into())),
        Capability::Partial(Limitation::Other("best effort write".into())),
        Capability::Partial(Limitation::ProcessGroupEscape),
        Capability::Partial(Limitation::Other("best effort signals".into())),
        Capability::Partial(Limitation::Other("other descendant limitation".into())),
    ];
    for (index, capability) in unsupported.into_iter().chain(partials).enumerate() {
        let dimension_index = index % 5;
        let (backend_report, dimension) = report_of_dimension(dimension_index, capability);
        let directory = tempfile::tempdir().unwrap();
        let rg = directory.path().join("rg");
        std::fs::write(&rg, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&rg, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let backend = RecordingBackend::available(backend_report);
        let launches = backend.calls();
        let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
        let runtime = SearchRuntime::bind_with_parts(
            workspace(directory.path()),
            directory.path(),
            Ok(rg),
            Ok(sandbox),
        )
        .unwrap();
        let error = runtime.check(&params("needle"), &WorkspaceToolLimits::default());
        match error {
            Err(SearchCheckError::Rejected(reason)) => {
                assert!(
                    reason.contains(dimension),
                    "case {index} must name {dimension:?}: {reason}"
                );
                assert!(
                    reason.contains("not supported") || reason.contains("only partially enforced"),
                    "case {index} must explain why {dimension:?} is unusable: {reason}"
                );
            }
            Err(SearchCheckError::Failed(reason)) => {
                panic!("case {index} ({dimension}) must reject, not fail: {reason}")
            }
            Ok(_) => panic!("case {index} ({dimension}) must reject"),
        }
        assert!(
            launches.lock().unwrap().is_empty(),
            "case {index} ({dimension}) must not launch a process"
        );
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_platform_without_a_backend_names_the_build_platform() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SearchRuntime::bind(workspace(directory.path()), directory.path(), None)
        .expect("binding a search runtime never fails for a missing sandbox");
    let error = runtime
        .check(&params("needle"), &WorkspaceToolLimits::default())
        .unwrap_err();
    let SearchCheckError::Rejected(reason) = error else {
        panic!("a platform without a sandbox backend must reject");
    };
    assert!(
        reason.contains("requires a platform sandbox backend"),
        "{reason}"
    );
    assert!(
        reason.contains(std::env::consts::OS),
        "the error must name the platform: {reason}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn version_control_paths_are_refused_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "url = secret\n").unwrap();
    std::fs::create_dir_all(root.join("sub/.git")).unwrap();
    std::fs::create_dir_all(root.join(".github")).unwrap();
    let rg = unexecutable_rg(directory.path());
    let (runtime, launches) = recording_runtime_with_report(&root, Ok(rg), full_report());
    for path in [".git", ".git/config", "./.git", "sub/.GIT", ".hg"] {
        let invocation = call(json!({"path": path, "query": "secret"}));
        let tool = bound(
            &root,
            Some(runtime.clone()),
            std::slice::from_ref(&invocation),
        );
        let output = tool.execute(&invocation, &control(), None).await;
        assert_eq!(output.status, ToolResultStatus::Rejected, "{path}");
        assert!(
            output.output.content["error"]
                .as_str()
                .is_some_and(|error| error.contains("version-control metadata")),
            "{path}: {}",
            output.output.content
        );
    }
    assert!(
        launches.lock().unwrap().is_empty(),
        "no refused path may launch rg"
    );

    let invocation = call(json!({"path": ".github", "query": "secret"}));
    let tool = bound(&root, Some(runtime), std::slice::from_ref(&invocation));
    tool.execute(&invocation, &control(), None).await;
    assert_eq!(launches.lock().unwrap().len(), 1, ".github is searched");
}

#[cfg(unix)]
#[cfg(unix)]
#[cfg(unix)]
#[tokio::test]
async fn the_launch_uses_the_read_only_policy_and_the_cleared_environment() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let rg = unexecutable_rg(directory.path());
    let (runtime, launches) = recording_runtime_with_report(&root, Ok(rg.clone()), full_report());
    let invocation = call(json!({"path": ".", "query": "needle", "glob": "*.rs"}));
    let tool = bound(&root, Some(runtime), std::slice::from_ref(&invocation));
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Failed);
    let launches = launches.lock().unwrap();
    assert_eq!(launches.len(), 1, "exactly one wrapped launch");
    let (policy, launch) = &launches[0];
    assert_eq!(policy.read_scope(), ReadScope::Roots);
    assert_eq!(policy.network(), NetworkAccess::None);
    let paths = policy_paths(policy);
    let read_roots = paths[0];
    let write_roots = paths[1];
    let workspace = root.canonicalize().unwrap();
    assert!(read_roots.contains(&workspace), "{read_roots:?}");
    assert_eq!(write_roots.len(), 1, "{write_roots:?}");
    assert!(read_roots.contains(&write_roots[0]), "{read_roots:?}");
    assert_ne!(write_roots[0], workspace);
    assert!(
        read_roots.contains(&directory.path().join("rg-bin").canonicalize().unwrap()),
        "the executable's directory must be readable: {read_roots:?}"
    );
    assert!(
        paths[2].is_empty() && paths[3].is_empty() && paths[4].is_empty(),
        "a read-only search must not add git protections or denials"
    );
    let _ = &write_roots;
    assert_eq!(launch_program(launch), rg.as_path());
    assert_eq!(
        launch_working_directory(launch).unwrap(),
        workspace,
        "the search runs at the workspace root"
    );
    let names: Vec<String> = launch_environment(launch)
        .iter()
        .map(|(name, _)| name.to_str().unwrap().to_owned())
        .collect();
    let mut expected = vec![
        "TMPDIR".to_owned(),
        "TERM".to_owned(),
        "NO_COLOR".to_owned(),
    ];
    for name in ["PATH", "HOME", "LANG"] {
        if std::env::var_os(name).is_some() {
            expected.push(name.to_owned());
        }
    }
    expected.sort();
    let mut names = names;
    names.sort();
    assert_eq!(
        names, expected,
        "only the allowed environment variables are set"
    );
    let environment: std::collections::HashMap<_, _> = launch_environment(launch)
        .iter()
        .map(|(name, value)| {
            (
                name.to_str().unwrap().to_owned(),
                value.to_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(environment["TERM"], "dumb");
    assert_eq!(environment["NO_COLOR"], "1");
    assert_eq!(
        Path::new(&environment["TMPDIR"]),
        write_roots[0],
        "TMPDIR must be the private writable directory"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn model_text_only_reaches_the_query_and_glob_positions() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let rg = unexecutable_rg(directory.path());
    let (runtime, launches) = recording_runtime_with_report(&root, Ok(rg), full_report());
    let query = "--pre=/bin/sh -c 'touch pwned'";
    let glob = "-e .*";
    let invocation = call(json!({"path": ".", "query": query, "glob": glob}));
    let tool = bound(&root, Some(runtime), std::slice::from_ref(&invocation));
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Failed);
    assert!(!root.join("pwned").exists());
    let launches = launches.lock().unwrap();
    assert_eq!(launches.len(), 1);
    let argv: Vec<String> = launch_args(&launches[0].1)
        .iter()
        .map(|argument| argument.to_str().unwrap().to_owned())
        .collect();
    let separator = argv.iter().position(|argument| argument == "--").unwrap();
    assert_eq!(argv[separator + 1], query, "the query follows --");
    assert_eq!(
        argv[separator + 2],
        ".",
        "the search path follows the query"
    );
    assert_eq!(
        argv.iter().filter(|argument| *argument == query).count(),
        1,
        "model text must appear exactly once: {argv:?}"
    );
    assert_eq!(
        argv[..separator]
            .iter()
            .filter(|argument| argument.starts_with("--pre"))
            .count(),
        0,
        "a dash-prefixed query must not become an rg option: {argv:?}"
    );
    assert!(argv.contains(&format!("--glob={glob}")));
    assert_eq!(
        argv.iter()
            .filter(|argument| argument.contains("-e .*"))
            .count(),
        1,
        "a dash-prefixed glob must only appear as a glob value: {argv:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn every_fixed_flag_and_limit_is_present_in_the_assembled_arguments() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let rg = unexecutable_rg(directory.path());
    let (runtime, launches) = recording_runtime_with_report(&root, Ok(rg), full_report());
    let invocation = call(json!({"path": ".", "query": "needle"}));
    let tool = bound(&root, Some(runtime), std::slice::from_ref(&invocation));
    let _ = tool.execute(&invocation, &control(), None).await;
    let launches = launches.lock().unwrap();
    let argv: Vec<&str> = launch_args(&launches[0].1)
        .iter()
        .map(|argument| argument.to_str().unwrap())
        .collect();
    let expected_prefix = [
        "--no-config",
        "--json",
        "--stats",
        "--hidden",
        "--no-ignore-global",
        "--no-ignore-parent",
        "--no-require-git",
    ];
    assert_eq!(&argv[..expected_prefix.len()], expected_prefix);
    for expected in [
        "--fixed-strings",
        "--case-sensitive",
        "--max-depth=33",
        "--max-filesize=2097152",
        "--regex-size-limit=1M",
        "--dfa-size-limit=1M",
        "--glob=!.git/",
        "--glob=!.hg/",
        "--glob=!.svn/",
        "--glob=!.jj/",
    ] {
        assert!(argv.contains(&expected), "missing {expected:?}: {argv:?}");
    }
    let exclusions = argv
        .iter()
        .position(|argument| *argument == "--glob=!.git/")
        .unwrap();
    assert!(
        exclusions > 0,
        "the version-control exclusions come from the fixed argument list"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn model_glob_comes_before_the_version_control_exclusions() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, launches) = recording_runtime_with_report(
        directory.path(),
        Ok(directory.path().join("rg")),
        full_report(),
    );
    let invocation =
        call(json!({"path": ".", "query": "needle", "glob": "!.git/**", "include_ignored": true}));
    let tool = bound(
        directory.path(),
        Some(runtime),
        std::slice::from_ref(&invocation),
    );
    let _ = tool.execute(&invocation, &control(), None).await;
    let launches = launches.lock().unwrap();
    let argv: Vec<&str> = launch_args(&launches[0].1)
        .iter()
        .map(|argument| argument.to_str().unwrap())
        .collect();
    let model_glob = argv
        .iter()
        .position(|argument| *argument == "--glob=!.git/**")
        .unwrap();
    let first_exclusion = argv
        .iter()
        .position(|argument| *argument == "--glob=!.git/")
        .unwrap();
    assert!(
        model_glob < first_exclusion,
        "the model glob must be overridden by the fixed exclusions: {argv:?}"
    );
}

#[tokio::test]
async fn invalid_parameter_bounds_are_rejected_before_any_launch() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = SearchRuntime::bind_with_parts(
        workspace(directory.path()),
        directory.path(),
        Err("never used".into()),
        Err(crate::sandbox::test_support::unavailable(
            BackendId::RECORDING,
            "never used",
        )),
    )
    .ok();
    let mut cases: Vec<Value> = vec![
        json!({"path": ".", "query": "needle", "glob": ""}),
        json!({"path": ".", "query": "needle", "glob": "x".repeat(257)}),
        json!({"path": ".", "query": "needle", "glob": "a\u{0}b"}),
        json!({"path": ".", "query": "needle", "context_lines": 4}),
        json!({"path": ".", "query": "needle", "context_lines": -1}),
        json!({"path": ".", "query": "needle", "regex": "yes"}),
        json!({"path": ".", "query": "needle", "ignore_case": 1}),
        json!({"path": ".", "query": "needle", "include_ignored": null}),
        json!({"path": ".", "query": "first\nsecond"}),
        json!({"path": ".", "query": "first\rsecond"}),
        json!({"path": ".", "query": "x".repeat(1025)}),
        json!({"path": ".", "query": "needle", "max_matches": 201}),
        json!({"path": ".", "query": "needle", "extra": true}),
    ];
    cases.push(json!({"path": "/etc", "query": "needle"}));
    cases.push(json!({"path": "../escape", "query": "needle"}));
    for arguments in cases {
        let invocation = call(arguments.clone());
        let tool = bound(
            directory.path(),
            runtime.clone(),
            std::slice::from_ref(&invocation),
        );
        let output = tool.execute(&invocation, &control(), None).await;
        assert_eq!(
            output.status,
            ToolResultStatus::Rejected,
            "{arguments} must be rejected"
        );
    }
}

#[tokio::test]
async fn a_host_without_a_search_runtime_rejects_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let invocation = call(json!({"path": ".", "query": "needle"}));
    let tool = bound(directory.path(), None, std::slice::from_ref(&invocation));
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Rejected);
    assert!(
        output.output.content["error"]
            .as_str()
            .unwrap()
            .contains("search runtime is not available"),
        "{}",
        output.output.content
    );
}

#[cfg(unix)]
#[test]
fn fifo_outside_links_and_missing_paths_never_reach_a_launch() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret"), "needle").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out-link")).unwrap();
    std::fs::create_dir(root.join("inner")).unwrap();
    std::fs::write(root.join("inner/file.txt"), "needle").unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .expect("mkfifo must be available on Unix test hosts");
    assert!(status.success());
    let (runtime, launches) =
        recording_runtime_with_report(&root, Ok(directory.path().join("rg")), full_report());
    let mut outside_path = params("needle");
    outside_path.path = "out-link".into();
    let error = runtime
        .check(&outside_path, &WorkspaceToolLimits::default())
        .unwrap_err();
    let SearchCheckError::Rejected(reason) = error else {
        panic!("an outside link must be rejected");
    };
    assert!(reason.contains("regular file or directory"), "{reason}");

    let mut fifo_path = params("needle");
    fifo_path.path = "pipe".into();
    let error = runtime
        .check(&fifo_path, &WorkspaceToolLimits::default())
        .unwrap_err();
    assert!(matches!(error, SearchCheckError::Rejected(_)));

    let mut missing = params("needle");
    missing.path = "missing".into();
    let error = runtime
        .check(&missing, &WorkspaceToolLimits::default())
        .unwrap_err();
    assert!(matches!(error, SearchCheckError::Failed(_)));

    let mut directory_path = params("needle");
    directory_path.path = "inner".into();
    assert!(
        runtime
            .check(&directory_path, &WorkspaceToolLimits::default())
            .is_ok()
    );

    let mut file_path = params("needle");
    file_path.path = "inner/file.txt".into();
    assert!(
        runtime
            .check(&file_path, &WorkspaceToolLimits::default())
            .is_ok(),
        "a single regular file remains searchable"
    );
    assert!(
        launches.lock().unwrap().is_empty(),
        "path checks must not launch a process"
    );
    // Keep the fixture's permissions normal so the temporary tree is removable.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn read_only_mode_executes_search_without_asking() {
    use crate::permission::{PermissionHub, PermissionMode, ToolPermissionClass};
    // The exhaustive `wm17` matrix already pins `KnownReadOnly` to direct
    // execution in read-only mode; here the bound tool must keep that class.
    let view = PermissionHub::new(PermissionMode::ReadOnly).view();
    assert_eq!(
        view.classify_tool("search_text"),
        Some(ToolPermissionClass::KnownReadOnly)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_queued_search_that_never_starts_is_rejected_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, launches) = recording_runtime_with_report(
        directory.path(),
        Ok(unexecutable_rg(directory.path())),
        full_report(),
    );
    // Hold the session's only search permit, as a running search would.
    let permit = runtime.acquire(&control()).await.unwrap();
    let invocation = call(json!({"path": ".", "query": "needle"}));
    let tool = bound(
        directory.path(),
        Some(runtime.clone()),
        std::slice::from_ref(&invocation),
    );
    let started = Instant::now();
    let deadline = CallControl::new(
        CancellationToken::new(),
        Some(Instant::now() + Duration::from_millis(150)),
    );
    let output = tool.execute(&invocation, &deadline, None).await;
    let elapsed = started.elapsed();
    assert_eq!(output.status, ToolResultStatus::Rejected);
    let content = output.output.content.clone();
    let reason = content["reason"].as_str().unwrap();
    assert!(reason.contains("one at a time"), "{reason}");
    assert!(reason.contains("deadline expired"), "{reason}");
    assert!(reason.contains("no process was started"), "{reason}");
    assert!(
        elapsed >= Duration::from_millis(90),
        "the queued search must wait for its deadline: {elapsed:?}"
    );

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = CallControl::new(cancellation, None);
    let output = tool.execute(&invocation, &cancelled, None).await;
    assert_eq!(output.status, ToolResultStatus::Rejected);
    let reason = output.output.content["reason"].as_str().unwrap();
    assert!(reason.contains("work was stopped"), "{reason}");
    assert!(
        launches.lock().unwrap().is_empty(),
        "a search that never started must not launch a process"
    );
    drop(permit);
}
