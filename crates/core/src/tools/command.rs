use super::parameters::CommandParams;
use crate::definition::{CommandNetwork, CommandSettings};
use crate::sandbox::{
    BackendId, Launch, NetworkAccess, ReadScope, Sandbox, SandboxPolicy, SandboxStatus,
    SandboxUnavailable, exit_status_code,
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
    pub(crate) policy: Option<SandboxPolicy>,
    pub(crate) outside_sandbox: bool,
}

/// The result of one command attempt, including bounded output.
#[derive(Debug)]
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
    storage: Option<PathBuf>,
    _temp_dir: tempfile::TempDir,
    gate: Arc<Semaphore>,
    #[cfg(test)]
    check_count: std::sync::atomic::AtomicUsize,
}

impl CommandRuntime {
    pub(crate) fn bind(
        workspace_root: &Path,
        settings: CommandSettings,
        storage: Option<PathBuf>,
    ) -> Result<Arc<Self>, CommandBindError> {
        let root = std::fs::canonicalize(workspace_root).map_err(CommandBindError::Workspace)?;
        let sandbox = Sandbox::open();
        Self::build(root, settings, sandbox, storage)
    }

    #[cfg(test)]
    pub(crate) fn bind_with_sandbox(
        workspace_root: &Path,
        settings: CommandSettings,
        sandbox: Result<Sandbox, SandboxUnavailable>,
        storage: Option<PathBuf>,
    ) -> Result<Arc<Self>, CommandBindError> {
        let root = std::fs::canonicalize(workspace_root).map_err(CommandBindError::Workspace)?;
        Self::build(root, settings, sandbox, storage)
    }

    fn build(
        root: PathBuf,
        settings: CommandSettings,
        sandbox: Result<Sandbox, SandboxUnavailable>,
        storage: Option<PathBuf>,
    ) -> Result<Arc<Self>, CommandBindError> {
        let sandbox = sandbox.map(Arc::new);
        let temp_dir = tempfile::tempdir().map_err(CommandBindError::Temporary)?;
        let temp = std::fs::canonicalize(temp_dir.path()).map_err(CommandBindError::Temporary)?;
        let storage = storage.map(|path| super::execute::project_storage_path(&path));
        Ok(Arc::new(Self {
            settings,
            sandbox,
            root,
            temp,
            storage,
            _temp_dir: temp_dir,
            gate: Arc::new(Semaphore::new(1)),
            #[cfg(test)]
            check_count: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    #[cfg(test)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn check_count(&self) -> usize {
        self.check_count.load(std::sync::atomic::Ordering::SeqCst)
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

    pub(super) fn policy(&self) -> Result<SandboxPolicy, String> {
        let storage_wraps_root = self
            .storage
            .as_ref()
            .is_some_and(|storage| self.root.starts_with(storage));
        let read_roots = vec![self.root.clone(), self.temp.clone()];
        let write_roots = if storage_wraps_root {
            vec![self.temp.clone()]
        } else {
            vec![self.root.clone(), self.temp.clone()]
        };
        let network = match self.settings.network {
            CommandNetwork::None => NetworkAccess::None,
            CommandNetwork::Open => NetworkAccess::Open,
        };
        let mut policy = SandboxPolicy::new(read_roots, write_roots)
            .and_then(|policy| policy.with_git_metadata_protection(&self.root))
            .map_err(|error| error.to_string())?
            .with_read_scope(ReadScope::Host)
            .with_network(network);
        if !storage_wraps_root {
            policy = policy
                .with_protected_read_only_path(&self.root, self.root.join(".tars"))
                .map_err(|error| error.to_string())?;
        }
        // Records of other sessions may contain pasted secrets; an external
        // store stays unreadable unless it overlaps a writable root, while an
        // in-workspace store stays read-only so commands cannot forge records.
        if let Some(storage) = &self.storage {
            if storage.starts_with(&self.root) && storage != &self.root {
                policy = policy
                    .with_protected_read_only_path(&self.root, storage)
                    .map_err(|error| error.to_string())?;
            } else if !overlaps(storage, &self.root) && !overlaps(storage, &self.temp) {
                policy = policy
                    .with_denied_path(storage)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(policy)
    }

    pub(crate) fn check(&self, params: &CommandParams) -> Result<PreparedCommand, String> {
        #[cfg(test)]
        self.check_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let timeout_secs = params
            .timeout_secs
            .unwrap_or(self.settings.default_timeout_secs);
        if timeout_secs == 0 || timeout_secs > self.settings.max_timeout_secs {
            return Err(format!(
                "timeout_secs must be between 1 and {}",
                self.settings.max_timeout_secs
            ));
        }
        let outside_sandbox = params.outside_sandbox == Some(true);
        if outside_sandbox && params.outside_justification().is_none() {
            return Err(
                "outside_sandbox requires a nonempty justification of at most 1 KiB without NUL"
                    .into(),
            );
        }
        let workdir = self.resolve_workdir(params.workdir.as_deref())?;
        let sandbox = self.sandbox.as_ref().map_err(|error| error.to_string())?;
        let policy = if outside_sandbox {
            None
        } else {
            let policy = self.policy()?;
            sandbox
                .report()
                .check(&policy)
                .map_err(|error| error.to_string())?;
            Some(policy)
        };
        Ok(PreparedCommand {
            command: params.command.clone(),
            workdir,
            timeout_secs,
            policy,
            outside_sandbox,
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
        prepared: &PreparedCommand,
        grant: super::lifecycle::ExecutionGrant,
        _permit: OwnedSemaphorePermit,
        control: &CallControl,
        max_output_bytes: usize,
    ) -> CommandExecution {
        if prepared.outside_sandbox && !grant.allow_outside_sandbox {
            return rejected("outside-sandbox execution was not granted");
        }
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
        let wrapped = if prepared.outside_sandbox {
            sandbox.unconfined(launch)
        } else {
            let Some(policy) = &prepared.policy else {
                return rejected("command sandbox policy is missing");
            };
            sandbox.wrap(policy, launch)
        };
        let mut wrapped = match wrapped {
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
        if prepared.outside_sandbox {
            content["outside_sandbox"] = json!(true);
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

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
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
        let runtime = CommandRuntime::bind(root.path(), CommandSettings::default(), None).unwrap();
        (root, runtime)
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
            .run(
                &prepared,
                super::super::lifecycle::ExecutionGrant::default(),
                permit,
                &command_control,
                max_output_bytes,
            )
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
            .join(format!("tars-command-outside-{}", uuid::Uuid::now_v7()));
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

    #[tokio::test]
    async fn workspace_dot_tars_is_read_only_and_non_creatable_in_sandbox() {
        let root = tempfile::tempdir().unwrap();
        let runtime = CommandRuntime::bind(root.path(), CommandSettings::default(), None).unwrap();

        // When `.tars` does not exist yet, creating it (including case variants on
        // macOS) is denied while normal workspace writes succeed.
        let missing_script = r#"set -u
if mkdir .tars 2>/dev/null; then exit 1; fi
if mkdir .TARS 2>/dev/null; then exit 2; fi
if printf x > .tars 2>/dev/null; then exit 3; fi
if printf x > .TARS 2>/dev/null; then exit 4; fi
if mkdir -p .tars/skills/a 2>/dev/null; then exit 5; fi
printf ok > normal.txt || exit 6
echo missing-tars-ok
"#;
        let missing = run(
            &runtime,
            params(missing_script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(missing.status, ToolResultStatus::Succeeded, "{missing:?}");
        assert_eq!(missing.content["exit_code"], 0, "{missing:?}");
        assert!(
            missing.content["output"]
                .as_str()
                .unwrap()
                .contains("missing-tars-ok")
        );
        assert!(!root.path().join(".tars").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("normal.txt")).unwrap(),
            "ok"
        );

        // Once `.tars/skills/...` exists, reading it succeeds while modifying,
        // creating, renaming or deleting `.tars` or its contents fails.
        std::fs::create_dir_all(root.path().join(".tars/skills/alpha")).unwrap();
        std::fs::write(
            root.path().join(".tars/skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: test\n---\nbody\n",
        )
        .unwrap();
        let runtime = CommandRuntime::bind(root.path(), CommandSettings::default(), None).unwrap();
        let existing_script = r#"set -u
if ! grep -q 'name: alpha' .tars/skills/alpha/SKILL.md; then exit 1; fi
if printf pwn > .tars/skills/alpha/SKILL.md 2>/dev/null; then exit 2; fi
if printf pwn > .TARS/skills/alpha/SKILL.md 2>/dev/null; then exit 3; fi
if printf pwn > .tars/new.txt 2>/dev/null; then exit 4; fi
if printf pwn > .TARS/new.txt 2>/dev/null; then exit 5; fi
if mv .tars .tars_moved 2>/dev/null; then exit 6; fi
if rm -rf .tars 2>/dev/null; then exit 7; fi
echo existing-tars-ok
"#;
        let existing = run(
            &runtime,
            params(existing_script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(existing.status, ToolResultStatus::Succeeded, "{existing:?}");
        assert_eq!(existing.content["exit_code"], 0, "{existing:?}");
        assert!(
            existing.content["output"]
                .as_str()
                .unwrap()
                .contains("existing-tars-ok")
        );
        assert!(
            std::fs::read_to_string(root.path().join(".tars/skills/alpha/SKILL.md"))
                .unwrap()
                .contains("name: alpha")
        );
        assert!(!root.path().join(".tars/new.txt").exists());
        assert!(!root.path().join(".tars_moved").exists());
    }

    #[tokio::test]
    async fn in_workspace_store_is_read_only_and_ancestor_locked_in_sandbox() {
        // Direct child store: `root/data`.
        let root_direct = tempfile::tempdir().unwrap();
        let store_direct = root_direct.path().join("data");
        std::fs::create_dir_all(store_direct.join("sessions")).unwrap();
        std::fs::write(store_direct.join("sessions/s1.json"), "{\"ok\":true}\n").unwrap();

        let runtime = CommandRuntime::bind(
            root_direct.path(),
            CommandSettings::default(),
            Some(store_direct.clone()),
        )
        .unwrap();
        let direct_script = r#"set -u
if ! grep -q '"ok":true' data/sessions/s1.json; then exit 1; fi
if printf pwn > data/sessions/s1.json 2>/dev/null; then exit 2; fi
if printf pwn > data/sessions/new.json 2>/dev/null; then exit 3; fi
if mv data data_bak 2>/dev/null; then exit 4; fi
if rm -rf data 2>/dev/null; then exit 5; fi
printf ok > work.txt || exit 6
echo direct-store-ok
"#;
        let direct = run(
            &runtime,
            params(direct_script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(direct.status, ToolResultStatus::Succeeded, "{direct:?}");
        assert_eq!(direct.content["exit_code"], 0, "{direct:?}");
        assert!(
            direct.content["output"]
                .as_str()
                .unwrap()
                .contains("direct-store-ok")
        );
        assert_eq!(
            std::fs::read_to_string(store_direct.join("sessions/s1.json")).unwrap(),
            "{\"ok\":true}\n"
        );
        assert!(!store_direct.join("sessions/new.json").exists());
        assert!(!root_direct.path().join("data_bak").exists());

        // Nested store: `root/nested/data`.
        let root_nested = tempfile::tempdir().unwrap();
        let store_nested = root_nested.path().join("nested/data");
        std::fs::create_dir_all(store_nested.join("sessions")).unwrap();
        std::fs::write(store_nested.join("sessions/s2.json"), "{\"nested\":true}\n").unwrap();

        let runtime = CommandRuntime::bind(
            root_nested.path(),
            CommandSettings::default(),
            Some(store_nested.clone()),
        )
        .unwrap();
        let nested_script = r#"set -u
if ! grep -q '"nested":true' nested/data/sessions/s2.json; then exit 1; fi
if printf pwn > nested/data/sessions/s2.json 2>/dev/null; then exit 2; fi
if printf pwn > nested/data/new.json 2>/dev/null; then exit 3; fi
if mv nested/data nested/data_bak 2>/dev/null; then exit 4; fi
if mv nested nested_bak 2>/dev/null; then exit 5; fi
if rm -rf nested/data 2>/dev/null; then exit 6; fi
printf sibling > nested/sibling.txt || exit 7
echo nested-store-ok
"#;
        let nested = run(
            &runtime,
            params(nested_script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(nested.status, ToolResultStatus::Succeeded, "{nested:?}");
        assert_eq!(nested.content["exit_code"], 0, "{nested:?}");
        assert!(
            nested.content["output"]
                .as_str()
                .unwrap()
                .contains("nested-store-ok")
        );
        assert_eq!(
            std::fs::read_to_string(store_nested.join("sessions/s2.json")).unwrap(),
            "{\"nested\":true}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root_nested.path().join("nested/sibling.txt")).unwrap(),
            "sibling"
        );
        assert!(!root_nested.path().join("nested_bak").exists());
    }

    #[tokio::test]
    async fn storage_equal_or_wrapping_workspace_makes_entire_workspace_read_only() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("existing.txt"), "keep\n").unwrap();
        let runtime = CommandRuntime::bind(
            root.path(),
            CommandSettings::default(),
            Some(root.path().to_path_buf()),
        )
        .unwrap();

        let script = r#"set -u
if ! grep -q keep existing.txt; then exit 1; fi
if printf pwn > existing.txt 2>/dev/null; then exit 2; fi
if printf pwn > new.txt 2>/dev/null; then exit 3; fi
if mkdir subdir 2>/dev/null; then exit 4; fi
echo whole-workspace-ro-ok
"#;
        let result = run(
            &runtime,
            params(script, None),
            Duration::from_secs(5),
            64 * 1024,
        )
        .await;
        assert_eq!(result.status, ToolResultStatus::Succeeded, "{result:?}");
        assert_eq!(result.content["exit_code"], 0, "{result:?}");
        assert!(
            result.content["output"]
                .as_str()
                .unwrap()
                .contains("whole-workspace-ro-ok")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("existing.txt")).unwrap(),
            "keep\n"
        );
        assert!(!root.path().join("new.txt").exists());
        assert!(!root.path().join("subdir").exists());
    }

    #[test]
    fn outside_storage_uses_denied_paths_without_extra_read_only_subpaths() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let outside = base.path().join("store");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();

        let runtime = CommandRuntime::bind(
            &workspace,
            CommandSettings::default(),
            Some(outside.clone()),
        )
        .unwrap();
        let policy = runtime.policy().unwrap();
        let [_, _, read_only_subpaths, _, denied_paths] =
            crate::sandbox::test_support::policy_paths(&policy);
        let canonical_outside = outside.canonicalize().unwrap();
        assert!(denied_paths.contains(&canonical_outside));
        assert!(!read_only_subpaths.contains(&canonical_outside));
    }
}
