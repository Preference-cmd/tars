//! Real macOS Seatbelt acceptance for the bound `run_command` tool (SC04-SC07, SC09).
//!
//! These tests drive the tool, not `CommandRuntime` directly, so the parameter,
//! permission-free execution path and output payload are exercised together.
//! Credential denial and the `setsid` group-escape residual are already proven
//! once in `crate::sandbox::tests`; this module does not repeat them. `HOME`
//! stays the test process's home on purpose: a synthetic HOME would test the
//! Seatbelt helper rather than the tool contract.
//!
//! Each command runs in a private temporary workspace. Network tests use a
//! local listener with bounded timeouts, never an external service.

use super::command::CommandRuntime;
use super::{ToolResources, WorkspaceTools};
use crate::definition::{
    CommandNetwork, CommandSettings, WorkspaceTool, WorkspaceToolLimits, WriteToolLimits,
};
use crate::sandbox::Sandbox;
use crate::sandbox::test_support::RecordingBackend;
use crate::session::{ToolReporter, WorkRef};
use crate::workspace::Workspace;
use causa::kernel::{
    BlockId, CallControl, InvocationId, RoundId, Tool, ToolBatch, ToolCallContext, ToolCallPayload,
    ToolResultPayload, ToolResultStatus, TurnId,
};
use serde_json::json;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn settings(network: CommandNetwork) -> CommandSettings {
    CommandSettings {
        network,
        ..CommandSettings::default()
    }
}

fn bound_tool(root: &Path, network: CommandNetwork, calls: &[ToolCallContext]) -> Arc<dyn Tool> {
    let runtime = CommandRuntime::bind(root, settings(network))
        .expect("the macOS Seatbelt backend must probe successfully");
    let workspace = Workspace::open(&root.canonicalize().unwrap()).unwrap();
    let shared = WorkspaceTools::with_command(
        workspace,
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        settings(network),
        Some(runtime),
    );
    shared
        .bind_for_test(&[WorkspaceTool::RunCommand], reporter(calls))
        .pop()
        .unwrap()
}

fn call(arguments: serde_json::Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new("run_command", arguments),
    )
}

fn reporter(calls: &[ToolCallContext]) -> ToolReporter {
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "command-acceptance".into(),
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

async fn execute(
    tool: &Arc<dyn Tool>,
    call: &ToolCallContext,
    control: &CallControl,
) -> ToolResultPayload {
    tool.execute(call, control, None).await
}

async fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read_pid(path: &Path) -> String {
    let pid = std::fs::read_to_string(path)
        .expect("child pid file")
        .trim()
        .to_owned();
    assert!(
        !pid.is_empty() && pid.chars().all(|character| character.is_ascii_digit()),
        "child pid file is not a numeric pid: {pid:?}"
    );
    pid
}

fn alive(pid: &str) -> bool {
    Command::new("/bin/kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Wait for a command child to disappear; a survivor is killed and fails.
async fn wait_dead(pid: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        if Instant::now() > deadline {
            let _ = Command::new("/bin/kill").args(["-9", pid]).status();
            panic!("command child {pid} survived cleanup");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const LONG_COMMAND: &str = r#"echo partial
trap '' TERM
/bin/bash -c 'trap "" TERM; while :; do sleep 1; done' &
echo $! > child.pid
wait
"#;

#[tokio::test]
async fn file_boundaries_hold_through_run_command() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("workspace");
    let outside = base.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    for name in ["abs.txt", "rel.txt", "escape.txt"] {
        std::fs::write(outside.join(name), "untouched\n").unwrap();
    }
    std::os::unix::fs::symlink(&outside, root.join("out-link")).unwrap();

    let abs = outside.join("abs.txt");
    let script = format!(
        r#"set -u
printf one > inside.txt || exit 60
printf two >> inside.txt || exit 61
mv inside.txt renamed.txt || exit 62
rm renamed.txt || exit 63
mkdir sub || exit 64
rmdir sub || exit 65
if printf pwn > '{abs}' 2>/dev/null; then echo abs-write-allowed; exit 66; fi
if printf pwn >> '{abs}' 2>/dev/null; then echo abs-append-allowed; exit 67; fi
if printf pwn > ../outside/rel.txt 2>/dev/null; then echo parent-write-allowed; exit 68; fi
if rm ../outside/rel.txt 2>/dev/null; then echo parent-delete-allowed; exit 69; fi
if mv ../outside/escape.txt ../outside/moved.txt 2>/dev/null; then echo parent-rename-allowed; exit 70; fi
if printf pwn > out-link/escape.txt 2>/dev/null; then echo symlink-write-allowed; exit 71; fi
if printf x > .git/hooks/x 2>/dev/null; then echo git-write-allowed; exit 72; fi
if mkdir .git 2>/dev/null; then echo git-created; exit 73; fi
echo boundaries-ok
"#,
        abs = abs.display()
    );
    let before_git = call(json!({ "command": script, "timeout_secs": 10 }));
    let hook_write = call(json!({
        "command": "printf x > .git/hooks/x",
        "timeout_secs": 10
    }));
    let tool = bound_tool(
        &root,
        CommandNetwork::None,
        &[before_git.clone(), hook_write.clone()],
    );

    let result = execute(&tool, &before_git, &control(Duration::from_secs(20))).await;
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    assert_eq!(result.output.content["exit_code"], 0, "{result:?}");
    assert!(
        result.output.content["output"]
            .as_str()
            .unwrap()
            .contains("boundaries-ok")
    );
    assert!(!root.join(".git").exists(), "the sandbox created .git");
    for name in ["abs.txt", "rel.txt", "escape.txt"] {
        assert_eq!(
            std::fs::read_to_string(outside.join(name)).unwrap(),
            "untouched\n",
            "{name} was changed through the command tool"
        );
    }
    assert!(!outside.join("moved.txt").exists());

    // Once `.git` exists, writing a hook is still denied. Creating the fixture
    // from the test process keeps this a write-policy test, not a mkdir test.
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    let hooks = execute(&tool, &hook_write, &control(Duration::from_secs(10))).await;
    assert_eq!(hooks.status, ToolResultStatus::Succeeded);
    assert_ne!(
        hooks.output.content["exit_code"], 0,
        "a .git hook write was allowed"
    );
    assert!(!root.join(".git/hooks/x").exists());
}

struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<bool>>,
}

impl Listener {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        );
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return false,
                }
            }
            false
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.take().unwrap().join().unwrap()
    }
}

#[tokio::test]
async fn network_none_denies_loopback_and_dns_while_open_allows_loopback() {
    assert!(
        Path::new("/usr/bin/curl").exists(),
        "the macOS acceptance suite requires /usr/bin/curl"
    );
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("workspace");
    std::fs::create_dir(&root).unwrap();

    let closed_listener = Listener::start();
    let closed_calls = [
        call(json!({
            "command": format!(
                "/usr/bin/curl -sS --max-time 2 http://127.0.0.1:{}/",
                closed_listener.port
            ),
            "timeout_secs": 10
        })),
        call(json!({
            "command": "/usr/bin/curl -sS --max-time 2 http://example.com/",
            "timeout_secs": 10
        })),
    ];
    let closed_tool = bound_tool(&root, CommandNetwork::None, &closed_calls);
    let denied = execute(
        &closed_tool,
        &closed_calls[0],
        &control(Duration::from_secs(20)),
    )
    .await;
    assert_eq!(denied.status, ToolResultStatus::Succeeded);
    assert_ne!(
        denied.output.content["exit_code"], 0,
        "loopback was reachable under network = none: {}",
        denied.output.content["output"]
    );
    assert!(
        !closed_listener.finish(),
        "a loopback connection reached the listener under network = none"
    );

    let dns = execute(
        &closed_tool,
        &closed_calls[1],
        &control(Duration::from_secs(20)),
    )
    .await;
    assert_ne!(
        dns.output.content["exit_code"], 0,
        "DNS resolved under network = none"
    );
    assert!(
        dns.output.content["output"]
            .as_str()
            .unwrap()
            .contains("Could not resolve host"),
        "DNS failure was not reported as resolution failure: {}",
        dns.output.content["output"]
    );

    let open_listener = Listener::start();
    let open_call = call(json!({
        "command": format!(
            "/usr/bin/curl -sS --max-time 2 http://127.0.0.1:{}/",
            open_listener.port
        ),
        "timeout_secs": 10
    }));
    let open_tool = bound_tool(
        &root,
        CommandNetwork::Open,
        std::slice::from_ref(&open_call),
    );
    let allowed = execute(&open_tool, &open_call, &control(Duration::from_secs(20))).await;
    assert_eq!(
        allowed.output.content["exit_code"], 0,
        "loopback was denied under network = open: {}",
        allowed.output.content["output"]
    );
    assert!(
        allowed.output.content["output"]
            .as_str()
            .unwrap()
            .contains("ok")
    );
    assert!(
        open_listener.finish(),
        "the open-network listener did not accept a connection"
    );
}

#[tokio::test]
async fn signals_reach_only_the_command_sandbox() {
    let base = tempfile::tempdir().unwrap();
    let host = std::process::id();
    let script = format!(
        r#"if kill -0 {host} 2>/dev/null; then echo host-signal-allowed; exit 70; fi
/bin/sleep 2 & p=$!
if ! kill -0 "$p" 2>/dev/null; then echo own-signal-failed; exit 71; fi
kill -TERM "$p" 2>/dev/null
wait "$p"; status=$?
if [ "$status" -ne 143 ]; then echo "own-term-status=$status"; exit 72; fi
echo signal-ok
"#
    );
    let invocation = call(json!({ "command": script, "timeout_secs": 10 }));
    let tool = bound_tool(
        base.path(),
        CommandNetwork::None,
        std::slice::from_ref(&invocation),
    );
    let result = execute(&tool, &invocation, &control(Duration::from_secs(20))).await;
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    assert_eq!(result.output.content["exit_code"], 0, "{result:?}");
    assert!(
        result.output.content["output"]
            .as_str()
            .unwrap()
            .contains("signal-ok")
    );
}

#[tokio::test]
async fn timeout_kills_the_command_group_and_keeps_partial_output() {
    let base = tempfile::tempdir().unwrap();
    let invocation = call(json!({ "command": LONG_COMMAND, "timeout_secs": 1 }));
    let tool = bound_tool(
        base.path(),
        CommandNetwork::None,
        std::slice::from_ref(&invocation),
    );
    let started = Instant::now();
    let result = execute(&tool, &invocation, &control(Duration::from_secs(20))).await;
    assert_eq!(result.status, ToolResultStatus::TimedOut);
    assert_eq!(result.output.content["may_have_partially_executed"], true);
    assert!(
        result.output.content["output"]
            .as_str()
            .unwrap()
            .contains("partial")
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let child = read_pid(&base.path().join("child.pid"));
    wait_dead(&child).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_kills_the_command_group() {
    let base = tempfile::tempdir().unwrap();
    let invocation = call(json!({ "command": LONG_COMMAND, "timeout_secs": 30 }));
    let tool = bound_tool(
        base.path(),
        CommandNetwork::None,
        std::slice::from_ref(&invocation),
    );
    let cancellation = CancellationToken::new();
    let call_control = CallControl::new(
        cancellation.clone(),
        Some(Instant::now() + Duration::from_secs(20)),
    );
    let tool_for_task = tool.clone();
    let call_for_task = invocation.clone();
    let task =
        tokio::spawn(async move { execute(&tool_for_task, &call_for_task, &call_control).await });

    wait_for_file(&base.path().join("child.pid")).await;
    let child = read_pid(&base.path().join("child.pid"));
    cancellation.cancel();
    let result = task.await.unwrap();
    assert_eq!(result.status, ToolResultStatus::Cancelled);
    assert_eq!(result.output.content["may_have_partially_executed"], true);
    wait_dead(&child).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_command_future_kills_the_group() {
    let base = tempfile::tempdir().unwrap();
    let invocation = call(json!({ "command": LONG_COMMAND, "timeout_secs": 30 }));
    let tool = bound_tool(
        base.path(),
        CommandNetwork::None,
        std::slice::from_ref(&invocation),
    );
    let call_control = control(Duration::from_secs(20));
    let tool_for_task = tool.clone();
    let call_for_task = invocation.clone();
    let task =
        tokio::spawn(async move { execute(&tool_for_task, &call_for_task, &call_control).await });

    wait_for_file(&base.path().join("child.pid")).await;
    let child = read_pid(&base.path().join("child.pid"));
    task.abort();
    assert!(
        matches!(task.await, Err(error) if error.is_cancelled()),
        "the aborted command task must be cancelled"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    wait_dead(&child).await;
}

#[tokio::test]
async fn a_failed_probe_rejects_every_tool_call_without_launching() {
    let directory = tempfile::tempdir().unwrap();
    let backend = RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = Sandbox::open_with(Box::new(backend)).unwrap_err();
    let runtime =
        CommandRuntime::bind_with_sandbox(directory.path(), CommandSettings::default(), Err(error))
            .unwrap();
    let workspace = Workspace::open(&directory.path().canonicalize().unwrap()).unwrap();
    let shared = WorkspaceTools::with_command(
        workspace,
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(16),
        CommandSettings::default(),
        Some(runtime),
    );
    let calls = vec![
        call(json!({"command": "printf first > first.txt"})),
        call(json!({"command": "printf second > second.txt"})),
    ];
    let report = reporter(&calls);
    let tool = shared
        .bind_for_test(&[WorkspaceTool::RunCommand], report.clone())
        .pop()
        .unwrap();
    let call_control = CallControl::new(CancellationToken::new(), None);
    for invocation in &calls {
        let output = tool.execute(invocation, &call_control, None).await;
        assert_eq!(output.status, ToolResultStatus::Rejected);
        let reason = output.output.content["error"].as_str().unwrap_or_default();
        assert!(reason.contains("unavailable"), "{reason}");
        assert!(reason.contains("probe failed"), "{reason}");
    }
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
    assert!(!directory.path().join("first.txt").exists());
    assert!(!directory.path().join("second.txt").exists());
    report.drain().await;
}
