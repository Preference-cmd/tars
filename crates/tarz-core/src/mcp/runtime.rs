//! Starting, owning and closing the MCP servers of one session.

use super::source::McpServerSource;
use super::status::{McpServerStatus, bounded_reason};
use super::stderr::StderrCapture;
use crate::definition::{McpServerConfig, SessionDefinition};
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
        let mut servers: Vec<ServerSlot> = Vec::with_capacity(definition.mcp_servers.len());
        let mut max_tool_timeout: Option<Duration> = None;
        for config in &definition.mcp_servers {
            let startup = Duration::from_secs(config.startup_timeout_sec);
            let mut capture = None;
            let attempt = if let Some(program) = config.command.as_deref() {
                match std::io::pipe() {
                    Ok((reader, writer)) => {
                        let sink = StderrCapture::start(reader);
                        capture = Some(sink.clone());
                        let command =
                            stdio_command(program, config, definition.workspace_root.as_deref());
                        match tokio::time::timeout(
                            startup,
                            connect_stdio(config, command, writer, sink),
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
                    Err(error) => Err(format!("could not create a stderr pipe: {error}")),
                }
            } else {
                match tokio::time::timeout(startup, connect_http(config)).await {
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
                    let reason = failure_reason(&reason, stderr.as_deref());
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
) -> Result<McpServerSource, String> {
    let inner = McpToolSource::connect_stdio_with_stderr(&config.id, command, writer)
        .await
        .map_err(|error| error.to_string())?;
    let source = McpServerSource::new(config.clone(), inner, Some(stderr));
    source.refresh().await?;
    Ok(source)
}

async fn connect_http(config: &McpServerConfig) -> Result<McpServerSource, String> {
    let url = config
        .url
        .as_deref()
        .ok_or_else(|| "missing server URL".to_owned())?;
    let token = match &config.bearer_token_env_var {
        Some(name) => Some(
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("environment variable {name} is missing or empty"))?,
        ),
        None => None,
    };
    let inner = McpToolSource::connect_http(&config.id, url, token)
        .await
        .map_err(|error| error.to_string())?;
    let source = McpServerSource::new(config.clone(), inner, None);
    source.refresh().await?;
    Ok(source)
}

/// A child gets only the minimal inherited environment plus the configured
/// literals and forwarded names. Everything else stays with Tarz.
fn stdio_command(
    program: &str,
    config: &McpServerConfig,
    workspace_root: Option<&Path>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    // The adapter bounds its shutdown handshake at two seconds, while the
    // transport waits three for the child before killing it; when the
    // handshake gives up first, the child is dropped without that kill. A
    // server still busy with a call would then outlive the session.
    command.kill_on_drop(true);
    command.args(&config.args);
    command.env_clear();
    for name in ["PATH", "HOME", "LANG"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for (name, value) in &config.env {
        command.env(name, value);
    }
    for name in &config.env_vars {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    if let Some(root) = workspace_root {
        command.current_dir(root);
    }
    command
}

fn failure_reason(base: &str, stderr: Option<&str>) -> String {
    let text = match stderr.map(str::trim).filter(|text| !text.is_empty()) {
        Some(stderr) => format!("{base}: {stderr}"),
        None => base.to_owned(),
    };
    bounded_reason(&text)
}
