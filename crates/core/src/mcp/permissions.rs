//! Shared MCP tool permission classification registry and lifecycle adapter.

use crate::definition::McpApproval;
use crate::policy::ToolPermissionClass;
use crate::tools::{
    ApprovalPreview, DynamicToolCatalog, ManagedTool, PreparedCall, ToolCallIntent,
};
use async_trait::async_trait;
use causa::kernel::{ToolCallContext, ToolHints};
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

/// Derive the TARS permission class for an MCP tool from frozen server config and server hints.
///
/// Server hints can only relax confirmation when frozen configuration selects `writes`
/// and the tool explicitly declares `read_only = true` without `destructive = true`.
/// When frozen configuration selects `prompt` (`ask`) or `approve`, server hints cannot override it.
pub(crate) fn classify_mcp_tool(approval: McpApproval, hints: &ToolHints) -> ToolPermissionClass {
    match approval {
        McpApproval::Approve => ToolPermissionClass::McpApprove,
        McpApproval::Prompt => ToolPermissionClass::McpPrompt,
        McpApproval::Writes => {
            if hints.read_only == Some(true) && hints.destructive != Some(true) {
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

/// Shared thread-safe registry of allowed MCP tools and their lifecycle adapters.
#[derive(Default)]
pub(crate) struct McpPermissionTable {
    tools: RwLock<HashMap<String, Arc<McpToolAdapter>>>,
}

impl McpPermissionTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn update_server_tools(
        &self,
        server_id: &str,
        new_tools: impl IntoIterator<Item = (String, McpToolInfo)>,
    ) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, adapter| adapter.info.server_id != server_id);
        guard.extend(
            new_tools
                .into_iter()
                .map(|(name, info)| (name, Arc::new(McpToolAdapter::new(info)))),
        );
    }

    pub(crate) fn remove_server(&self, server_id: &str) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, adapter| adapter.info.server_id != server_id);
    }
}

impl DynamicToolCatalog for McpPermissionTable {
    fn get(&self, name: &str) -> Option<Arc<dyn ManagedTool>> {
        self.tools
            .read()
            .expect("mcp permission table lock")
            .get(name)
            .map(|adapter| adapter.clone() as Arc<dyn ManagedTool>)
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
