//! Sandboxed ripgrep search runtime.
//!
//! `search_text` runs the resolved `rg` executable inside the platform sandbox
//! instead of walking the workspace in-process. The runtime is bound once per
//! session: it probes the sandbox, records the external-tool check, and keeps
//! one private temporary directory and one search permit. A failed probe or an
//! unready `rg` leaves the runtime bound but rejects every call before a
//! process starts, so a saved session or preset stays open when the search
//! environment is missing.
//!
//! TARS assembles every argument. Model text reaches `rg` only after `--` in the
//! query and inside `--glob=` values. The sandbox policy reads the workspace,
//! the private temporary directory and the executable's own directories, and
//! writes only inside the private temporary directory; network access is
//! denied, so global and parent ignore files outside the workspace cannot be
//! read. Ignore behavior therefore depends only on the workspace contents.

use super::parameters::SearchParams;
use crate::definition::WorkspaceToolLimits;
use crate::external_tools::ExternalTools;
use crate::sandbox::{
    BackendId, Launch, NetworkAccess, ReadScope, Sandbox, SandboxPolicy, SandboxUnavailable,
    exit_status_code,
};
use crate::workspace::Workspace;
#[cfg(unix)]
use crate::workspace::WorkspaceError;
use causa::kernel::{CallControl, ToolResultStatus};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant as TokioInstant;

/// Grace period between the TERM and KILL of a stopped search process tree.
const STOP_GRACE: Duration = Duration::from_secs(1);
/// Result bytes reserved for the tool envelope around a search result.
const RESULT_RESERVE: usize = 1024;
/// At most this many `rg` stderr observations become issues.
const MAX_ISSUES: usize = 32;
/// Longest stderr line kept as an issue reason.
const MAX_ISSUE_BYTES: usize = 512;
/// Longest context line kept in a result.
const MAX_CONTEXT_BYTES: usize = 1024;

/// Arguments every search passes, in this order.
const FIXED_ARGS: &[&str] = &[
    "--no-config",
    "--json",
    "--stats",
    "--hidden",
    "--no-ignore-global",
    "--no-ignore-parent",
    "--no-require-git",
];

/// Exclusions kept for every search; they come last so a model glob cannot
/// re-include a version-control directory.
const VCS_EXCLUDES: &[&str] = &["!.git/", "!.hg/", "!.svn/", "!.jj/"];

/// Why binding the search runtime failed before a session could open.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SearchBindError {
    /// The workspace root could not be resolved.
    #[error("could not resolve the search workspace: {0}")]
    Workspace(std::io::Error),
    /// The per-session temporary directory could not be created.
    #[error("could not create the search temporary directory: {0}")]
    Temporary(std::io::Error),
}

/// Why one search call was rejected before a process started.
#[derive(Debug)]
pub(crate) enum SearchCheckError {
    /// The call cannot be attempted as requested.
    Rejected(String),
    /// The requested path cannot be used because of an I/O failure.
    Failed(String),
}

/// A validated search ready for execution.
#[derive(Debug)]
pub(crate) struct PreparedSearch {
    path: String,
    context_lines: usize,
    match_limit: usize,
    args: Vec<OsString>,
    policy: SandboxPolicy,
}

/// The result of one search attempt.
pub(crate) struct SearchExecution {
    pub(crate) status: ToolResultStatus,
    pub(crate) content: Value,
}

/// Session-owned search resources: one probe, one resolved executable, one
/// private temporary directory and one search permit.
pub(crate) struct SearchRuntime {
    #[cfg_attr(not(unix), allow(dead_code))]
    workspace: Arc<Workspace>,
    rg: Result<PathBuf, String>,
    sandbox: Result<Arc<Sandbox>, SandboxUnavailable>,
    root: PathBuf,
    temp: PathBuf,
    _temp_dir: tempfile::TempDir,
    gate: Arc<Semaphore>,
}

impl SearchRuntime {
    /// Bind the runtime for one session.
    ///
    /// `tools` is the process-wide external-tool check. A host that resolved no
    /// tools (the TUI until its startup check lands) binds the runtime with an
    /// unconfigured state, and every call is then rejected by name.
    pub(crate) fn bind(
        workspace: Arc<Workspace>,
        root: &Path,
        tools: Option<&ExternalTools>,
    ) -> Result<Arc<Self>, SearchBindError> {
        let root = std::fs::canonicalize(root).map_err(SearchBindError::Workspace)?;
        let rg = resolve_rg(tools);
        let sandbox = Sandbox::open().map(Arc::new);
        Self::build(workspace, root, rg, sandbox)
    }

    /// Bind the runtime with an injected sandbox probe result.
    ///
    /// The executable still comes from the external-tool check, mirroring
    /// `CommandRuntime::bind_with_sandbox`; production code cannot choose a
    /// backend or a probe outcome.
    #[cfg(test)]
    pub(crate) fn bind_with_sandbox(
        workspace: Arc<Workspace>,
        root: &Path,
        tools: Option<&ExternalTools>,
        sandbox: Result<Sandbox, SandboxUnavailable>,
    ) -> Result<Arc<Self>, SearchBindError> {
        let root = std::fs::canonicalize(root).map_err(SearchBindError::Workspace)?;
        let rg = resolve_rg(tools);
        Self::build(workspace, root, rg, sandbox.map(Arc::new))
    }

    /// Bind the runtime with an injected sandbox probe and tool path.
    ///
    /// This is the `#[cfg(test)]` seam of the recording backend; production
    /// code cannot choose a backend or a resolved executable.
    #[cfg(test)]
    pub(crate) fn bind_with_parts(
        workspace: Arc<Workspace>,
        root: &Path,
        rg: Result<PathBuf, String>,
        sandbox: Result<Sandbox, SandboxUnavailable>,
    ) -> Result<Arc<Self>, SearchBindError> {
        let root = std::fs::canonicalize(root).map_err(SearchBindError::Workspace)?;
        Self::build(workspace, root, rg, sandbox.map(Arc::new))
    }

    fn build(
        workspace: Arc<Workspace>,
        root: PathBuf,
        rg: Result<PathBuf, String>,
        sandbox: Result<Arc<Sandbox>, SandboxUnavailable>,
    ) -> Result<Arc<Self>, SearchBindError> {
        let temp_dir = tempfile::tempdir().map_err(SearchBindError::Temporary)?;
        let temp = std::fs::canonicalize(temp_dir.path()).map_err(SearchBindError::Temporary)?;
        Ok(Arc::new(Self {
            workspace,
            rg,
            sandbox,
            root,
            temp,
            _temp_dir: temp_dir,
            gate: Arc::new(Semaphore::new(1)),
        }))
    }

    /// The session search permit. Searches run one at a time so a session's
    /// workspace access stays predictable.
    pub(crate) async fn acquire(&self, control: &CallControl) -> Option<OwnedSemaphorePermit> {
        let deadline = control.deadline().map(TokioInstant::from);
        tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => None,
            _ = sleep_until(deadline) => None,
            permit = self.gate.clone().acquire_owned() => permit.ok(),
        }
    }

    fn available(&self) -> Result<(&Sandbox, &Path), String> {
        let sandbox = match &self.sandbox {
            Ok(sandbox) => sandbox.as_ref(),
            Err(error) if error.backend == BackendId::UNAVAILABLE => {
                return Err(format!(
                    "the search tool requires a platform sandbox backend; {} has none: {error}",
                    std::env::consts::OS
                ));
            }
            Err(error) => {
                return Err(format!(
                    "the search tool requires a platform sandbox: {error}"
                ));
            }
        };
        let rg = self
            .rg
            .as_deref()
            .map_err(|reason| format!("the search tool requires ripgrep: {reason}"))?;
        Ok((sandbox, rg))
    }

    /// Check one call and assemble its launch request.
    pub(crate) fn check(
        &self,
        params: &SearchParams,
        limits: &WorkspaceToolLimits,
    ) -> Result<PreparedSearch, SearchCheckError> {
        let (sandbox, rg) = self.available().map_err(SearchCheckError::Rejected)?;
        // Reject an unenforceable policy before touching the workspace; the
        // path check still runs before any process starts.
        let policy = self.policy(rg).map_err(SearchCheckError::Rejected)?;
        sandbox
            .report()
            .check(&policy)
            .map_err(|error| SearchCheckError::Rejected(error.to_string()))?;
        let path = self.validate_path(&params.path)?;
        let args = search_args(params, &path, limits);
        Ok(PreparedSearch {
            path,
            context_lines: params.context_lines.unwrap_or(0) as usize,
            match_limit: params.max_matches.unwrap_or(limits.max_search_matches),
            args,
            policy,
        })
    }

    /// Open `path` through the workspace handle and require a directory or a
    /// regular file. The checked path is not re-resolved here; a path replaced
    /// by a FIFO after the check is bounded by the call deadline instead.
    fn validate_path(&self, path: &str) -> Result<String, SearchCheckError> {
        let normalized = Workspace::components(path)
            .map_err(|error| SearchCheckError::Rejected(error.to_string()))?;
        let path = if normalized.is_empty() {
            ".".to_owned()
        } else {
            normalized.join("/")
        };
        #[cfg(unix)]
        {
            match self.workspace.directory(&path) {
                Ok(_) => Ok(path),
                Err(directory_error) => match self.workspace.file(&path) {
                    Ok(_) => Ok(path),
                    Err(file_error) => Err(invalid_path_error(&path, directory_error, file_error)),
                },
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(SearchCheckError::Failed(
                "search paths require Unix handle-relative filesystem access".into(),
            ))
        }
    }

    /// Build the read-only policy for one search.
    ///
    /// The workspace, the private temporary directory and the executable's own
    /// directories are readable; only the private temporary directory is
    /// writable; network access is denied and the backend's credential denials
    /// stay in place. The executable's parent is a policy read root rather than
    /// a sandbox toolchain baseline entry: where TARS installs external tools is
    /// an external-tools fact, and the sandbox package must not learn it.
    fn policy(&self, rg: &Path) -> Result<SandboxPolicy, String> {
        let mut roots = vec![self.root.clone(), self.temp.clone()];
        if let Some(parent) = rg.parent() {
            roots.push(parent.to_path_buf());
        }
        if let Ok(resolved) = std::fs::canonicalize(rg)
            && let Some(parent) = resolved.parent()
        {
            roots.push(parent.to_path_buf());
        }
        SandboxPolicy::new(roots, [self.temp.clone()])
            .map(|policy| {
                policy
                    .with_read_scope(ReadScope::Roots)
                    .with_network(NetworkAccess::None)
            })
            .map_err(|error| error.to_string())
    }

    /// Run one prepared search.
    pub(crate) async fn run(
        &self,
        prepared: PreparedSearch,
        _permit: OwnedSemaphorePermit,
        control: &CallControl,
        limits: &WorkspaceToolLimits,
    ) -> SearchExecution {
        let (sandbox, rg) = match self.available() {
            Ok(parts) => parts,
            Err(reason) => return rejected(reason),
        };
        let context_lines = prepared.context_lines;
        let match_limit = prepared.match_limit;
        let mut launch = Launch::new(rg).current_dir(&self.root).args(prepared.args);
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
        wrapped
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut tree = match wrapped.spawn() {
            Ok(tree) => tree,
            Err(error) => return failed(format!("could not start ripgrep: {error}")),
        };
        let Some(stdout) = tree.take_stdout() else {
            return failed("ripgrep stdout is unavailable");
        };
        let Some(stderr) = tree.take_stderr() else {
            return failed("ripgrep stderr is unavailable");
        };
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut capture = Capture::new(prepared.path, context_lines, match_limit, limits);
        let deadline = control.deadline().map(TokioInstant::from);
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut exited: Option<std::io::Result<std::process::ExitStatus>> = None;
        let mut stopped: Option<SearchStop> = None;
        loop {
            if stdout_done && stderr_done && exited.is_some() {
                break;
            }
            tokio::select! {
                biased;
                _ = control.cancellation_token().cancelled() => {
                    stopped = Some(SearchStop::Cancelled);
                    break;
                }
                _ = sleep_until(deadline) => {
                    stopped = Some(SearchStop::TimedOut);
                    break;
                }
                line = stdout_lines.next_line(), if !stdout_done => {
                    match line {
                        Ok(Some(line)) => {
                            if let Some(limit) = capture.push(&line) {
                                stopped = Some(SearchStop::Limit(limit));
                                break;
                            }
                        }
                        _ => stdout_done = true,
                    }
                }
                line = stderr_lines.next_line(), if !stderr_done => {
                    match line {
                        Ok(Some(line)) => capture.push_stderr(&line),
                        _ => stderr_done = true,
                    }
                }
                result = tree.wait(), if exited.is_none() => {
                    exited = Some(result);
                }
            }
        }
        let terminated = stopped.is_some();
        if terminated {
            let _ = tree.terminate(STOP_GRACE).await;
        }
        let (exit_code, signal) = match exited {
            Some(Ok(status)) => exit_status_code(status),
            _ => (None, None),
        };
        let (mut content, stats, complete, issues) = capture.finish();
        let status = match stopped {
            Some(SearchStop::Limit(SearchLimit::MaxMatches | SearchLimit::OutputBytes)) => {
                ToolResultStatus::Succeeded
            }
            Some(SearchStop::TimedOut) => ToolResultStatus::TimedOut,
            Some(SearchStop::Cancelled) => ToolResultStatus::Cancelled,
            None => match exit_code {
                Some(0) | Some(1) => ToolResultStatus::Succeeded,
                Some(2) if matches_empty(&content) => {
                    content = json!({ "error": stderr_message(&content) });
                    ToolResultStatus::Rejected
                }
                Some(2) => ToolResultStatus::Succeeded,
                Some(code) => {
                    content = json!({
                        "error": match signal {
                            Some(signal) => format!("ripgrep was terminated by signal {signal}"),
                            None => format!("ripgrep exited with status {code}"),
                        }
                    });
                    ToolResultStatus::Failed
                }
                None => {
                    content = json!({ "error": "ripgrep did not report a status" });
                    ToolResultStatus::Failed
                }
            },
        };
        if matches!(
            status,
            ToolResultStatus::Succeeded | ToolResultStatus::TimedOut | ToolResultStatus::Cancelled
        ) {
            // A stopped run keeps what it collected but is not a complete scan.
            if stopped.is_some() {
                content["complete"] = json!(false);
            }
            let normal = !terminated && matches!(exit_code, Some(0) | Some(1)) && complete;
            content["scanned_files"] = match (normal, stats) {
                (true, Some(stats)) => json!(stats.files),
                _ => Value::Null,
            };
            content["scanned_bytes"] = match (normal, stats) {
                (true, Some(stats)) => json!(stats.bytes),
                _ => Value::Null,
            };
            // `scanned_entries` is always null: ripgrep never reports directory
            // entries. `skipped` counts the bounded `rg` stderr observations
            // that became issues (files it could not read), not every ignored
            // file, which ripgrep does not report at all.
            content["scanned_entries"] = Value::Null;
            content["skipped"] = json!(issues);
        }
        SearchExecution { status, content }
    }
}

fn resolve_rg(tools: Option<&ExternalTools>) -> Result<PathBuf, String> {
    let Some(tools) = tools else {
        return Err("this host did not resolve the external tools at startup".into());
    };
    let status = tools.rg();
    match status.ready_path() {
        Some(path) => Ok(path.to_path_buf()),
        None => Err(status
            .unavailable_reason()
            .unwrap_or("the startup check did not report a reason")
            .to_owned()),
    }
}

#[cfg(unix)]
fn invalid_path_error(
    path: &str,
    directory_error: WorkspaceError,
    file_error: WorkspaceError,
) -> SearchCheckError {
    match (&directory_error, &file_error) {
        (WorkspaceError::Forbidden, _)
        | (_, WorkspaceError::Forbidden)
        | (WorkspaceError::InvalidPath, _)
        | (_, WorkspaceError::InvalidPath) => SearchCheckError::Rejected(format!(
            "search path '{path}' must be a regular file or directory inside the workspace"
        )),
        _ => SearchCheckError::Failed(format!("search path is unavailable: {directory_error}")),
    }
}

fn matches_empty(content: &Value) -> bool {
    content["matches"]
        .as_array()
        .is_none_or(|matches| matches.is_empty())
}

fn stderr_message(content: &Value) -> String {
    let mut lines: Vec<String> = content["issues"]
        .as_array()
        .map(|issues| {
            issues
                .iter()
                .filter_map(|issue| {
                    let path = issue["path"].as_str().unwrap_or_default();
                    let reason = issue["reason"].as_str().unwrap_or_default();
                    if path.is_empty() {
                        (!reason.is_empty()).then(|| reason.to_owned())
                    } else {
                        Some(format!("{path}: {reason}"))
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        return "ripgrep exited with status 2".into();
    }
    let mut message = String::new();
    for line in lines.drain(..).take(8) {
        if !message.is_empty() {
            message.push_str("; ");
        }
        message.push_str(&line);
    }
    if message.len() > 2048 {
        let mut end = 2048;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    message
}

/// Assemble the complete argument list.
///
/// Model text appears only as [`--glob=<value>`] and as the query after `--`;
/// the search path follows the query because `rg` reads its positional
/// arguments as `PATTERN [PATH ...]`.
fn search_args(params: &SearchParams, path: &str, limits: &WorkspaceToolLimits) -> Vec<OsString> {
    let mut args: Vec<OsString> = FIXED_ARGS.iter().map(OsString::from).collect();
    if params.regex != Some(true) {
        args.push("--fixed-strings".into());
    }
    args.push(
        if params.ignore_case == Some(true) {
            "--ignore-case"
        } else {
            "--case-sensitive"
        }
        .into(),
    );
    if let Some(context) = params.context_lines.filter(|lines| *lines > 0) {
        args.push(format!("--context={context}").into());
    }
    if params.include_ignored == Some(true) {
        args.push("--no-ignore".into());
    }
    // `rg --max-depth=N` counts path components, so the stored depth (which
    // counted directory levels below the root) maps to `depth + 1`; a stored 0
    // still searches the root's own files only.
    args.push(format!("--max-depth={}", limits.max_search_depth.saturating_add(1)).into());
    args.push(format!("--max-filesize={}", limits.max_file_scan_bytes).into());
    args.push("--regex-size-limit=1M".into());
    args.push("--dfa-size-limit=1M".into());
    if let Some(glob) = &params.glob {
        args.push(format!("--glob={glob}").into());
    }
    for exclude in VCS_EXCLUDES {
        args.push(format!("--glob={exclude}").into());
    }
    args.push("--".into());
    args.push(params.query.clone().into());
    args.push(path.into());
    args
}

#[derive(Clone, Copy)]
enum SearchStop {
    TimedOut,
    Cancelled,
    Limit(SearchLimit),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchLimit {
    MaxMatches,
    OutputBytes,
}

/// Bounded accumulation of one `rg --json` run.
struct Capture {
    output: Value,
    context_lines: usize,
    pending: VecDeque<(u64, String)>,
    last_match: Option<LastMatch>,
    match_count: usize,
    issue_count: usize,
    complete: bool,
    limit_reason: Option<&'static str>,
    stats: Option<Stats>,
    match_limit: usize,
    max_output_bytes: usize,
}

struct LastMatch {
    index: usize,
    path: String,
    after: usize,
}

#[derive(Clone, Copy)]
struct Stats {
    files: u64,
    bytes: u64,
}

impl Capture {
    fn new(
        path: String,
        context_lines: usize,
        match_limit: usize,
        limits: &WorkspaceToolLimits,
    ) -> Self {
        Self {
            output: json!({
                "path": path,
                "matches": [],
                "issues": [],
                "complete": true,
                "truncated": false,
                "limit_reason": null,
            }),
            context_lines,
            pending: VecDeque::new(),
            last_match: None,
            match_count: 0,
            issue_count: 0,
            complete: true,
            limit_reason: None,
            stats: None,
            match_limit,
            max_output_bytes: limits.max_output_bytes,
        }
    }

    /// Add one `rg --json` line. Returns a limit when reading must stop.
    fn push(&mut self, line: &str) -> Option<SearchLimit> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return None;
        };
        let kind = message.get("type").and_then(Value::as_str);
        let limit = match kind {
            Some("begin") => {
                self.begin();
                None
            }
            Some("match") => self.matches(&message),
            Some("context") => self.context(&message),
            Some("end") => {
                self.pending.clear();
                self.last_match = None;
                None
            }
            Some("summary") => {
                self.summary(&message);
                None
            }
            _ => None,
        };
        limit.or_else(|| self.overflow())
    }

    fn begin(&mut self) {
        self.pending.clear();
        self.last_match = None;
    }

    fn matches(&mut self, message: &Value) -> Option<SearchLimit> {
        if self.match_count >= self.match_limit {
            self.complete = false;
            self.limit_reason = Some("max_matches");
            return Some(SearchLimit::MaxMatches);
        }
        let data = message.get("data")?;
        let Some(path) = data.pointer("/path/text").and_then(Value::as_str) else {
            self.issue("", "unreadable match path");
            return None;
        };
        let path = normalize_path(path);
        let Some(raw_line) = data.pointer("/lines/text").and_then(Value::as_str) else {
            self.issue(&path, "binary_match");
            return None;
        };
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let text = line_text(raw_line);
        let (start, end) = first_submatch(data).unwrap_or((0, 0));
        let (snippet, snippet_truncated) = snippet(&text, start, end);
        let mut object = json!({
            "path": path,
            "line": line_number,
            "text": snippet,
            "snippet_truncated": snippet_truncated,
        });
        if self.context_lines > 0 {
            let before = self
                .pending
                .drain(..)
                .map(|(line, text)| json!({"line": line, "text": text}))
                .collect::<Vec<_>>();
            self.pending.clear();
            object["before"] = json!(before);
            object["after"] = json!([]);
        }
        self.output["matches"]
            .as_array_mut()
            .expect("matches array")
            .push(object);
        let index = self.output["matches"]
            .as_array()
            .expect("matches array")
            .len()
            - 1;
        self.match_count += 1;
        self.last_match = Some(LastMatch {
            index,
            path,
            after: 0,
        });
        None
    }

    fn context(&mut self, message: &Value) -> Option<SearchLimit> {
        if self.context_lines == 0 {
            return None;
        }
        let data = message.get("data")?;
        let path = data.pointer("/path/text").and_then(Value::as_str)?;
        let raw_line = data.pointer("/lines/text").and_then(Value::as_str)?;
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let text = bounded_context(line_text(raw_line));
        if let Some(last) = &mut self.last_match
            && last.path == path
            && last.after < self.context_lines
        {
            let index = last.index;
            last.after += 1;
            self.output["matches"][index]["after"]
                .as_array_mut()
                .expect("after array")
                .push(json!({"line": line_number, "text": text.clone()}));
        }
        self.pending.push_back((line_number, text));
        while self.pending.len() > self.context_lines {
            self.pending.pop_front();
        }
        None
    }

    fn summary(&mut self, message: &Value) {
        let Some(stats) = message.pointer("/data/stats") else {
            return;
        };
        self.stats = Some(Stats {
            files: stats.get("searches").and_then(Value::as_u64).unwrap_or(0),
            bytes: stats
                .get("bytes_searched")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    }

    fn push_stderr(&mut self, line: &str) {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return;
        }
        let body = line.strip_prefix("rg: ").unwrap_or(line);
        if let Some((path, reason)) = body.split_once(": ") {
            let path = normalize_path(path);
            self.issue(&path, reason);
        } else {
            self.issue("", body);
        }
    }

    fn issue(&mut self, path: &str, reason: &str) {
        self.complete = false;
        if self.issue_count >= MAX_ISSUES {
            return;
        }
        self.issue_count += 1;
        let reason = bounded_reason(reason);
        self.output["issues"]
            .as_array_mut()
            .expect("issues array")
            .push(json!({"path": path, "reason": reason}));
    }

    /// Stop when the bounded result no longer fits; the last matches are
    /// dropped so earlier matches stay readable.
    fn overflow(&mut self) -> Option<SearchLimit> {
        let budget = self.max_output_bytes.saturating_sub(RESULT_RESERVE);
        let over =
            |value: &Value| serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > budget);
        if !over(&self.output) {
            return None;
        }
        while self.output["matches"]
            .as_array()
            .is_some_and(|matches| !matches.is_empty())
            && over(&self.output)
        {
            self.output["matches"]
                .as_array_mut()
                .expect("matches array")
                .pop();
            self.match_count = self.match_count.saturating_sub(1);
            self.last_match = None;
        }
        self.complete = false;
        self.limit_reason = Some("output_bytes");
        Some(SearchLimit::OutputBytes)
    }

    fn finish(mut self) -> (Value, Option<Stats>, bool, usize) {
        let complete = self.complete && self.limit_reason.is_none();
        self.output["complete"] = json!(complete);
        self.output["truncated"] = json!(self.limit_reason.is_some());
        self.output["limit_reason"] = json!(self.limit_reason);
        (self.output, self.stats, complete, self.issue_count)
    }
}

fn normalize_path(path: &str) -> String {
    let path = path.strip_prefix("./").unwrap_or(path);
    if path.is_empty() {
        ".".into()
    } else {
        path.to_owned()
    }
}

fn line_text(raw: &str) -> String {
    let line = raw.strip_suffix('\n').unwrap_or(raw);
    let line = line.strip_suffix('\r').unwrap_or(line);
    line.to_owned()
}

fn bounded_context(text: String) -> String {
    if text.len() <= MAX_CONTEXT_BYTES {
        return text;
    }
    let mut end = MAX_CONTEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn bounded_reason(text: &str) -> String {
    if text.len() <= MAX_ISSUE_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_ISSUE_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn first_submatch(data: &Value) -> Option<(usize, usize)> {
    let submatch = data.get("submatches")?.as_array()?.first()?;
    Some((
        submatch.get("start")?.as_u64()? as usize,
        submatch.get("end")?.as_u64()? as usize,
    ))
}

/// Take at most 128 bytes around the match, never splitting a scalar.
fn snippet(line: &str, start: usize, end: usize) -> (String, bool) {
    let line_len = line.len();
    let start = start.min(line_len);
    let end = end.clamp(start, line_len);
    let mut snippet_start = start.saturating_sub(128);
    while !line.is_char_boundary(snippet_start) {
        snippet_start += 1;
    }
    let mut snippet_end = end.saturating_add(128).min(line_len);
    while !line.is_char_boundary(snippet_end) {
        snippet_end -= 1;
    }
    let snippet = &line[snippet_start..snippet_end];
    (snippet.to_owned(), snippet.len() < line_len)
}

fn rejected(message: impl Into<String>) -> SearchExecution {
    SearchExecution {
        status: ToolResultStatus::Rejected,
        content: json!({ "error": message.into() }),
    }
}

fn failed(message: impl Into<String>) -> SearchExecution {
    SearchExecution {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_keep_model_text_only_after_the_separator_and_in_globs() {
        let limits = WorkspaceToolLimits::default();
        let params = SearchParams {
            path: "src".into(),
            query: "--pre=/bin/sh -c evil".into(),
            max_matches: None,
            regex: None,
            ignore_case: None,
            glob: Some("*.rs".into()),
            context_lines: None,
            include_ignored: None,
        };
        let args = search_args(&params, "src", &limits);
        let rendered: Vec<String> = args
            .iter()
            .map(|argument| argument.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(&rendered[..FIXED_ARGS.len()], FIXED_ARGS);
        let separator = rendered.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(rendered[separator + 1], params.query);
        assert_eq!(rendered[separator + 2], "src");
        assert_eq!(
            rendered.iter().filter(|arg| arg.contains("--pre")).count(),
            1
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|arg| arg.starts_with("--glob=*.rs"))
                .count(),
            1
        );
        assert_eq!(
            rendered
                .iter()
                .filter(|arg| arg.starts_with("--glob=!"))
                .count(),
            VCS_EXCLUDES.len()
        );
        // Every VCS exclusion follows the model glob so the exclusion wins.
        let model_glob = rendered
            .iter()
            .position(|arg| arg == "--glob=*.rs")
            .unwrap();
        let first_exclusion = rendered
            .iter()
            .position(|arg| arg.starts_with("--glob=!"))
            .unwrap();
        assert!(model_glob < first_exclusion);
    }

    #[test]
    fn arguments_switch_literal_regex_case_context_and_ignored_switches() {
        let limits = WorkspaceToolLimits {
            max_search_depth: 6,
            max_file_scan_bytes: 4096,
            ..Default::default()
        };
        let params = SearchParams {
            path: ".".into(),
            query: "needle".into(),
            max_matches: None,
            regex: Some(true),
            ignore_case: Some(true),
            glob: Some("*.log".into()),
            context_lines: Some(3),
            include_ignored: Some(true),
        };
        let rendered: Vec<String> = search_args(&params, ".", &limits)
            .iter()
            .map(|argument| argument.to_str().unwrap().to_owned())
            .collect();
        assert!(!rendered.contains(&"--fixed-strings".to_owned()));
        assert!(rendered.contains(&"--ignore-case".to_owned()));
        assert!(rendered.contains(&"--context=3".to_owned()));
        assert!(rendered.contains(&"--no-ignore".to_owned()));
        assert!(rendered.contains(&"--max-depth=7".to_owned()));
        assert!(rendered.contains(&"--max-filesize=4096".to_owned()));
        assert!(rendered.contains(&"--regex-size-limit=1M".to_owned()));
        assert!(rendered.contains(&"--dfa-size-limit=1M".to_owned()));
        assert!(rendered.contains(&"--glob=*.log".to_owned()));

        let literal = SearchParams {
            regex: None,
            ignore_case: None,
            context_lines: Some(0),
            include_ignored: None,
            glob: None,
            ..params
        };
        let rendered: Vec<String> = search_args(&literal, ".", &limits)
            .iter()
            .map(|argument| argument.to_str().unwrap().to_owned())
            .collect();
        assert!(rendered.contains(&"--fixed-strings".to_owned()));
        assert!(rendered.contains(&"--case-sensitive".to_owned()));
        assert!(!rendered.iter().any(|arg| arg.starts_with("--context=")));
        assert!(!rendered.contains(&"--no-ignore".to_owned()));
    }

    #[test]
    fn parser_keeps_matches_context_issues_and_summary_stats() {
        let limits = WorkspaceToolLimits::default();
        let mut capture = Capture::new(".".into(), 2, 200, &limits);
        let lines = [
            r#"{"type":"begin","data":{"path":{"text":"docs/a.txt"}}}"#,
            r#"{"type":"context","data":{"path":{"text":"docs/a.txt"},"lines":{"text":"before 1\n"},"line_number":1,"absolute_offset":0,"submatches":[]}}"#,
            r#"{"type":"context","data":{"path":{"text":"docs/a.txt"},"lines":{"text":"before 2\n"},"line_number":2,"absolute_offset":9,"submatches":[]}}"#,
            r#"{"type":"match","data":{"path":{"text":"docs/a.txt"},"lines":{"text":"a needle line\n"},"line_number":3,"absolute_offset":18,"submatches":[{"match":{"text":"needle"},"start":2,"end":8}]}}"#,
            r#"{"type":"context","data":{"path":{"text":"docs/a.txt"},"lines":{"text":"after 1\n"},"line_number":4,"absolute_offset":33,"submatches":[]}}"#,
            r#"{"type":"end","data":{"path":{"text":"docs/a.txt"},"binary_offset":null,"stats":{}}}"#,
            r#"{"data":{"elapsed_total":{"secs":0,"nanos":1,"human":"0s"},"stats":{"searches":1,"bytes_searched":42}},"type":"summary"}"#,
        ];
        for line in lines {
            assert!(capture.push(line).is_none());
        }
        let (output, stats, complete, issues) = capture.finish();
        assert!(complete);
        assert_eq!(issues, 0);
        assert_eq!(output["matches"][0]["path"], "docs/a.txt");
        assert_eq!(output["matches"][0]["line"], 3);
        assert_eq!(output["matches"][0]["text"], "a needle line");
        assert_eq!(output["matches"][0]["snippet_truncated"], false);
        assert_eq!(output["matches"][0]["before"][0]["text"], "before 1");
        assert_eq!(output["matches"][0]["before"][1]["line"], 2);
        assert_eq!(output["matches"][0]["after"][0]["text"], "after 1");
        assert_eq!(output["scanned_files"], Value::Null);
        assert_eq!(output["limit_reason"], Value::Null);
        let stats = stats.expect("summary stats");
        assert_eq!(stats.files, 1);
        assert_eq!(stats.bytes, 42);
    }

    #[test]
    fn parser_marks_stderr_lines_as_incomplete_issues() {
        let limits = WorkspaceToolLimits::default();
        let mut capture = Capture::new(".".into(), 0, 200, &limits);
        capture.push_stderr("rg: denied/private: Permission denied (os error 13)");
        capture.push_stderr("rg: another: I/O error");
        let (output, _stats, complete, issues) = capture.finish();
        assert!(!complete);
        assert_eq!(issues, 2);
        assert_eq!(output["complete"], false);
        assert_eq!(output["truncated"], false);
        assert_eq!(output["issues"][0]["path"], "denied/private");
        assert_eq!(
            output["issues"][0]["reason"],
            "Permission denied (os error 13)"
        );
    }

    #[test]
    fn parser_stops_at_the_match_limit_and_drops_the_extra_match() {
        let limits = WorkspaceToolLimits {
            max_search_matches: 1,
            ..Default::default()
        };
        let mut capture = Capture::new(".".into(), 0, 1, &limits);
        let first = r#"{"type":"match","data":{"path":{"text":"a"},"lines":{"text":"needle\n"},"line_number":1,"submatches":[{"start":0,"end":6}]}}"#;
        let second = r#"{"type":"match","data":{"path":{"text":"a"},"lines":{"text":"needle\n"},"line_number":2,"submatches":[{"start":0,"end":6}]}}"#;
        assert!(capture.push(first).is_none());
        assert_eq!(capture.push(second), Some(SearchLimit::MaxMatches));
        let (output, _stats, complete, _issues) = capture.finish();
        assert!(!complete);
        assert_eq!(output["limit_reason"], "max_matches");
        assert_eq!(output["truncated"], true);
        assert_eq!(output["matches"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn parser_drops_matches_that_do_not_fit_the_result_budget() {
        let limits = WorkspaceToolLimits {
            max_output_bytes: 1024,
            ..Default::default()
        };
        let mut capture = Capture::new(".".into(), 0, 200, &limits);
        let limits_match = r#"{"type":"match","data":{"path":{"text":"a"},"lines":{"text":"needle\n"},"line_number":1,"submatches":[{"start":0,"end":6}]}}"#;
        let mut stopped = None;
        for _ in 0..64 {
            if let Some(limit) = capture.push(limits_match) {
                stopped = Some(limit);
                break;
            }
        }
        assert_eq!(stopped, Some(SearchLimit::OutputBytes));
        let (output, _stats, complete, _issues) = capture.finish();
        assert!(!complete);
        assert_eq!(output["limit_reason"], "output_bytes");
        assert!(serde_json::to_vec(&output).unwrap().len() <= 1024);
    }
}
