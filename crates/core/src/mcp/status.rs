//! Bounded, display-safe runtime state for MCP servers.

use crate::permission::escape_untrusted_text;

/// Longest failure reason retained for display, in bytes.
pub(crate) const MAX_REASON_BYTES: usize = 512;
/// Longest skipped tool name retained for display, in bytes.
pub(crate) const MAX_NAME_BYTES: usize = 512;
/// Most skipped tools retained per server.
pub(crate) const MAX_SKIPPED_TOOLS: usize = 128;

/// Runtime state of one MCP server attached to an open session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerStatus {
    /// Server id from the frozen session definition.
    pub id: String,
    /// Whether the connection is established and its filtered tool list is usable.
    pub ready: bool,
    /// Why the server is unusable, bounded to 512 bytes on a
    /// UTF-8 boundary with control characters escaped; `None` while ready.
    pub reason: Option<String>,
    /// Tools excluded from the model-visible list, at most 128 entries.
    pub skipped_tools: Vec<McpSkippedTool>,
}

/// One MCP tool excluded from the model-visible tool list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpSkippedTool {
    /// Model-facing tool name, bounded and escaped for display.
    pub name: String,
    /// Why the tool was excluded, such as `not enabled`, `disabled`,
    /// `invalid name`, `too long` or `name collision`.
    pub reason: String,
}

/// Escape control characters and fold line breaks so a status stays on one line.
pub(crate) fn single_line(text: &str) -> String {
    escape_untrusted_text(text)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// Truncate on a UTF-8 character boundary so the result is at most `limit` bytes.
pub(crate) fn truncate_utf8(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// Bound a failure reason for display.
pub(crate) fn bounded_reason(text: &str) -> String {
    truncate_utf8(single_line(text), MAX_REASON_BYTES)
}

/// Bound a tool name for display.
pub(crate) fn bounded_name(text: &str) -> String {
    truncate_utf8(single_line(text), MAX_NAME_BYTES)
}
