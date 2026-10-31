//! Starting, owning and closing the MCP servers of one session.

use super::source::McpServerSource;
use super::status::{McpServerStatus, bounded_reason};
use super::stderr::StderrCapture;
use crate::definition::{
    McpServerConfig, SessionDefinition, expand_template_recording, redact_substitutions,
    valid_http_endpoint,
};
use crate::permission::McpPermissionTable;
use causa::extension::McpToolSource;
use causa::kernel::DynamicToolSource;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Bounded wait for the stderr reader to reach end of file after a failure.
const STDERR_WAIT: Duration = Duration::from_secs(1);

/// Why starting an MCP server prevented the session from opening.
#[derive(Debug)]
pub(crate) struct McpStartError {
    pub(crate) server_id: String,
    pub(crate) reason: String,
}

impl std::fmt::Display for McpStartError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "MCP server '{}' failed to start: {}",
            self.server_id, self.reason
        )
    }
}

impl std::error::Error for McpStartError {}

enum ServerSlot {
    Ready(Arc<McpServerSource>),
    Failed(McpServerStatus),
}

/// All MCP connections of one open session, in definition order.
pub(crate) struct McpRuntime {
    servers: Vec<ServerSlot>,
    max_tool_timeout: Option<Duration>,
    closed: AtomicBool,
    permission_table: Arc<McpPermissionTable>,
}

impl McpRuntime {
    /// Connect the servers named by a frozen definition.
    ///
    /// A non-required server that fails keeps its status and stays absent;
    /// a required failure closes every connection started so far and returns
    /// an error naming the server and its bounded reason.
    pub(crate) async fn start(
        definition: &SessionDefinition,
    ) -> Result<Option<Arc<Self>>, McpStartError> {
        if definition.mcp_servers.is_empty() {
            return Ok(None);
        }
        let permission_table = Arc::new(McpPermissionTable::new());
        let mut servers: Vec<ServerSlot> = Vec::with_capacity(definition.mcp_servers.len());
        let mut max_tool_timeout: Option<Duration> = None;
        for config in &definition.mcp_servers {
            let startup = Duration::from_secs(config.startup_timeout_sec);
            let mut capture = None;
            let mut substitutions = Vec::new();
            let attempt = if let Some(program) = config.command.as_deref() {
                match std::io::pipe() {
                    Ok((reader, writer)) => {
                        let sink = StderrCapture::start(reader);
                        capture = Some(sink.clone());
                        match stdio_command(
                            program,
                            config,
                            definition.workspace_root.as_deref(),
                            &mut substitutions,
                        ) {
                            Ok(command) => {
                                match tokio::time::timeout(
                                    startup,
                                    connect_stdio(
                                        config,
                                        command,
                                        writer,
                                        sink,
                                        substitutions.clone(),
                                        permission_table.clone(),
                                    ),
                                )
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_) => Err(format!(
                                        "startup timed out after {}s",
                                        config.startup_timeout_sec
                                    )),
                                }
                            }
                            Err(reason) => Err(reason),
                        }
                    }
                    Err(error) => Err(format!("could not create a stderr pipe: {error}")),
                }
            } else {
                match tokio::time::timeout(
                    startup,
                    connect_http(config, &mut substitutions, permission_table.clone()),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(format!(
                        "startup timed out after {}s",
                        config.startup_timeout_sec
                    )),
                }
            };
            match attempt {
                Ok(source) => {
                    let timeout = Duration::from_secs(config.tool_timeout_sec);
                    max_tool_timeout =
                        Some(max_tool_timeout.map_or(timeout, |current| current.max(timeout)));
                    servers.push(ServerSlot::Ready(Arc::new(source)));
                }
                Err(reason) => {
                    let stderr = match &capture {
                        Some(capture) => capture.text(STDERR_WAIT).await,
                        None => None,
                    };
                    let reason = failure_reason(&reason, stderr.as_deref(), &substitutions);
                    if config.required {
                        shutdown_all(&servers).await;
                        return Err(McpStartError {
                            server_id: config.id.clone(),
                            reason,
                        });
                    }
                    servers.push(ServerSlot::Failed(McpServerStatus {
                        id: config.id.clone(),
                        ready: false,
                        reason: Some(reason),
                        skipped_tools: Vec::new(),
                    }));
                }
            }
        }
        Ok(Some(Arc::new(Self {
            servers,
            max_tool_timeout,
            closed: AtomicBool::new(false),
            permission_table,
        })))
    }

    /// Registered sources of the servers that started, in definition order.
    pub(crate) fn sources(&self) -> Vec<Arc<dyn DynamicToolSource>> {
        self.servers
            .iter()
            .filter_map(|slot| match slot {
                ServerSlot::Ready(source) => Some(source.clone() as Arc<dyn DynamicToolSource>),
                ServerSlot::Failed(_) => None,
            })
            .collect()
    }

    /// Read-only handle to the shared MCP permission table.
    pub(crate) fn permission_table(&self) -> Arc<McpPermissionTable> {
        self.permission_table.clone()
    }

    /// Largest per-call deadline among the servers that started.
    pub(crate) fn max_tool_timeout(&self) -> Option<Duration> {
        self.max_tool_timeout
    }

    /// Snapshot of every configured server, started or failed, in definition order.
    pub(crate) fn status(&self) -> Vec<McpServerStatus> {
        self.servers
            .iter()
            .map(|slot| match slot {
                ServerSlot::Ready(source) => source.status(),
                ServerSlot::Failed(status) => status.clone(),
            })
            .collect()
    }

    /// Close every connection, waiting for the child processes or HTTP sessions
    /// to end. Repeated calls are no-ops and individual failures are ignored.
    pub(crate) async fn close(self: &Arc<Self>) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        shutdown_all(&self.servers).await;
    }
}

/// Shut every started server down at once, so one slow server does not
/// stretch the others' waits; each shutdown is bounded on its own.
async fn shutdown_all(servers: &[ServerSlot]) {
    let mut shutdowns = tokio::task::JoinSet::new();
    for slot in servers {
        if let ServerSlot::Ready(source) = slot {
            let source = source.clone();
            shutdowns.spawn(async move { source.shutdown().await });
        }
    }
    while shutdowns.join_next().await.is_some() {}
}

async fn connect_stdio(
    config: &McpServerConfig,
    command: tokio::process::Command,
    writer: std::io::PipeWriter,
    stderr: Arc<StderrCapture>,
    substitutions: Vec<(String, String)>,
    permission_table: Arc<McpPermissionTable>,
) -> Result<McpServerSource, String> {
    let inner = McpToolSource::connect_stdio_with_stderr(&config.id, command, writer)
        .await
        .map_err(|error| error.to_string())?;
    let source = McpServerSource::new(
        config.clone(),
        inner,
        Some(stderr),
        substitutions,
        permission_table,
    );
    source.refresh().await?;
    Ok(source)
}

async fn connect_http(
    config: &McpServerConfig,
    substitutions: &mut Vec<(String, String)>,
    permission_table: Arc<McpPermissionTable>,
) -> Result<McpServerSource, String> {
    let url_template = config
        .url
        .as_deref()
        .ok_or_else(|| "missing server URL".to_owned())?;
    let mut url_substitutions = Vec::new();
    let url = expand_template_recording(url_template, &mut url_substitutions)?;
    // Connection errors quote the URL after percent-encoding and host
    // normalization, so a value changed that way would escape redaction.
    if url_substitutions
        .iter()
        .any(|(_, value)| !value.bytes().all(url_safe_byte))
    {
        return Err(
            "a variable in the URL expands to characters that are not allowed there".to_string(),
        );
    }
    substitutions.extend(url_substitutions);
    if !valid_http_endpoint(&url) {
        return Err("expanded URL is not a valid HTTP endpoint".to_string());
    }
    let mut headers = Vec::new();
    for (name, val_template) in &config.headers {
        let val = expand_template_recording(val_template, substitutions)?;
        headers.push((name.as_str(), val));
    }
    let inner = McpToolSource::connect_http_with_headers(&config.id, url, headers)
        .await
        .map_err(|error| error.to_string())?;
    let source = McpServerSource::new(
        config.clone(),
        inner,
        None,
        substitutions.clone(),
        permission_table,
    );
    source.refresh().await?;
    Ok(source)
}

/// Printable ASCII that the URL parser keeps as written in every component
/// a valid endpoint can have.
fn url_safe_byte(byte: u8) -> bool {
    byte.is_ascii_graphic()
        && !matches!(
            byte,
            b'"' | b'<' | b'>' | b'`' | b'{' | b'}' | b'\'' | b'\\'
        )
}

/// A child gets only the minimal inherited environment plus the configured
/// literals and expanded variable values. Everything else stays with TARS.
fn stdio_command(
    program: &str,
    config: &McpServerConfig,
    workspace_root: Option<&Path>,
    substitutions: &mut Vec<(String, String)>,
) -> Result<tokio::process::Command, String> {
    let mut command = tokio::process::Command::new(program);
    // Causa bounds its close at two seconds; rmcp may still drain in-flight
    // responses for up to two seconds, close stdin, then wait three seconds
    // before killing the child. `kill_on_drop` covers a runtime that ends
    // before that kill runs.
    command.kill_on_drop(true);
    let mut expanded_args = Vec::with_capacity(config.args.len());
    for arg in &config.args {
        expanded_args.push(expand_template_recording(arg, substitutions)?);
    }
    command.args(&expanded_args);
    command.env_clear();
    for name in ["PATH", "HOME", "LANG"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (name, value_template) in &config.env {
        let value = expand_template_recording(value_template, substitutions)?;
        command.env(name, value);
    }
    if let Some(root) = workspace_root {
        command.current_dir(root);
    }
    Ok(command)
}

fn failure_reason(base: &str, stderr: Option<&str>, substitutions: &[(String, String)]) -> String {
    let text = match stderr.map(str::trim).filter(|text| !text.is_empty()) {
        Some(stderr) => format!("{base}: {stderr}"),
        None => base.to_owned(),
    };
    let redacted = redact_substitutions(&text, substitutions);
    bounded_reason(&redacted)
}
