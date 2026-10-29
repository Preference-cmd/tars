//! Shared instance permission state, views, and control handles.

use super::approval::{ApprovalReceiver, ApprovalsHub};
use super::mode::PermissionMode;
use super::processor::ToolPermissionClass;
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

struct PermissionStateInner {
    mode: PermissionMode,
    revision: u64,
}

/// Read-only snapshot access to an instance's active permission mode, revision, and tool registry.
#[derive(Clone)]
pub struct PermissionView {
    state: Arc<RwLock<PermissionStateInner>>,
    mcp_table: Arc<McpPermissionTable>,
}

impl PermissionView {
    /// Read the current permission mode and revision together consistently.
    pub fn get(&self) -> (PermissionMode, u64) {
        let guard = self.state.read().expect("permission lock");
        (guard.mode, guard.revision)
    }

    /// Read the current permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.get().0
    }

    /// Read the current revision number.
    pub fn revision(&self) -> u64 {
        self.get().1
    }

    /// Classify a tool call by name and arguments.
    ///
    /// The gate and the permission processor both use this so a per-call
    /// outside-sandbox request cannot be classified differently in the two
    /// places.
    pub(crate) fn classify_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Option<ToolPermissionClass> {
        if name == "run_command" && crate::tools::parameters::requests_outside_sandbox(arguments) {
            return Some(ToolPermissionClass::CommandOutsideSandbox);
        }
        self.classify_tool(name)
    }

    /// Classify a tool by name, checking built-in workspace tools first,
    /// then the registered MCP tools.
    pub(crate) fn classify_tool(&self, name: &str) -> Option<ToolPermissionClass> {
        if crate::definition::WorkspaceTool::is_builtin_name(name) {
            match name {
                "list_directory" | "read_file" | "search_text" => {
                    Some(ToolPermissionClass::KnownReadOnly)
                }
                "write_file" | "edit_file" => Some(ToolPermissionClass::BuiltinWrite),
                "run_command" => Some(ToolPermissionClass::BuiltinCommand),
                _ => None,
            }
        } else {
            self.mcp_table.get(name).map(|info| info.category)
        }
    }

    /// Look up MCP tool registration details (server ID and original tool name).
    pub(crate) fn mcp_tool_info(&self, name: &str) -> Option<McpToolInfo> {
        self.mcp_table.get(name)
    }
}

/// Instance-level hub managing permission mode state, revisions, and approvals.
pub(crate) struct PermissionHub {
    state: Arc<RwLock<PermissionStateInner>>,
    approvals: Arc<ApprovalsHub>,
    mcp_table: Arc<McpPermissionTable>,
}

impl PermissionHub {
    #[cfg(test)]
    pub(crate) fn new(initial_mode: PermissionMode) -> Self {
        Self::with_mcp_table(initial_mode, Arc::new(McpPermissionTable::new()))
    }

    pub(crate) fn with_mcp_table(
        initial_mode: PermissionMode,
        mcp_table: Arc<McpPermissionTable>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(PermissionStateInner {
                mode: initial_mode,
                revision: 0,
            })),
            approvals: Arc::new(ApprovalsHub::new()),
            mcp_table,
        }
    }

    pub(crate) fn view(&self) -> PermissionView {
        PermissionView {
            state: self.state.clone(),
            mcp_table: self.mcp_table.clone(),
        }
    }

    pub(crate) fn approvals(&self) -> Arc<ApprovalsHub> {
        self.approvals.clone()
    }

    pub(crate) fn get(&self) -> (PermissionMode, u64) {
        let guard = self.state.read().expect("permission lock");
        (guard.mode, guard.revision)
    }

    pub(crate) fn set(&self, mode: PermissionMode) -> (PermissionMode, u64) {
        let mut guard = self.state.write().expect("permission lock");
        if guard.mode != mode {
            guard.mode = mode;
            guard.revision += 1;
        }
        (guard.mode, guard.revision)
    }

    pub(crate) fn connect_approvals(&self) -> ApprovalReceiver {
        self.approvals.connect()
    }
}
