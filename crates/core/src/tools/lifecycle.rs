//! Tool lifecycle contract separating tool-owned facts/preparation from policy orchestration.

use crate::definition::CommandNetwork;
use crate::policy::{ToolPermissionClass, WriteAction};
use async_trait::async_trait;
use causa::kernel::{BlockId, CallControl, ToolCallContext, ToolOutput, ToolResultStatus};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Zero-I/O intent reported by a tool for global permission evaluation and rule matching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolCallIntent {
    /// Permission class governing this call.
    pub(crate) class: ToolPermissionClass,
    /// Raw command text for `CommandRuleSet` matching (`run_command` only).
    pub(crate) command_subject: Option<String>,
}

/// Single-call execution authorization issued by the policy orchestrator on approval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ExecutionGrant {
    /// Whether this call is authorized to run outside the sandbox.
    pub(crate) allow_outside_sandbox: bool,
}

/// Target environment preview for a command awaiting confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CommandPreviewTarget {
    /// Inside the platform sandbox with the session's network policy.
    Sandboxed {
        /// Network policy for this command.
        network: CommandNetwork,
    },
    /// Outside the sandbox with the model's justification.
    Outside {
        /// Model-provided reason for requesting outside execution.
        justification: String,
    },
}

/// Lightweight, read-only preview constructed from prepared state for user approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalPreview {
    /// Workspace file write or edit preview.
    FileWrite {
        /// Target file path relative to workspace root.
        path: String,
        /// Whether the file will be created or updated.
        action: WriteAction,
        /// Unified diff of the proposed change.
        diff: String,
    },
    /// Shell command preview.
    Command {
        /// Command string to run.
        command: String,
        /// Workspace-relative working directory, or `"."`.
        workdir: String,
        /// Effective timeout in seconds.
        timeout_secs: u64,
        /// Sandbox / outside-sandbox target details.
        target: CommandPreviewTarget,
    },
    /// MCP tool invocation preview.
    McpCall {
        /// Configured MCP server identifier.
        server: String,
        /// Raw MCP tool name advertised by the server.
        tool: String,
        /// Compact, bounded JSON arguments string.
        arguments: String,
    },
}

/// Opaque preflight artifact produced by [`ManagedTool::prepare`] and consumed by
/// [`ManagedTool::approval_preview`] / [`ManagedTool::commit_prepared`].
///
/// The inner payload stays completely private to the owning tool implementation.
pub(crate) struct PreparedCall(Option<Box<dyn Any + Send + Sync>>);

impl PreparedCall {
    /// An empty prepared state for tools without preflight artifacts.
    pub(crate) fn empty() -> Self {
        Self(None)
    }

    /// Wrap a tool-private preflight artifact.
    pub(crate) fn new<T: Any + Send + Sync>(value: T) -> Self {
        Self(Some(Box::new(value)))
    }

    /// Borrow the underlying tool-private artifact if it has type `T`.
    pub(crate) fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.0.as_deref().and_then(|any| any.downcast_ref::<T>())
    }

    /// Extract the underlying tool-private artifact if it has type `T`.
    pub(crate) fn downcast<T: Any>(self) -> Option<T> {
        self.0
            .and_then(|any| any.downcast::<T>().ok())
            .map(|boxed| *boxed)
    }
}

/// Failure produced by [`ManagedTool::prepare`] that immediately settles the call.
#[derive(Debug)]
pub(crate) struct PrepareRejection {
    /// Terminal status for the rejected/failed call.
    pub(crate) status: ToolResultStatus,
    /// Structured JSON output payload.
    pub(crate) output: ToolOutput,
}

impl PrepareRejection {
    /// Construct a `Rejected` preparation failure with `{"error": message}`.
    pub(crate) fn rejected(message: impl Into<String>) -> Self {
        Self {
            status: ToolResultStatus::Rejected,
            output: ToolOutput::new(serde_json::json!({ "error": message.into() })),
        }
    }

    /// Construct a preparation failure with explicit status and output.
    pub(crate) fn with_output(status: ToolResultStatus, output: ToolOutput) -> Self {
        Self { status, output }
    }
}

/// Per-batch shared preflight state (e.g. duplicate write target detection).
#[derive(Default)]
pub(crate) struct BatchPrepareState {
    seen_write_paths: HashSet<PathBuf>,
}

impl BatchPrepareState {
    /// Create empty per-batch preparation state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record a normalized workspace-relative write path for this batch.
    ///
    /// Returns `false` if this path was already prepared earlier in the same batch.
    pub(crate) fn record_write_path(&mut self, path: impl Into<PathBuf>) -> bool {
        self.seen_write_paths.insert(path.into())
    }
}

/// Polymorphic tool lifecycle contract used by the unified permission orchestrator.
///
/// Tools declare facts, validate/prepare once, produce approval previews, and
/// commit prepared state only when explicitly authorized. They never inspect
/// `PermissionMode` directly.
#[async_trait]
pub(crate) trait ManagedTool: Send + Sync {
    /// Zero-I/O classification of a call for fast gating and command-rule matching.
    fn classify(&self, arguments: &serde_json::Value) -> ToolCallIntent;

    /// Validate parameters and perform single-pass preflight preparation.
    #[allow(clippy::result_large_err)]
    async fn prepare(
        &self,
        _call: &ToolCallContext,
        _batch_state: &mut BatchPrepareState,
        _control: &CallControl,
    ) -> Result<PreparedCall, PrepareRejection> {
        Ok(PreparedCall::empty())
    }

    /// Build a lightweight approval preview from the call and its prepared state.
    ///
    /// Tools that do not require confirmation return `None` by default.
    fn approval_preview(
        &self,
        _call: &ToolCallContext,
        _prepared: &PreparedCall,
    ) -> Option<ApprovalPreview> {
        None
    }

    /// Commit an approved prepared call into the tool's private execution slot.
    fn commit_prepared(
        &self,
        _call_block_id: BlockId,
        _prepared: PreparedCall,
        _grant: ExecutionGrant,
    ) {
    }

    /// Discard a previously committed prepared call if the batch is cancelled before dispatch.
    fn discard_prepared(&self, _call_block_id: BlockId) {}
}

/// Dynamic lookup provider for runtime-registered managed tools (such as MCP servers).
pub(crate) trait DynamicToolCatalog: Send + Sync {
    /// Look up a dynamic managed tool by its model-facing name.
    fn get(&self, name: &str) -> Option<Arc<dyn ManagedTool>>;
}

/// Per-work registry mapping bound model-facing tool names to their [`ManagedTool`] adapters.
#[derive(Default)]
pub(crate) struct ToolCatalog {
    static_tools: HashMap<String, Arc<dyn ManagedTool>>,
    dynamic_catalog: Option<Arc<dyn DynamicToolCatalog>>,
}

impl ToolCatalog {
    /// Create an empty static tool catalog.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Create a catalog backed by a dynamic tool catalog (e.g. MCP).
    pub(crate) fn with_dynamic(dynamic: Arc<dyn DynamicToolCatalog>) -> Self {
        Self {
            static_tools: HashMap::new(),
            dynamic_catalog: Some(dynamic),
        }
    }

    /// Register a statically bound managed tool by model-facing name.
    pub(crate) fn register(&mut self, name: impl Into<String>, tool: Arc<dyn ManagedTool>) {
        self.static_tools.insert(name.into(), tool);
    }

    /// Look up a managed tool by model-facing name.
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn ManagedTool>> {
        if let Some(tool) = self.static_tools.get(name) {
            return Some(tool.clone());
        }
        self.dynamic_catalog.as_ref()?.get(name)
    }
}
