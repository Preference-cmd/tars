use super::parameters::CommandParams;
use crate::definition::{CommandNetwork, CommandSettings};
use crate::sandbox::{
    BackendId, Launch, NetworkAccess, Sandbox, SandboxPolicy, SandboxStatus, SandboxUnavailable,
    exit_status_code,
};
use crate::workspace::Workspace;
use causa::kernel::{CallControl, ToolResultStatus};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant as TokioInstant;

const STOP_GRACE: Duration = Duration::from_secs(1);
const READER_WAIT: Duration = Duration::from_secs(2);
const CLEANUP_RESERVE: Duration = Duration::from_secs(4);
const RESULT_RESERVE: usize = 4096;

/// Keep the command's own timeout earlier than the session's work deadline so
/// its TERM/KILL and reader cleanup finish before the executor drops it.
pub(crate) fn bounded_timeout(
    deadline: Option<std::time::Instant>,
    requested: Duration,
) -> Duration {
    let Some(deadline) = deadline else {
        return requested.max(Duration::from_millis(100));
    };
    let remaining = deadline
        .saturating_duration_since(std::time::Instant::now())
        .saturating_sub(CLEANUP_RESERVE);
    requested.min(remaining).max(Duration::from_millis(100))
}

/// Why binding the command tool failed before a session could open.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandBindError {
    /// The workspace root could not be resolved.
    #[error("could not resolve the command workspace: {0}")]
    Workspace(std::io::Error),
    /// The per-session temporary directory could not be created.
    #[error("could not create the command temporary directory: {0}")]
    Temporary(std::io::Error),
}

/// A validated command ready for execution.
#[derive(Debug)]
pub(crate) struct PreparedCommand {
    pub(crate) command: String,
    pub(crate) workdir: PathBuf,
    pub(crate) timeout_secs: u64,
    pub(crate) policy: SandboxPolicy,
}

/// The result of one command attempt, including bounded output.
pub(crate) struct CommandExecution {
    pub(crate) status: ToolResultStatus,
    pub(crate) content: Value,
}

/// Session-owned command resources: one probe, one private temp directory and
/// one command permit.
pub(crate) struct CommandRuntime {
    settings: CommandSettings,
    sandbox: Result<Arc<Sandbox>, SandboxUnavailable>,
    root: PathBuf,
    temp: PathBuf,
    _temp_dir: tempfile::TempDir,
    gate: Arc<Semaphore>,
}

impl CommandRuntime {
    pub(crate) fn bind(
        workspace_root: &Path,
        settings: CommandSettings,
    ) -> Result<Arc<Self>, CommandBindError> {
        let root = std::fs::canonicalize(workspace_root).map_err(CommandBindError::Workspace)?;
        let sandbox = Sandbox::open();
        Self::build(root, settings, sandbox)
    }

    #[cfg(test)]
    pub(crate) fn bind_with_sandbox(
        workspace_root: &Path,
        settings: CommandSettings,
        sandbox: Result<Sandbox, SandboxUnavailable>,
    ) -> Result<Arc<Self>, CommandBindError> {
        let root = std::fs::canonicalize(workspace_root).map_err(CommandBindError::Workspace)?;
        Self::build(root, settings, sandbox)
    }

    fn build(
        root: PathBuf,
        settings: CommandSettings,
        sandbox: Result<Sandbox, SandboxUnavailable>,
    ) -> Result<Arc<Self>, CommandBindError> {
        let sandbox = sandbox.map(Arc::new);
        let temp_dir = tempfile::tempdir().map_err(CommandBindError::Temporary)?;
        let temp = std::fs::canonicalize(temp_dir.path()).map_err(CommandBindError::Temporary)?;
        Ok(Arc::new(Self {
            settings,
            sandbox,
            root,
            temp,
            _temp_dir: temp_dir,
            gate: Arc::new(Semaphore::new(1)),
        }))
    }

    /// Why this platform cannot host a command runtime, if it has no backend.
    pub(crate) fn unavailable_platform(&self) -> Option<String> {
        self.sandbox
            .as_ref()
            .err()
            .filter(|error| error.backend == BackendId::UNAVAILABLE)
            .map(|error| {
                format!(
                    "the command tool requires a platform sandbox backend; {} has none: {error}",
                    std::env::consts::OS
                )
            })
    }

    /// The probed sandbox captured when this runtime was bound.
    pub(crate) fn sandbox_status(&self) -> SandboxStatus {
        match &self.sandbox {
            Ok(sandbox) => SandboxStatus::Available(sandbox.report().clone()),
            Err(error) => SandboxStatus::Unavailable(error.clone()),
        }
    }

    fn policy(&self) -> Result<SandboxPolicy, String> {
        let roots = [self.root.clone(), self.temp.clone()];
        let network = match self.settings.network {
            CommandNetwork::None => NetworkAccess::None,
            CommandNetwork::Open => NetworkAccess::Open,
        };
        SandboxPolicy::new(roots.to_vec(), roots.to_vec())
            .and_then(|policy| policy.with_read_only_subpath(self.root.join(".git")))
            .map(|policy| policy.with_network(network))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn check(&self, params: &CommandParams) -> Result<PreparedCommand, String> {
        let timeout_secs = params
            .timeout_secs
            .unwrap_or(self.settings.default_timeout_secs);
        if timeout_secs == 0 || timeout_secs > self.settings.max_timeout_secs {
            return Err(format!(
                "timeout_secs must be between 1 and {}",
                self.settings.max_timeout_secs
            ));
        }
        let workdir = self.resolve_workdir(params.workdir.as_deref())?;
        let sandbox = self.sandbox.as_ref().map_err(|error| error.to_string())?;
        let policy = self.policy()?;
        sandbox
            .report()
            .check(&policy)
            .map_err(|error| error.to_string())?;
        Ok(PreparedCommand {
            command: params.command.clone(),
            workdir,
            timeout_secs,
            policy,
        })
    }

    fn resolve_workdir(&self, workdir: Option<&str>) -> Result<PathBuf, String> {
        let components = match workdir {
            None | Some("") => Vec::new(),
            Some(path) => Workspace::components(path).map_err(|error| error.to_string())?,
        };
        let candidate = components
            .iter()
            .fold(self.root.clone(), |path, name| path.join(name));
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|error| format!("command working directory is unavailable: {error}"))?;
        if !canonical.starts_with(&self.root) || !canonical.is_dir() {
            return Err("command working directory escapes the workspace".into());
        }
        Ok(canonical)
    }

    pub(crate) async fn acquire(&self, control: &CallControl) -> Option<OwnedSemaphorePermit> {
        let deadline = control.deadline().map(TokioInstant::from);
        tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => None,
            _ = sleep_until(deadline) => None,
            permit = self.gate.clone().acquire_owned() => permit.ok(),
        }
    }

    pub(crate) async fn run(
        &self,
        prepared: PreparedCommand,
        _permit: OwnedSemaphorePermit,
        control: &CallControl,
        max_output_bytes: usize,
    ) -> CommandExecution {
        let sandbox = match &self.sandbox {
            Ok(sandbox) => sandbox,
            Err(error) => return rejected(error.to_string()),
        };
        let mut launch = Launch::new("/bin/bash")
            .arg("-c")
            .arg(&prepared.command)
            .current_dir(&prepared.workdir);
        for name in ["PATH", "HOME", "LANG"] {
            if let Some(value) = std::env::var_os(name) {
                launch = launch.env(name, value);
            }
        }
        launch = launch
            .env("TMPDIR", self.temp.as_os_str())
            .env("TERM", "dumb")
            .env("NO_COLOR", "1");
        let mut wrapped = match sandbox.wrap(&prepared.policy, launch) {
            Ok(command) => command,
            Err(error) => return rejected(error.to_string()),
        };
        let (reader, writer) = match std::io::pipe() {
            Ok(pair) => pair,
            Err(error) => return failed(format!("could not create command output pipe: {error}")),
        };
        let stderr = match writer.try_clone() {
            Ok(writer) => writer,
            Err(error) => return failed(format!("could not clone command output pipe: {error}")),
        };
        wrapped
            .stdin(Stdio::null())
            .stdout(Stdio::from(writer))
            .stderr(Stdio::from(stderr));
        let mut tree = match wrapped.spawn() {
            Ok(tree) => tree,
            Err(error) => return failed(format!("could not start command: {error}")),
        };
        let capture = Arc::new(Mutex::new(OutputCapture::new(max_output_bytes)));
        let reader_capture = capture.clone();
        let reader_task = tokio::task::spawn_blocking(move || {
            let mut reader = reader;
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => reader_capture.lock().unwrap().push(&buffer[..count]),
                    Err(_) => break,
                }
            }
        });
        let started = Instant::now();
        let deadline = control.deadline().map(TokioInstant::from);
        let mut stopped = None;
        let mut status = None;
        let mut wait_error = None;
        tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => {
                stopped = Some(if control.is_cancelled() {
                    CommandStop::Cancelled
                } else {
                    CommandStop::TimedOut
                });
            }
            _ = sleep_until(deadline) => {
                stopped = Some(if control.is_cancelled() {
                    CommandStop::Cancelled
                } else {
                    CommandStop::TimedOut
                });
            }
            result = tree.wait() => {
                match result {
                    Ok(value) => status = Some(value),
                    Err(error) => wait_error = Some(error.to_string()),
                }
            }
        }
        if stopped.is_some() {
            status = tree.terminate(STOP_GRACE).await.ok();
        }
        // The pipe closes only when the direct child and its group exit; an
        // escaped descendant can hold it open. The capture stays bounded and
        // the detached reader is a documented residual.
        let _ = tokio::time::timeout(READER_WAIT, reader_task).await;
        if let Some(error) = wait_error {
            return failed(format!("command wait failed: {error}"));
        }
        let wall_time_ms = started.elapsed().as_millis() as u64;
        let (output, truncated, omitted_bytes) = capture.lock().unwrap().finish();
        let (exit_code, signal) = status.map_or((None, None), exit_status_code);
        let mut content = json!({
            "exit_code": exit_code,
            "output": output,
            "truncated": truncated,
            "omitted_bytes": omitted_bytes,
            "wall_time_ms": wall_time_ms,
        });
        if let Some(signal) = signal {
            content["signal"] = json!(signal);
        }
        let status = match stopped {
            Some(stop) => {
                content["may_have_partially_executed"] = json!(true);
                match stop {
                    CommandStop::TimedOut => ToolResultStatus::TimedOut,
                    CommandStop::Cancelled => ToolResultStatus::Cancelled,
                }
            }
            None => ToolResultStatus::Succeeded,
        };
        fit_command_result(&mut content, max_output_bytes);
        CommandExecution { status, content }
    }
}

#[derive(Clone, Copy)]
enum CommandStop {
    TimedOut,
    Cancelled,
}

fn fit_command_result(content: &mut Value, max_output_bytes: usize) {
    if serde_json::to_vec(content).is_ok_and(|bytes| bytes.len() <= max_output_bytes) {
        return;
    }
    let original = content["output"].as_str().unwrap_or_default().to_owned();
    let mut end = original.len().min(max_output_bytes / 2);
    while !original.is_char_boundary(end) {
        end -= 1;
    }
    content["output"] = json!(&original[..end]);
    content["truncated"] = json!(true);
    let omitted = content["omitted_bytes"].as_u64().unwrap_or(0)
        + (original.len().saturating_sub(end)) as u64;
    content["omitted_bytes"] = json!(omitted);
    if serde_json::to_vec(content).is_ok_and(|bytes| bytes.len() > max_output_bytes) {
        content["output"] = json!("(output omitted: result limit)");
    }
}

fn rejected(message: impl Into<String>) -> CommandExecution {
    CommandExecution {
        status: ToolResultStatus::Rejected,
        content: json!({ "error": message.into() }),
    }
}

fn failed(message: impl Into<String>) -> CommandExecution {
    CommandExecution {
        status: ToolResultStatus::Failed,
        content: json!({ "error": message.into() }),
    }
}

async fn sleep_until(deadline: Option<TokioInstant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

struct OutputCapture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
    head_limit: usize,
    tail_limit: usize,
}

impl OutputCapture {
    fn new(max_output_bytes: usize) -> Self {
        let budget = max_output_bytes.saturating_sub(RESULT_RESERVE).max(1024);
        let head_limit = (16 * 1024).min(budget);
        let tail_limit = budget - head_limit;
        Self {
            head: Vec::with_capacity(head_limit),
            tail: VecDeque::with_capacity(tail_limit),
            total: 0,
            head_limit,
            tail_limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len();
        let mut rest = bytes;
        if self.head.len() < self.head_limit {
            let take = (self.head_limit - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if !rest.is_empty() && self.tail_limit > 0 {
            self.tail.extend(rest);
            while self.tail.len() > self.tail_limit {
                self.tail.pop_front();
            }
        }
    }

    fn finish(&self) -> (String, bool, usize) {
        let omitted = self.total.saturating_sub(self.head.len() + self.tail.len());
        let mut bytes = self.head.clone();
        bytes.extend(self.tail.iter().copied());
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        text.retain(|character| matches!(character, '\n' | '\t') || !character.is_control());
        if text.is_empty() {
            return ("(no output)".to_owned(), omitted > 0, omitted);
        }
        let budget = self.head_limit.saturating_add(self.tail_limit).max(1024);
        if text.len() > budget {
            let mut end = budget;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            return (text, true, omitted);
        }
        (text, omitted > 0, omitted)
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::definition::CommandSettings;
    use causa::kernel::{CallControl, ToolResultStatus};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    fn runtime() -> (tempfile::TempDir, Arc<CommandRuntime>) {
        let root = tempfile::tempdir().unwrap();
        let runtime = CommandRuntime::bind(root.path(), CommandSettings::default()).unwrap();
        (root, runtime)
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
    fn a_failed_probe_rejects_every_command() {
        let directory = tempfile::tempdir().unwrap();
        let error = Sandbox::open_with(Box::new(
            crate::sandbox::test_support::RecordingBackend::unavailable(),
        ))
        .unwrap_err();
        let runtime = CommandRuntime::bind_with_sandbox(
            directory.path(),
            CommandSettings::default(),
            Err(error),
        )
        .unwrap();
        let error = runtime.check(&params("true", None)).unwrap_err();
        assert!(error.contains("unavailable"), "{error}");
    }

    fn params(command: &str, timeout_secs: Option<u64>) -> CommandParams {
        CommandParams {
            command: command.into(),
            workdir: None,
            timeout_secs,
        }
    }

    fn control(timeout: Duration) -> CallControl {
        CallControl::new(CancellationToken::new(), Some(Instant::now() + timeout))
    }

    async fn run(
        runtime: &Arc<CommandRuntime>,
        params: CommandParams,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> CommandExecution {
        let prepared = runtime.check(&params).unwrap();
        let outer = control(Duration::from_secs(20));
        let command_control = outer.with_timeout(timeout);
        let permit = runtime.acquire(&command_control).await.unwrap();
        runtime
            .run(prepared, permit, &command_control, max_output_bytes)
            .await
    }

    #[tokio::test]
    async fn merges_streams_and_reports_exit() {
        let (_root, runtime) = runtime();
        let result = run(
            &runtime,
            params("printf out; printf err >&2; printf out2", None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Succeeded);
        let output = result.content["output"].as_str().unwrap();
        assert_eq!(output, "outerrout2");
        assert_eq!(result.content["exit_code"], 0);
        assert_eq!(result.content["truncated"], false);
    }

    #[tokio::test]
    async fn keeps_head_and_tail_and_reports_omitted_bytes() {
        let (_root, runtime) = runtime();
        let script = "i=0; while [ $i -lt 12000 ]; do printf '0123456789'; i=$((i+1)); done";
        let result = run(
            &runtime,
            params(script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Succeeded);
        assert_eq!(result.content["truncated"], true);
        assert!(result.content["omitted_bytes"].as_u64().unwrap() > 0);
        let output = result.content["output"].as_str().unwrap();
        assert!(output.starts_with("0123456789"));
        assert!(output.ends_with("0123456789"));
        assert!(serde_json::to_vec(&result.content).unwrap().len() <= 64 * 1024);
    }

    #[tokio::test]
    async fn escaped_output_fits_the_result_budget_without_losing_metadata() {
        let (_root, runtime) = runtime();
        let script = "i=0; while [ $i -lt 2000 ]; do printf '\\\"'; i=$((i+1)); done";
        let result = run(&runtime, params(script, None), Duration::from_secs(5), 1024).await;
        assert_eq!(result.status, ToolResultStatus::Succeeded);
        assert_eq!(result.content["exit_code"], 0);
        assert_eq!(result.content["truncated"], true);
        assert!(
            serde_json::to_vec(&result.content).unwrap().len() <= 1024,
            "serialized command result exceeds the bound"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_and_signal_are_still_succeeded() {
        let (_root, runtime) = runtime();
        let nonzero = run(
            &runtime,
            params("exit 7", None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(nonzero.status, ToolResultStatus::Succeeded);
        assert_eq!(nonzero.content["exit_code"], 7);

        let signal = run(
            &runtime,
            params("kill -TERM $$", None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(signal.status, ToolResultStatus::Succeeded);
        assert_eq!(signal.content["exit_code"], 143);
        assert_eq!(signal.content["signal"], 15);
    }

    #[tokio::test]
    async fn empty_non_utf8_and_control_output_is_normalized() {
        let (_root, runtime) = runtime();
        let empty = run(
            &runtime,
            params("true", None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(empty.content["output"], "(no output)");

        let weird = run(
            &runtime,
            params("printf '\\377\\001ok\\n'", None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        let output = weird.content["output"].as_str().unwrap();
        assert!(output.contains("ok"));
        assert!(!output.chars().any(|c| c == '\u{1}'));
        assert!(output.contains('\u{fffd}'));
    }

    #[tokio::test]
    async fn timeout_marks_timed_out_and_stops_the_command() {
        let (_root, runtime) = runtime();
        let started = Instant::now();
        let result = run(
            &runtime,
            params("sleep 5", Some(1)),
            Duration::from_secs(1),
            64 * 1024,
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::TimedOut);
        assert_eq!(result.content["may_have_partially_executed"], true);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn outside_writes_are_rejected_by_the_sandbox() {
        let (root, runtime) = runtime();
        let outside = root
            .path()
            .parent()
            .unwrap()
            .join(format!("tarz-command-outside-{}", uuid::Uuid::now_v7()));
        let script = format!("printf x > '{}'", outside.display());
        let result = run(
            &runtime,
            params(&script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Succeeded);
        assert_ne!(result.content["exit_code"], 0);
        assert!(!outside.exists());
    }

    #[test]
    fn timeout_above_the_session_bound_is_rejected() {
        let (_root, runtime) = runtime();
        let error = runtime.check(&params("true", Some(601))).unwrap_err();
        assert!(error.contains("between 1 and 600"), "{error}");
    }

    #[test]
    fn workdir_parent_escapes_are_rejected_before_execution() {
        let (_root, runtime) = runtime();
        let error = runtime
            .check(&CommandParams {
                command: "pwd".into(),
                workdir: Some("../".into()),
                timeout_secs: None,
            })
            .unwrap_err();
        assert!(error.contains("bounded relative path"));
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
        };
        definition.validate().unwrap();
        assert!(definition.effective_command_settings().is_some());
    }
}
