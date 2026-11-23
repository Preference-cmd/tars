//! Shared MCP tool permission classification registry and lifecycle adapter.

use crate::definition::McpApproval;
use crate::policy::ToolPermissionClass;
use crate::tools::{ApprovalPreview, ManagedTool, PreparedCall, ToolCallIntent};
use async_trait::async_trait;
use causa::extension::McpToolAnnotations;
use causa::kernel::ToolCallContext;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Information about an allowed MCP tool recorded during catalog refresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct McpToolInfo {
    /// Server identifier.
    pub(crate) server_id: String,
    /// Original tool name advertised by the server.
    pub(crate) raw_tool_name: String,
    /// Categorization for permission checks.
    pub(crate) category: ToolPermissionClass,
}

/// Metadata for a currently discoverable `KnownReadOnly` MCP tool.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpReadOnlyToolDescriptor {
    /// Model-facing prefixed tool name (`mcp_<server>_<tool>`).
    pub(crate) name: String,
    /// Owning MCP server identifier.
    pub(crate) server_id: String,
    /// Tool description advertised by the MCP server.
    pub(crate) description: String,
    /// JSON Schema for tool arguments.
    pub(crate) parameters: serde_json::Value,
}

/// Derive the TARS permission class for an MCP tool from frozen server config and server annotations.
///
/// Server annotations can only relax confirmation when frozen configuration selects `writes`
/// and the tool explicitly declares `read_only = true` without `destructive = true`.
/// When frozen configuration selects `prompt` (`ask`) or `approve`, server annotations cannot override it.
pub(crate) fn classify_mcp_tool(
    approval: McpApproval,
    annotations: Option<&McpToolAnnotations>,
) -> ToolPermissionClass {
    match approval {
        McpApproval::Approve => ToolPermissionClass::McpApprove,
        McpApproval::Prompt => ToolPermissionClass::McpPrompt,
        McpApproval::Writes => {
            if annotations.is_some_and(|a| a.read_only == Some(true) && a.destructive != Some(true))
            {
                ToolPermissionClass::KnownReadOnly
            } else {
                ToolPermissionClass::McpPrompt
            }
        }
    }
}

/// Formats arguments JSON compactly, truncated at a UTF-8 character boundary to at most 1024 bytes.
/// If truncated, an ellipsis is appended.
pub(crate) fn format_mcp_arguments(value: &serde_json::Value) -> String {
    let full = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    if full.len() <= 1024 {
        full
    } else {
        let mut end = 1024;
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &full[..end])
    }
}

/// Managed lifecycle adapter for one registered MCP tool.
#[derive(Clone, Debug)]
pub(crate) struct McpToolAdapter {
    info: McpToolInfo,
}

impl McpToolAdapter {
    pub(crate) fn new(info: McpToolInfo) -> Self {
        Self { info }
    }
}

#[async_trait]
impl ManagedTool for McpToolAdapter {
    fn classify(&self, _arguments: &serde_json::Value) -> ToolCallIntent {
        ToolCallIntent {
            class: self.info.category,
            command_subject: None,
        }
    }

    fn approval_preview(
        &self,
        call: &ToolCallContext,
        _prepared: &PreparedCall,
    ) -> Option<ApprovalPreview> {
        Some(ApprovalPreview::McpCall {
            server: self.info.server_id.clone(),
            tool: self.info.raw_tool_name.clone(),
            arguments: format_mcp_arguments(&call.input.arguments),
        })
    }
}

#[derive(Clone)]
struct McpToolEntry {
    adapter: Arc<McpToolAdapter>,
    description: String,
    parameters: serde_json::Value,
}

/// Shared thread-safe registry of allowed MCP tools and their lifecycle adapters.
#[derive(Default)]
pub(crate) struct McpPermissionTable {
    tools: RwLock<HashMap<String, McpToolEntry>>,
}

impl McpPermissionTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn update_server_tools(
        &self,
        server_id: &str,
        new_tools: impl IntoIterator<Item = (String, McpToolInfo)>,
    ) {
        self.update_server_catalog(
            server_id,
            new_tools
                .into_iter()
                .map(|(name, info)| (name, info, String::new(), serde_json::json!({}))),
        );
    }

    pub(crate) fn update_server_catalog(
        &self,
        server_id: &str,
        new_tools: impl IntoIterator<Item = (String, McpToolInfo, String, serde_json::Value)>,
    ) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, entry| entry.adapter.info.server_id != server_id);
        guard.extend(
            new_tools
                .into_iter()
                .map(|(name, info, description, parameters)| {
                    (
                        name,
                        McpToolEntry {
                            adapter: Arc::new(McpToolAdapter::new(info)),
                            description,
                            parameters,
                        },
                    )
                }),
        );
    }

    pub(crate) fn remove_server(&self, server_id: &str) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, entry| entry.adapter.info.server_id != server_id);
    }

    /// Return all currently registered MCP tools classified as `KnownReadOnly`, sorted by name.
    pub(crate) fn readonly_tools(&self) -> Vec<McpReadOnlyToolDescriptor> {
        let guard = self.tools.read().expect("mcp permission table lock");
        let mut tools: Vec<McpReadOnlyToolDescriptor> = guard
            .iter()
            .filter(|(_, entry)| entry.adapter.info.category == ToolPermissionClass::KnownReadOnly)
            .map(|(name, entry)| McpReadOnlyToolDescriptor {
                name: name.clone(),
                server_id: entry.adapter.info.server_id.clone(),
                description: entry.description.clone(),
                parameters: entry.parameters.clone(),
            })
            .collect();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    /// Look up one MCP tool only if it is currently classified as `KnownReadOnly`.
    pub(crate) fn readonly_tool(&self, name: &str) -> Option<McpReadOnlyToolDescriptor> {
        let guard = self.tools.read().expect("mcp permission table lock");
        let entry = guard.get(name)?;
        if entry.adapter.info.category != ToolPermissionClass::KnownReadOnly {
            return None;
        }
        Some(McpReadOnlyToolDescriptor {
            name: name.to_owned(),
            server_id: entry.adapter.info.server_id.clone(),
            description: entry.description.clone(),
            parameters: entry.parameters.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_mcp_arguments_within_limit_outputs_compact_json() {
        let val = json!({
            "key": "value",
            "nested": {
                "num": 42,
                "bool": true,
            }
        });
        let formatted = format_mcp_arguments(&val);
        assert_eq!(
            formatted,
            r#"{"key":"value","nested":{"bool":true,"num":42}}"#
        );
        assert!(!formatted.ends_with('…'));
        assert!(formatted.len() <= 1024);
    }

    #[test]
    fn format_mcp_arguments_exceeding_1024_bytes_truncates_with_ellipsis() {
        let long_string = "a".repeat(1200);
        let val = json!({ "data": long_string });
        let formatted = format_mcp_arguments(&val);
        assert!(formatted.ends_with('…'));
        let without_ellipsis = formatted.strip_suffix('…').unwrap();
        assert_eq!(without_ellipsis.len(), 1024);
    }

    #[test]
    fn format_mcp_arguments_multibyte_utf8_boundary_truncation() {
        let padding = "a".repeat(1015);
        let val = json!({ "msg": format!("{padding}中extra") });
        let full = serde_json::to_string(&val).unwrap();
        assert!(!full.is_char_boundary(1024));

        let formatted = format_mcp_arguments(&val);
        assert!(formatted.ends_with('…'));
        let without_ellipsis = formatted.strip_suffix('…').unwrap();
        assert_eq!(without_ellipsis.len(), 1023);
        assert!(without_ellipsis.is_char_boundary(without_ellipsis.len()));
        assert_eq!(&without_ellipsis[without_ellipsis.len() - 1..], "a");
    }
}
