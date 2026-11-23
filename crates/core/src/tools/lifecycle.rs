//! Tool lifecycle contract separating tool-owned facts/preparation from policy orchestration.

use crate::definition::CommandNetwork;
use crate::policy::{ToolPermissionClass, WriteAction};
use crate::workspace::FileIdentity;
use async_trait::async_trait;
use causa::kernel::{
    CallAttachments, CallControl, ToolCallContext, ToolMetadata, ToolOutput, ToolResultStatus,
};
use std::any::Any;
use std::collections::HashSet;
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
/// [`ManagedTool::approval_preview`] / [`ManagedTool::attach_prepared`].
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

/// Canonical target key used to deduplicate write and edit calls within one batch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum WriteTargetKey {
    /// Existing regular file identified by device and inode pair.
    Existing(FileIdentity),
    /// New file not yet present on disk, identified by normalized parent path and file name.
    New {
        /// Normalized workspace-relative parent directory components.
        parent_components: Vec<String>,
        /// Target file name.
        file_name: String,
    },
}

/// Per-batch shared preflight state (e.g. duplicate write target detection).
#[derive(Default)]
pub(crate) struct BatchPrepareState {
    seen_write_targets: HashSet<WriteTargetKey>,
}

impl BatchPrepareState {
    /// Create empty per-batch preparation state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Snapshot the set of write targets already prepared in this batch.
    pub(crate) fn seen_write_targets(&self) -> HashSet<WriteTargetKey> {
        self.seen_write_targets.clone()
    }

    /// Record a write target key for this batch.
    ///
    /// Returns `false` if this target was already prepared earlier in the same batch.
    pub(crate) fn record_write_target(&mut self, target: WriteTargetKey) -> bool {
        self.seen_write_targets.insert(target)
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

    /// Attach an approved prepared call and execution grant onto the call's
    /// non-persisted [`CallAttachments`] for consumption in `Tool::execute`.
    fn attach_prepared(
        &self,
        _attachments: &mut CallAttachments,
        _prepared: PreparedCall,
        _grant: ExecutionGrant,
    ) {
    }
}

/// Stateless [`ManagedTool`] for read-only built-in tools without preflight work.
pub(crate) struct ReadOnlyManagedTool;

#[async_trait]
impl ManagedTool for ReadOnlyManagedTool {
    fn classify(&self, _arguments: &serde_json::Value) -> ToolCallIntent {
        ToolCallIntent {
            class: ToolPermissionClass::KnownReadOnly,
            command_subject: None,
        }
    }
}

/// Build [`ToolMetadata`] carrying a [`ManagedTool`] handle for `ToolPolicyProcessor`.
pub(crate) fn managed_tool_metadata(tool: Arc<dyn ManagedTool>) -> ToolMetadata {
    ToolMetadata::new().with(tool)
}
