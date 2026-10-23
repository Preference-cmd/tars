//! One MCP server connection exposed as a dynamic tool source.

use super::status::{
    MAX_SKIPPED_TOOLS, McpServerStatus, McpSkippedTool, bounded_name, bounded_reason,
};
use super::stderr::StderrCapture;
use crate::definition::{McpApproval, McpServerConfig, WorkspaceTool, redact_substitutions};
use crate::permission::{McpPermissionTable, McpToolInfo, ToolPermissionClass};
use causa::extension::McpToolSource;
use causa::kernel::{
    ArtifactStore, CallControl, DynamicToolSource, SourceError, ToolCallContext, ToolDefinition,
    ToolDescription, ToolExecutionError, ToolResultPayload, Truncation,
};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Longest model-facing tool name a provider can be asked to call.
const MAX_TOOL_NAME_BYTES: usize = 64;
/// Largest serialized tool result kept intact; longer results lose their middle.
pub(crate) const MAX_RESULT_BYTES: usize = 64 * 1024;
/// Room for the omitted-byte count, which grows the notice after measuring.
const NOTICE_DIGITS_MARGIN: usize = 24;
/// Bound on an orderly MCP shutdown, including waiting for a stdio child to
/// exit. The adapter gives up on its handshake after two seconds while the
/// transport keeps closing in the background and kills the child after
/// three, so five seconds covers both.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct SourceState {
    ready: bool,
    reason: Option<String>,
    allowed: HashSet<String>,
    skipped: Vec<McpSkippedTool>,
}

/// A connected MCP server whose tools have been filtered for the model.
///
/// Calls and listings share the connection through a read lock, so calls on
/// one server run concurrently. Closing consumes the adapter while
/// [`DynamicToolSource`] only offers `&self`, so shutdown takes the write
/// lock: it waits for in-flight calls, which their own deadlines bound.
pub(crate) struct McpServerSource {
    id: String,
    prefix: String,
    config: McpServerConfig,
    inner: RwLock<Option<McpToolSource>>,
    state: Mutex<SourceState>,
    version: AtomicU64,
    /// The stderr pipe of a stdio server; it ends when the child exits.
    stderr: Option<Arc<StderrCapture>>,
    substitutions: Vec<(String, String)>,
    permission_table: Arc<McpPermissionTable>,
}

impl McpServerSource {
    pub(crate) fn new(
        config: McpServerConfig,
        inner: McpToolSource,
        stderr: Option<Arc<StderrCapture>>,
        substitutions: Vec<(String, String)>,
        permission_table: Arc<McpPermissionTable>,
    ) -> Self {
        Self {
            prefix: format!("mcp_{}_", config.id),
            id: config.id.clone(),
            version: AtomicU64::new(inner.version()),
            config,
            inner: RwLock::new(Some(inner)),
            state: Mutex::new(SourceState {
                ready: true,
                reason: None,
                allowed: HashSet::new(),
                skipped: Vec::new(),
            }),
            stderr,
            substitutions,
            permission_table,
        }
    }

    pub(crate) fn status(&self) -> McpServerStatus {
        let state = self.state.lock().expect("MCP source state lock");
        McpServerStatus {
            id: self.id.clone(),
            ready: state.ready,
            reason: state.reason.clone(),
            skipped_tools: state.skipped.clone(),
        }
    }

    /// List and filter the current catalog, updating the allowed names and
    /// the reported skipped tools.
    pub(crate) async fn refresh(&self) -> Result<Vec<ToolDescription>, String> {
        let guard = self.inner.read().await;
        let Some(source) = guard.as_ref() else {
            return Err("MCP server connection is closed".to_owned());
        };
        // Binding waits for every source's listing, so a server that stops
        // answering must not hold up the model or the other tools.
        let listing = Duration::from_secs(self.config.startup_timeout_sec);
        let listed = match tokio::time::timeout(listing, source.describe()).await {
            Ok(result) => result.map_err(|error| format!("listing tools failed: {error}")),
            Err(_) => Err(format!(
                "listing tools timed out after {}s",
                self.config.startup_timeout_sec
            )),
        };
        let descriptions = match listed {
            Ok(descriptions) => descriptions,
            Err(error) => {
                self.permission_table.remove_server(&self.id);
                let redacted = redact_substitutions(&error, &self.substitutions);
                let reason = bounded_reason(&redacted);
                let mut state = self.state.lock().expect("MCP source state lock");
                state.ready = false;
                state.reason = Some(reason.clone());
                state.allowed.clear();
                return Err(reason);
            }
        };
        let (filtered, allowed, skipped) = self.filter(descriptions);
        let mut server_tools = Vec::new();
        for description in &filtered {
            let name = &description.definition.name;
            let Some(raw) = name.strip_prefix(&self.prefix) else {
                continue;
            };
            let approval = self
                .config
                .tools
                .get(raw)
                .map(|t| t.approval)
                .unwrap_or(self.config.approval);
            let category = match approval {
                McpApproval::Approve => ToolPermissionClass::McpApprove,
                McpApproval::Prompt => ToolPermissionClass::McpPrompt,
                McpApproval::Writes => {
                    if description.hints.read_only == Some(true)
                        && description.hints.destructive != Some(true)
                    {
                        ToolPermissionClass::KnownReadOnly
                    } else {
                        ToolPermissionClass::McpPrompt
                    }
                }
            };
            server_tools.push((
                name.clone(),
                McpToolInfo {
                    server_id: self.id.clone(),
                    raw_tool_name: raw.to_string(),
                    category,
                },
            ));
        }
        self.permission_table
            .update_server_tools(&self.id, server_tools);
        self.version.store(source.version(), Ordering::SeqCst);
        let mut state = self.state.lock().expect("MCP source state lock");
        state.ready = true;
        state.reason = None;
        state.allowed = allowed;
        state.skipped = skipped;
        Ok(filtered)
    }

    /// Close the connection and wait, within [`SHUTDOWN_TIMEOUT`], for a stdio
    /// child to exit. Repeated calls are no-ops; failures are ignored because
    /// the transport closes when the adapter drops either way.
    pub(crate) async fn shutdown(&self) {
        self.permission_table.remove_server(&self.id);
        let closed = self.inner.write().await.take();
        if let Some(source) = closed {
            let started = Instant::now();
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, source.close()).await;
            if let Some(stderr) = &self.stderr {
                stderr
                    .ended(SHUTDOWN_TIMEOUT.saturating_sub(started.elapsed()))
                    .await;
            }
        }
        let mut state = self.state.lock().expect("MCP source state lock");
        state.ready = false;
        state
            .reason
            .get_or_insert_with(|| "connection closed".to_owned());
        state.allowed.clear();
    }

    fn filter(
        &self,
        descriptions: Vec<ToolDescription>,
    ) -> (Vec<ToolDescription>, HashSet<String>, Vec<McpSkippedTool>) {
        let mut filtered = Vec::new();
        let mut allowed = HashSet::new();
        let mut skipped = Vec::new();
        let mut reported = HashSet::new();
        for description in descriptions {
            let name = description.definition.name.clone();
            if let Some(reason) = self.skip_reason(&name) {
                let name = bounded_name(&name);
                if skipped.len() < MAX_SKIPPED_TOOLS && reported.insert((name.clone(), reason)) {
                    skipped.push(McpSkippedTool {
                        name,
                        reason: reason.to_owned(),
                    });
                }
                continue;
            }
            // A server listing one name twice is answered by its first entry.
            if allowed.insert(name) {
                filtered.push(description);
            }
        }
        (filtered, allowed, skipped)
    }

    fn skip_reason(&self, name: &str) -> Option<&'static str> {
        let Some(raw) = name.strip_prefix(&self.prefix) else {
            return Some("invalid name");
        };
        if let Some(enabled) = &self.config.enabled_tools
            && !enabled.iter().any(|candidate| candidate == raw)
        {
            return Some("not enabled");
        }
        if let Some(disabled) = &self.config.disabled_tools
            && disabled.iter().any(|candidate| candidate == raw)
        {
            return Some("disabled");
        }
        if name.len() > MAX_TOOL_NAME_BYTES {
            return Some("too long");
        }
        if raw.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Some("invalid name");
        }
        // Causa prefixes every MCP name, so a collision means a malformed
        // listing; it is still never allowed to shadow a built-in tool.
        if WorkspaceTool::is_builtin_name(name) {
            return Some("name collision");
        }
        None
    }

    fn redact_tool_error(&self, error: ToolExecutionError) -> ToolExecutionError {
        match error {
            ToolExecutionError::Protocol(text) => {
                ToolExecutionError::Protocol(redact_substitutions(&text, &self.substitutions))
            }
            ToolExecutionError::Unavailable(text) => {
                ToolExecutionError::Unavailable(redact_substitutions(&text, &self.substitutions))
            }
            ToolExecutionError::UnknownTool(text) => {
                ToolExecutionError::UnknownTool(redact_substitutions(&text, &self.substitutions))
            }
            other => other,
        }
    }
}

#[async_trait::async_trait]
impl DynamicToolSource for McpServerSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn version(&self) -> u64 {
        // The adapter counts `tools/list_changed` notifications. While shutdown
        // holds the connection the last observed count stands.
        if let Ok(guard) = self.inner.try_read()
            && let Some(source) = guard.as_ref()
        {
            self.version.store(source.version(), Ordering::SeqCst);
        }
        self.version.load(Ordering::SeqCst)
    }

    async fn list(&self) -> Result<Vec<ToolDefinition>, SourceError> {
        Ok(self
            .describe()
            .await?
            .into_iter()
            .map(|description| description.definition)
            .collect())
    }

    async fn describe(&self) -> Result<Vec<ToolDescription>, SourceError> {
        // A server that fails after startup must not make the other tools
        // unbindable: its tools are absent and its status carries the reason.
        Ok(self.refresh().await.unwrap_or_default())
    }

    async fn invoke(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        store: Option<&dyn ArtifactStore>,
    ) -> Result<ToolResultPayload, ToolExecutionError> {
        let name = &call.input.tool_name;
        let allowed = name.starts_with(&self.prefix)
            && self
                .state
                .lock()
                .expect("MCP source state lock")
                .allowed
                .contains(name.as_str());
        if !allowed {
            return Err(ToolExecutionError::UnknownTool(name.clone()));
        }
        let guard = self.inner.read().await;
        let Some(source) = guard.as_ref() else {
            return Err(ToolExecutionError::Unavailable(
                "MCP server connection is closed".into(),
            ));
        };
        let control = control.with_timeout(Duration::from_secs(self.config.tool_timeout_sec));
        let mut payload = source
            .invoke(call, &control, store)
            .await
            .map_err(|error| self.redact_tool_error(limit_error(error)))?;
        limit_result(&mut payload);
        Ok(payload)
    }
}

/// Longest error text a server can put in front of the model. Errors carry
/// the server's own message and data, so they get the same middle cut as
/// results, well inside [`MAX_RESULT_BYTES`] once wrapped.
pub(crate) const MAX_ERROR_BYTES: usize = 16 * 1024;

/// Bound the server-provided text inside an error.
pub(crate) fn limit_error(error: ToolExecutionError) -> ToolExecutionError {
    match error {
        ToolExecutionError::Protocol(text) => ToolExecutionError::Protocol(cut_middle(text)),
        ToolExecutionError::Unavailable(text) => ToolExecutionError::Unavailable(cut_middle(text)),
        ToolExecutionError::UnknownTool(text) => ToolExecutionError::UnknownTool(cut_middle(text)),
        other => other,
    }
}

/// `text` itself when its escaped form fits [`MAX_ERROR_BYTES`]; otherwise
/// its head, the omission notice and its tail.
fn cut_middle(text: String) -> String {
    let escaped: usize = text.chars().map(escaped_len).sum();
    if escaped <= MAX_ERROR_BYTES {
        return text;
    }
    let available = MAX_ERROR_BYTES - notice(0).len() - NOTICE_DIGITS_MARGIN;
    let head_end = prefix_end(&text, available / 2);
    let tail_start = suffix_start(&text, available - available / 2).max(head_end);
    format!(
        "{}{}{}",
        &text[..head_end],
        notice(tail_start - head_end),
        &text[tail_start..]
    )
}

/// Keep the whole serialized result within [`MAX_RESULT_BYTES`]. An oversized
/// observation becomes one string holding its head, a notice naming how many
/// bytes were omitted, and its tail, each end cut on a character boundary.
pub(crate) fn limit_result(payload: &mut ToolResultPayload) {
    if serialized_len(payload) <= MAX_RESULT_BYTES {
        return;
    }
    let text = match &payload.output.content {
        serde_json::Value::String(text) => text.clone(),
        value => value.to_string(),
    };
    payload.output.truncation = Truncation::Middle;
    payload.output.content = serde_json::Value::String(notice(0));
    let base = serialized_len(payload).saturating_add(NOTICE_DIGITS_MARGIN);
    let available = MAX_RESULT_BYTES.saturating_sub(base);
    let head_end = prefix_end(&text, available / 2);
    let tail_start = suffix_start(&text, available - available / 2).max(head_end);
    let omitted = tail_start - head_end;
    payload.output.content = serde_json::Value::String(format!(
        "{}{}{}",
        &text[..head_end],
        notice(omitted),
        &text[tail_start..]
    ));
}

fn notice(omitted: usize) -> String {
    format!("\n[... truncated: {omitted} bytes omitted from the middle ...]\n")
}

fn serialized_len(payload: &ToolResultPayload) -> usize {
    serde_json::to_vec(payload).map_or(usize::MAX, |bytes| bytes.len())
}

/// Length of one character inside a JSON string as `serde_json` writes it.
fn escaped_len(character: char) -> usize {
    match character {
        '\"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        character if (character as u32) < 0x20 => 6,
        character => character.len_utf8(),
    }
}

/// Byte index ending the longest prefix whose escaped form fits `budget`.
fn prefix_end(text: &str, budget: usize) -> usize {
    let mut used = 0;
    for (index, character) in text.char_indices() {
        used += escaped_len(character);
        if used > budget {
            return index;
        }
    }
    text.len()
}

/// Byte index starting the longest suffix whose escaped form fits `budget`.
fn suffix_start(text: &str, budget: usize) -> usize {
    let mut used = 0;
    for (index, character) in text.char_indices().rev() {
        used += escaped_len(character);
        if used > budget {
            return index + character.len_utf8();
        }
    }
    0
}
