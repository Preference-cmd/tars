//! Cross-platform contract tests for the command runtime and its policy.
//!
//! These use the recording backend (no isolation) and the `#[cfg(test)]`
//! backend-injection seam, so they run on Linux, macOS and Windows CI. The real
//! Seatbelt execution tests stay in the macOS-gated module in `command.rs`.

use super::command::{CommandRuntime, bounded_timeout};
use super::parameters::CommandParams;
use crate::definition::{CommandSettings, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::sandbox::test_support::{RecordingBackend, denied_paths};
use crate::sandbox::{
    BackendId, BackendReport, Capability, Limitation, ReadScope, Sandbox, SandboxUnavailable,
};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

fn recording_runtime(root: &Path, storage: Option<std::path::PathBuf>) -> Arc<CommandRuntime> {
    recording_runtime_with_report(root, full_report(), storage)
}

fn recording_runtime_with_report(
    root: &Path,
    report: BackendReport,
    storage: Option<std::path::PathBuf>,
) -> Arc<CommandRuntime> {
    let sandbox = Sandbox::open_with(Box::new(RecordingBackend::available(report))).unwrap();
    CommandRuntime::bind_with_sandbox(root, CommandSettings::default(), Ok(sandbox), storage)
        .unwrap()
}

fn params(command: &str, timeout_secs: Option<u64>) -> CommandParams {
    CommandParams {
        command: command.into(),
        workdir: None,
        timeout_secs,
        outside_sandbox: None,
        justification: None,
    }
}

#[test]
fn bounded_timeout_reserves_cleanup_before_the_work_deadline() {
    let requested = Duration::from_secs(120);
    let deadline = Instant::now() + Duration::from_secs(10);
    let bounded = bounded_timeout(Some(deadline), requested);
    assert!(bounded <= Duration::from_secs(6));
    assert!(bounded >= Duration::from_millis(100));
    assert_eq!(bounded_timeout(None, requested), requested);
}

#[test]
fn a_failed_probe_rejects_every_command_before_a_process_starts() {
    let directory = tempfile::tempdir().unwrap();
    let backend = RecordingBackend::unavailable();
    let launches = backend.calls();
    let error: SandboxUnavailable = Sandbox::open_with(Box::new(backend)).unwrap_err();
    let runtime = CommandRuntime::bind_with_sandbox(
        directory.path(),
        CommandSettings::default(),
        Err(error),
        None,
    )
    .unwrap();
    let error = runtime.check(&params("true", None)).unwrap_err();
    assert!(error.contains("unavailable"), "{error}");
    assert!(error.contains("probe failed"), "{error}");
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
}

#[test]
fn a_policy_the_backend_cannot_enforce_is_rejected_before_a_process_starts() {
    let cases = [
        (
            report(
                Capability::Full,
                Capability::Full,
                Capability::Unsupported("no network boundary".into()),
                Capability::Full,
                Capability::Full,
            ),
            "network",
        ),
        (
            report(
                Capability::Full,
                Capability::Full,
                Capability::Full,
                Capability::Partial(Limitation::Other("partial signals".into())),
                Capability::Full,
            ),
            "signal isolation",
        ),
    ];
    for (index, (report, dimension)) in cases.into_iter().enumerate() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = recording_runtime_with_report(directory.path(), report, None);
        let error = runtime.check(&params("true", None)).unwrap_err();
        assert!(error.contains(dimension), "case {index}: {error}");
        assert!(
            error.contains("not supported") || error.contains("only partially enforced"),
            "case {index}: {error}"
        );
    }
}

#[test]
fn outside_justification_bounds_are_enforced_by_decoding() {
    use serde_json::json;

    let decode = |outside: bool, justification: Option<String>| {
        crate::tools::parameters::Params::decode(
            WorkspaceTool::RunCommand,
            &json!({
                "command": "true",
                "outside_sandbox": outside,
                "justification": justification
            }),
            &WorkspaceToolLimits::default(),
            &WriteToolLimits::default(),
            CommandSettings::default(),
        )
    };
    assert!(decode(true, Some("x".repeat(1024))).is_ok());
    assert!(decode(true, Some("x".repeat(1025))).is_err());
    assert!(decode(true, Some("中".repeat(341))).is_ok());
    assert!(decode(true, Some("中".repeat(342))).is_err());
    assert!(decode(true, Some("  ".into())).is_err());
    assert!(decode(true, Some("a\0b".into())).is_err());
    assert!(decode(true, None).is_err());
    assert!(decode(false, Some("x".repeat(1025))).is_err());
    assert!(decode(false, Some("a\0b".into())).is_err());
}

#[test]
fn an_outside_request_needs_a_justification_before_execution() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = recording_runtime(directory.path(), None);
    let without = CommandParams {
        command: "true".into(),
        workdir: None,
        timeout_secs: None,
        outside_sandbox: Some(true),
        justification: None,
    };
    let error = runtime.check(&without).unwrap_err();
    assert!(error.contains("justification"), "{error}");

    let with = CommandParams {
        justification: Some("the sandbox blocks a needed path".into()),
        ..without
    };
    let prepared = runtime.check(&with).unwrap();
    assert!(prepared.outside_sandbox);
}

#[test]
fn timeout_above_the_session_bound_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = recording_runtime(directory.path(), None);
    let error = runtime.check(&params("true", Some(601))).unwrap_err();
    assert!(error.contains("between 1 and 600"), "{error}");
}

#[test]
fn workdir_parent_escapes_are_rejected_before_execution() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = recording_runtime(directory.path(), None);
    let error = runtime
        .check(&CommandParams {
            command: "pwd".into(),
            workdir: Some("../".into()),
            timeout_secs: None,
            outside_sandbox: None,
            justification: None,
        })
        .unwrap_err();
    assert!(error.contains("bounded relative path"), "{error}");
}

#[test]
fn storage_denial_is_skipped_only_when_it_overlaps_a_writable_root() {
    let root = tempfile::tempdir().unwrap();
    let inside = root.path().join("store/sessions");
    std::fs::create_dir_all(&inside).unwrap();
    let runtime = recording_runtime(root.path(), Some(inside));
    assert!(denied_paths(&runtime.policy().unwrap()).is_empty());

    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join("sessions");
    std::fs::create_dir(&outside).unwrap();
    let runtime = recording_runtime(root.path(), Some(outside.clone()));
    let policy = runtime.policy().unwrap();
    assert_eq!(policy.read_scope(), ReadScope::Host);
    assert_eq!(
        denied_paths(&policy).to_vec(),
        vec![outside.canonicalize().unwrap()]
    );
}

#[test]
fn command_definition_exposes_the_sandbox_and_timeout() {
    let definition = crate::definition::SessionDefinition {
        provider: crate::definition::Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: crate::definition::CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 1,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(std::env::current_dir().unwrap()),
        tools: vec![crate::definition::WorkspaceTool::RunCommand],
        tool_limits: Default::default(),
        write_limits: None,
        command: Some(CommandSettings::default()),
        permission_mode: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
    };
    definition.validate().unwrap();
    assert!(definition.effective_command_settings().is_some());
}
