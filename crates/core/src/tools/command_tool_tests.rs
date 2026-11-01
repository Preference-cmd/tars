//! Cross-platform tests for the bound `run_command` tool.
//!
//! These bind the recording backend, which performs no isolation and records
//! the launches it receives, so they observe the tool contract, the policy it
//! sends to the backend and whether a process launch was attempted. They run
//! on Linux, macOS and Windows CI; the real Seatbelt acceptance stays in
//! `command_acceptance_tests` and only runs on macOS.
//!
//! Windows has no workspace tools, so a command-only test binds the shared
//! tool set over a detached workspace handle: the command path resolves its own
//! workspace root and never touches the file tools.

use super::command::CommandRuntime;
use super::{ToolResources, WorkspaceTools};
use crate::definition::{CommandSettings, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::sandbox::{
    BackendId, BackendReport, Capability, Launch, Limitation, NetworkAccess, ReadScope, Sandbox,
    SandboxPolicy,
};
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, Tool, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(unix)]
use tars_sandbox::test_support::launch_environment;
use tars_sandbox::test_support::{RecordingBackend, policy_paths};
use tokio_util::sync::CancellationToken;

type RecordedLaunches = Arc<Mutex<Vec<(SandboxPolicy, Launch)>>>;
type RecordedUnconfined = Arc<Mutex<Vec<Launch>>>;

fn call(arguments: Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new("run_command", arguments),
    )
}

fn reporter(calls: &[ToolCallContext]) -> ToolReporter {
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "command-tool-tests".into(),
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

fn command_test_workspace(root: &Path) -> Arc<Workspace> {
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

fn bound_tool(
    root: &Path,
    runtime: Arc<CommandRuntime>,
    calls: &[ToolCallContext],
) -> Arc<dyn Tool> {
    let shared = WorkspaceTools::with_command(
        command_test_workspace(root),
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        CommandSettings::default(),
        Some(runtime),
    );
    shared
        .bind_for_test(&[WorkspaceTool::RunCommand], reporter(calls))
        .pop()
        .unwrap()
}

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

fn recording_runtime_with(
    root: &Path,
    report: BackendReport,
    storage: Option<PathBuf>,
) -> (Arc<CommandRuntime>, RecordedLaunches, RecordedUnconfined) {
    let backend = RecordingBackend::available(report);
    let launches = backend.calls();
    let unconfined = backend.unconfined_calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let runtime =
        CommandRuntime::bind_with_sandbox(root, CommandSettings::default(), Ok(sandbox), storage)
            .unwrap();
    (runtime, launches, unconfined)
}

fn recording_runtime(root: &Path) -> (Arc<CommandRuntime>, RecordedLaunches, RecordedUnconfined) {
    recording_runtime_with(root, full_report(), None)
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

#[test]
fn run_command_definition_declares_the_sandbox_and_the_default_network() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _launches, _unconfined) = recording_runtime(dir.path());
    let call = call(json!({"command": "true"}));
    let tool = bound_tool(dir.path(), runtime, std::slice::from_ref(&call));
    let definition = tool.definition();
    assert_eq!(definition.name, "run_command");
    assert!(
        definition.description.contains("sandboxed workspace"),
        "{}",
        definition.description
    );
    assert!(
        definition
            .description
            .contains("network is disabled unless the session enables it"),
        "{}",
        definition.description
    );
    assert_eq!(
        definition.parameters["properties"]["command"]["maxLength"],
        16384
    );
    assert_eq!(
        definition.parameters["properties"]["timeout_secs"]["maximum"],
        600
    );
    assert_eq!(
        definition.parameters["properties"]["outside_sandbox"]["type"],
        "boolean"
    );
    assert_eq!(
        definition.parameters["properties"]["justification"]["maxLength"],
        1024
    );
    assert!(definition.description.contains("outside_sandbox"));
    let hints = tool.hints();
    assert_eq!(hints.read_only, Some(false));
    assert_eq!(hints.destructive, Some(true));
    assert_eq!(hints.idempotent, Some(false));
}

#[tokio::test]
async fn outside_command_without_a_grant_is_rejected_without_launching() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, launches, unconfined) = recording_runtime(dir.path());
    let invocation = call(json!({
        "command": "printf ran > ran.txt",
        "outside_sandbox": true,
        "justification": "needs to write outside the workspace"
    }));
    let tool = bound_tool(dir.path(), runtime, std::slice::from_ref(&invocation));
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Rejected);
    assert!(
        output.output.content["error"]
            .as_str()
            .unwrap()
            .contains("not granted"),
        "{}",
        output.output.content
    );
    assert!(
        launches.lock().unwrap().is_empty(),
        "a bypassed outside call must not be wrapped"
    );
    assert!(
        unconfined.lock().unwrap().is_empty(),
        "a bypassed outside call must not launch unconfined"
    );
    assert!(!dir.path().join("ran.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn granted_outside_command_uses_the_unconfined_launch() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, launches, unconfined) = recording_runtime(dir.path());
    let invocation = call(json!({
        "command": "printf ran > ran.txt",
        "outside_sandbox": true,
        "justification": "needs to write outside the workspace"
    }));
    let shared = WorkspaceTools::with_command(
        command_test_workspace(dir.path()),
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        CommandSettings::default(),
        Some(runtime),
    );
    shared
        .outside_sandbox_grants()
        .grant(invocation.call_block_id);
    let tool = shared
        .bind_for_test(
            &[WorkspaceTool::RunCommand],
            reporter(std::slice::from_ref(&invocation)),
        )
        .pop()
        .unwrap();
    let output = tool.execute(&invocation, &control(), None).await;
    assert_eq!(output.status, ToolResultStatus::Succeeded);
    assert_eq!(output.output.content["exit_code"], 0);
    assert_eq!(output.output.content["outside_sandbox"], true);
    assert!(
        launches.lock().unwrap().is_empty(),
        "a granted outside call must not be wrapped"
    );
    let unconfined = unconfined.lock().unwrap();
    assert_eq!(unconfined.len(), 1);
    for (name, _) in launch_environment(&unconfined[0]) {
        assert!(
            ["PATH", "HOME", "LANG", "TMPDIR", "TERM", "NO_COLOR"]
                .contains(&name.to_str().unwrap()),
            "unexpected environment variable {name:?}"
        );
    }
    drop(unconfined);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ran.txt")).unwrap(),
        "ran"
    );
}

#[tokio::test]
async fn queued_command_that_never_starts_is_rejected_without_launching() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, launches, _unconfined) = recording_runtime(dir.path());
    // Hold the session's only command permit, as a running command would, so
    // this call has to queue and then time out before it can start.
    let permit = runtime.acquire(&control()).await.unwrap();
    let invocation = call(json!({"command": "printf ran > ran.txt", "timeout_secs": 1}));
    let tool = bound_tool(
        dir.path(),
        runtime.clone(),
        std::slice::from_ref(&invocation),
    );
    let started = Instant::now();
    let output = tool.execute(&invocation, &control(), None).await;
    let elapsed = started.elapsed();
    assert_eq!(output.status, ToolResultStatus::Rejected);
    assert!(
        elapsed >= Duration::from_millis(900),
        "queued command must wait for its deadline, not reject immediately: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "queued command must return near its own deadline: {elapsed:?}"
    );
    let content = output.output.content.clone();
    assert!(
        content["error"].as_str().unwrap().contains("not started"),
        "{content}"
    );
    let reason = content["reason"].as_str().unwrap();
    assert!(reason.contains("one at a time"), "{reason}");
    assert!(
        reason.contains("waiting for the previous command"),
        "{reason}"
    );
    assert!(reason.contains("deadline expired"), "{reason}");
    assert!(
        content.get("may_have_partially_executed").is_none(),
        "{content}"
    );
    assert!(
        launches.lock().unwrap().is_empty(),
        "a queued command that never started must not launch a process"
    );
    assert!(!dir.path().join("ran.txt").exists());
    drop(permit);
}

#[tokio::test]
async fn cancelled_queued_command_is_rejected_without_launching() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, launches, _unconfined) = recording_runtime(dir.path());
    // Hold the session's only permit so acquire cannot succeed; the
    // pre-cancelled token then takes the biased cancellation branch.
    let permit = runtime.acquire(&control()).await.unwrap();
    let invocation = call(json!({"command": "printf ran > ran.txt"}));
    let tool = bound_tool(
        dir.path(),
        runtime.clone(),
        std::slice::from_ref(&invocation),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let cancelled = CallControl::new(cancellation, None);
    let output = tool.execute(&invocation, &cancelled, None).await;
    assert_eq!(output.status, ToolResultStatus::Rejected);
    let content = output.output.content.clone();
    assert!(
        content["error"].as_str().unwrap().contains("not started"),
        "{content}"
    );
    let reason = content["reason"].as_str().unwrap();
    assert!(reason.contains("work was stopped"), "{reason}");
    assert!(
        content.get("may_have_partially_executed").is_none(),
        "{content}"
    );
    assert!(
        launches.lock().unwrap().is_empty(),
        "a queued command stopped by cancellation must not launch a process"
    );
    assert!(!dir.path().join("ran.txt").exists());
    drop(permit);
}

#[tokio::test]
async fn a_failed_probe_rejects_every_tool_call_without_launching() {
    let dir = tempfile::tempdir().unwrap();
    let backend = RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = Sandbox::open_with(Box::new(backend)).unwrap_err();
    let runtime =
        CommandRuntime::bind_with_sandbox(dir.path(), CommandSettings::default(), Err(error), None)
            .unwrap();
    let outside = call(json!({
        "command": "printf third > third.txt",
        "outside_sandbox": true,
        "justification": "needs to write outside the workspace"
    }));
    let shared = WorkspaceTools::with_command(
        command_test_workspace(dir.path()),
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        CommandSettings::default(),
        Some(runtime),
    );
    shared.outside_sandbox_grants().grant(outside.call_block_id);
    let calls = vec![
        call(json!({"command": "printf first > first.txt"})),
        call(json!({"command": "printf second > second.txt"})),
        outside,
    ];
    let tool = shared
        .bind_for_test(&[WorkspaceTool::RunCommand], reporter(&calls))
        .pop()
        .unwrap();
    for invocation in &calls {
        let output = tool.execute(invocation, &control(), None).await;
        assert_eq!(output.status, ToolResultStatus::Rejected);
        let reason = output.output.content["error"].as_str().unwrap_or_default();
        assert!(reason.contains("unavailable"), "{reason}");
        assert!(reason.contains("probe failed"), "{reason}");
    }
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
    assert!(!dir.path().join("first.txt").exists());
    assert!(!dir.path().join("second.txt").exists());
    assert!(!dir.path().join("third.txt").exists());
}

/// Each case is a capability report the backend produced and the dimension the
/// tool must name when it rejects the call.
fn unenforceable_reports() -> Vec<(BackendReport, &'static str)> {
    let mut cases = Vec::new();
    let unsupported = [
        Capability::Unsupported("no read boundary".into()),
        Capability::Unsupported("no write boundary".into()),
        Capability::Unsupported("no network boundary".into()),
        Capability::Unsupported("no signal boundary".into()),
        Capability::Unsupported("no descendant control".into()),
    ];
    for (index, capability) in unsupported.into_iter().enumerate() {
        let (report, dimension) = report_of_dimension(index, capability);
        cases.push((report, dimension));
    }
    // Only the descendant-lifecycle dimension accepts a partial capability, and
    // only for the documented `setsid` limitation. Any other partial dimension
    // rejects, including the same limitation reported elsewhere.
    let partials = [
        Capability::Partial(Limitation::Other("best effort read".into())),
        Capability::Partial(Limitation::Other("best effort write".into())),
        Capability::Partial(Limitation::ProcessGroupEscape),
        Capability::Partial(Limitation::Other("best effort signals".into())),
        Capability::Partial(Limitation::Other("best effort descendants".into())),
    ];
    for (index, capability) in partials.into_iter().enumerate() {
        let (report, dimension) = report_of_dimension(index, capability);
        cases.push((report, dimension));
    }
    cases
}

#[tokio::test]
async fn unenforceable_dimensions_reject_every_call_without_launching() {
    for (index, (report, dimension)) in unenforceable_reports().into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let backend = RecordingBackend::available(report);
        let launches = backend.calls();
        let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
        let runtime = CommandRuntime::bind_with_sandbox(
            dir.path(),
            CommandSettings::default(),
            Ok(sandbox),
            None,
        )
        .unwrap();
        let invocation = call(json!({"command": "printf ran > ran.txt"}));
        let tool = bound_tool(dir.path(), runtime, std::slice::from_ref(&invocation));
        let output = tool.execute(&invocation, &control(), None).await;
        assert_eq!(
            output.status,
            ToolResultStatus::Rejected,
            "case {index} ({dimension}) must reject"
        );
        let reason = output.output.content["error"].as_str().unwrap_or_default();
        assert!(
            reason.contains(dimension),
            "case {index} must name {dimension:?}: {reason}"
        );
        assert!(
            reason.contains("not supported") || reason.contains("only partially enforced"),
            "case {index} must explain why {dimension:?} is unusable: {reason}"
        );
        assert!(
            launches.lock().unwrap().is_empty(),
            "case {index} ({dimension}) must not launch a process"
        );
        assert!(!dir.path().join("ran.txt").exists());
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn a_platform_without_a_backend_names_the_build_platform() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = CommandRuntime::bind(dir.path(), CommandSettings::default(), None).unwrap();
    let status = runtime.sandbox_status();
    assert!(!status.is_available(), "{status}");
    let reason = runtime
        .unavailable_platform()
        .expect("a platform without a sandbox backend must report one");
    assert!(
        reason.contains("requires a platform sandbox backend"),
        "{reason}"
    );
    assert!(
        reason.contains(std::env::consts::OS),
        "the error must name the platform: {reason}"
    );
}

/// Resolve the longest existing ancestor and re-append the missing components,
/// the same projection the policy applies to a path whose leaf does not exist.
fn canonical_projection(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut projected = canonical;
            for name in tail.iter().rev() {
                projected.push(name);
            }
            return projected;
        }
        let Some(name) = current.file_name() else {
            return current;
        };
        tail.push(name.to_os_string());
        current.pop();
    }
}

#[test]
fn the_generated_policy_holds_canonical_absolute_paths_and_no_backend_vocabulary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    let storage = dir.path().join("store/sessions");
    std::fs::create_dir_all(&storage).unwrap();
    let (runtime, _launches, _unconfined) =
        recording_runtime_with(&root, full_report(), Some(storage.clone()));
    let policy = runtime.policy().unwrap();
    assert_eq!(policy.read_scope(), ReadScope::Host);
    assert_eq!(policy.network(), NetworkAccess::None);

    let mut seen = 0;
    for paths in policy_paths(&policy) {
        for path in paths {
            seen += 1;
            assert!(path.is_absolute(), "policy path is not absolute: {path:?}");
            assert!(
                path.components().all(|component| matches!(
                    component,
                    // Windows canonicalization returns a `\\?\C:\...` verbatim
                    // path; its drive/UNC prefix is not a relative component.
                    // Only `.` and `..` are relative components.
                    Component::Prefix(_) | Component::RootDir | Component::Normal(_)
                )),
                "policy path contains a relative component: {path:?}"
            );
            assert_eq!(
                canonical_projection(path),
                *path,
                "policy path is not a canonical absolute path: {path:?}"
            );
        }
    }
    assert!(seen >= 4, "the policy must hold its read and write roots");
    assert!(
        policy_paths(&policy)[4].contains(&storage.canonicalize().unwrap()),
        "the storage denial must stay in the policy"
    );

    let rendered = format!("{policy:?}").to_lowercase();
    for vocabulary in [
        "sbpl",
        "sandbox-exec",
        "seatbelt",
        "bwrap",
        "mxc",
        "job object",
        "(allow",
        "(deny",
    ] {
        assert!(
            !rendered.contains(vocabulary),
            "the policy must not name backend vocabulary {vocabulary:?}: {rendered}"
        );
    }
}
