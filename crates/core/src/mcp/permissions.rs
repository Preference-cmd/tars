//! Shared MCP tool permission classification registry.

use crate::policy::ToolPermissionClass;
use std::collections::HashMap;
use std::sync::RwLock;

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

/// Shared thread-safe registry of allowed MCP tools and their permission categories.
#[derive(Default)]
pub(crate) struct McpPermissionTable {
    tools: RwLock<HashMap<String, McpToolInfo>>,
}

impl McpPermissionTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self, name: &str) -> Option<McpToolInfo> {
        self.tools
            .read()
            .expect("mcp permission table lock")
            .get(name)
            .cloned()
    }

    pub(crate) fn update_server_tools(
        &self,
        server_id: &str,
        new_tools: impl IntoIterator<Item = (String, McpToolInfo)>,
    ) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, info| info.server_id != server_id);
        guard.extend(new_tools);
    }

    pub(crate) fn remove_server(&self, server_id: &str) {
        let mut guard = self.tools.write().expect("mcp permission table lock");
        guard.retain(|_, info| info.server_id != server_id);
    }
}
