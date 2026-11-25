use super::definitions::collaboration_tool_definitions;
use crate::agents::{AgentPreset, ModelConfigInput, resolve_model_override};
use crate::application::{
    CoordinatorError, Entry, MessageAgentRequest, OperationOutcome, SpawnAgentOutcome,
    SpawnAgentRequest,
};
use crate::definition::{
    BuiltinTool, ChildNarrowingRequest, SessionDefinition, WorkspaceTool, permission_mode_rank,
};
use crate::events::{
    AgentInteraction, MAX_READ_AGENT_PAGE_BYTES, OnParentComplete, OperationExecutionMode,
    OperationStatus,
};
use crate::permission::PermissionMode;
use async_trait::async_trait;
use causa::kernel::{
    CallControl, Tool, ToolCallContext, ToolDefinition, ToolMetadata, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex, Weak};

const MAX_TIMEOUT_SECS: u64 = 86_400;

/// Trusted runtime caller identity bound at assembly time (never supplied by model arguments).
#[derive(Clone)]
pub(crate) struct CollaborationContext {
    entry: Weak<Entry>,
    caller_agent_id: String,
    caller_operation_id: Arc<Mutex<Option<String>>>,
}

impl CollaborationContext {
    /// Creates a collaboration context for the session's root/entry agent (`caller_operation_id` is `None`).
    pub(crate) fn for_entry(entry: Weak<Entry>, entry_agent_id: String) -> Self {
        Self {
            entry,
            caller_agent_id: entry_agent_id,
            caller_operation_id: Arc::new(Mutex::new(None)),
        }
    }

    /// Creates a collaboration context for a child agent whose active `operation_id` is updated per run.
    pub(crate) fn for_child(
        entry: Weak<Entry>,
        child_agent_id: String,
        caller_operation_id: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            entry,
            caller_agent_id: child_agent_id,
            caller_operation_id,
        }
    }

    fn active_operation_id(&self) -> Option<String> {
        self.caller_operation_id
            .lock()
            .expect("caller operation slot lock")
            .clone()
    }
}

/// Constructs the 4 collaboration tools bound to `ctx`.
pub(crate) fn collaboration_tools(ctx: CollaborationContext) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SpawnAgentTool { ctx: ctx.clone() }),
        Arc::new(MessageAgentTool { ctx: ctx.clone() }),
        Arc::new(ReadAgentTool { ctx: ctx.clone() }),
        Arc::new(CancelAgentTool { ctx }),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArgs {
    agent: String,
    prompt: String,
    #[serde(default)]
    model: Option<ModelConfigInput>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    reusable: Option<bool>,
    #[serde(default)]
    on_parent_complete: Option<OnParentComplete>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_rounds: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageAgentArgs {
    agent_id: String,
    prompt: String,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    on_parent_complete: Option<OnParentComplete>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_rounds: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadAgentArgs {
    operation_id: String,
    #[serde(default)]
    offset_bytes: Option<usize>,
    #[serde(default)]
    limit_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelAgentArgs {
    operation_id: String,
}

struct SpawnAgentTool {
    ctx: CollaborationContext,
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn definition(&self) -> ToolDefinition {
        collaboration_tool_definitions()
            .into_iter()
            .nth(0)
            .expect("spawn_agent definition")
    }

    fn metadata(&self) -> ToolMetadata {
        crate::tools::managed_tool_metadata(Arc::new(crate::tools::ReadOnlyManagedTool))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return stopped_result(call, control);
        }
        let args: SpawnAgentArgs = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(args) => args,
            Err(err) => {
                return error_result(
                    call,
                    ToolResultStatus::Rejected,
                    format!("invalid spawn_agent arguments: {err}"),
                );
            }
        };
        if args.agent.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "spawn_agent 'agent' must not be empty",
            );
        }
        if args.prompt.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "spawn_agent 'prompt' must not be empty",
            );
        }
        if args.max_rounds == Some(0) {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "spawn_agent 'max_rounds' must be positive",
            );
        }
        if let Some(secs) = args.timeout_secs
            && !(1..=MAX_TIMEOUT_SECS).contains(&secs)
        {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "spawn_agent 'timeout_secs' must be in 1..=86400",
            );
        }

        let Some(entry) = self.ctx.entry.upgrade() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session instance is no longer active",
            );
        };
        let Some(coord) = entry.coordinator.get().cloned() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session coordinator is unavailable",
            );
        };

        let preset = {
            let guard = entry.agent_registry.read().expect("agent registry lock");
            guard.get(args.agent.trim()).cloned()
        };
        let Some(preset) = preset else {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                format!("unknown or untrusted agent preset '{}'", args.agent),
            );
        };

        let (caller_def, caller_live_mode) =
            match coord.caller_definition_and_mode(&self.ctx.caller_agent_id) {
                Ok(pair) => pair,
                Err(err) => return coordinator_error_result(call, err),
            };

        let model_override = match resolve_model_override(
            &caller_def,
            preset.model.as_ref(),
            args.model.as_ref(),
            &entry.model_profiles,
        ) {
            Ok(over) => over,
            Err(err) => return error_result(call, ToolResultStatus::Rejected, err.to_string()),
        };

        let narrowing =
            match build_narrowing_request(&caller_def, caller_live_mode, &preset, model_override) {
                Ok(req) => req,
                Err(msg) => return error_result(call, ToolResultStatus::Rejected, msg),
            };

        let interaction = match args.reusable {
            Some(true) => AgentInteraction::Reusable,
            Some(false) => AgentInteraction::SingleWork,
            None => preset.default_interaction,
        };
        let execution_mode = if args.background.unwrap_or(false) {
            OperationExecutionMode::Background
        } else {
            OperationExecutionMode::Sync
        };
        let on_parent_complete = args
            .on_parent_complete
            .unwrap_or(OnParentComplete::Continue);

        let req = SpawnAgentRequest {
            caller_agent_id: self.ctx.caller_agent_id.clone(),
            caller_operation_id: self.ctx.active_operation_id(),
            source_block_id: Some(call.call_block_id.0.to_string()),
            name: preset.name.clone(),
            preset_name: Some(preset.name),
            role_prompt: preset.role_prompt,
            prompt: args.prompt,
            narrowing,
            interaction,
            execution_mode,
            on_parent_complete,
            timeout_secs: args.timeout_secs,
            max_rounds: args.max_rounds,
            coalesce_key: None,
        };

        match coord
            .spawn_agent_with_cancel(req, control.cancellation_token().clone())
            .await
        {
            Ok(outcome) => spawn_outcome_result(call, outcome),
            Err(err) => coordinator_error_result(call, err),
        }
    }
}

struct MessageAgentTool {
    ctx: CollaborationContext,
}

#[async_trait]
impl Tool for MessageAgentTool {
    fn definition(&self) -> ToolDefinition {
        collaboration_tool_definitions()
            .into_iter()
            .nth(1)
            .expect("message_agent definition")
    }

    fn metadata(&self) -> ToolMetadata {
        crate::tools::managed_tool_metadata(Arc::new(crate::tools::ReadOnlyManagedTool))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return stopped_result(call, control);
        }
        let args: MessageAgentArgs = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(args) => args,
            Err(err) => {
                return error_result(
                    call,
                    ToolResultStatus::Rejected,
                    format!("invalid message_agent arguments: {err}"),
                );
            }
        };
        if args.agent_id.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "message_agent 'agent_id' must not be empty",
            );
        }
        if args.prompt.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "message_agent 'prompt' must not be empty",
            );
        }
        if args.max_rounds == Some(0) {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "message_agent 'max_rounds' must be positive",
            );
        }
        if let Some(secs) = args.timeout_secs
            && !(1..=MAX_TIMEOUT_SECS).contains(&secs)
        {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "message_agent 'timeout_secs' must be in 1..=86400",
            );
        }

        let Some(entry) = self.ctx.entry.upgrade() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session instance is no longer active",
            );
        };
        let Some(coord) = entry.coordinator.get().cloned() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session coordinator is unavailable",
            );
        };

        let req = MessageAgentRequest {
            caller_agent_id: self.ctx.caller_agent_id.clone(),
            caller_operation_id: self.ctx.active_operation_id(),
            source_block_id: Some(call.call_block_id.0.to_string()),
            target_agent_id: args.agent_id,
            prompt: args.prompt,
            execution_mode: if args.background.unwrap_or(false) {
                OperationExecutionMode::Background
            } else {
                OperationExecutionMode::Sync
            },
            on_parent_complete: args
                .on_parent_complete
                .unwrap_or(OnParentComplete::Continue),
            timeout_secs: args.timeout_secs,
            max_rounds: args.max_rounds,
            coalesce_key: None,
        };

        match coord
            .message_agent_with_cancel(req, control.cancellation_token().clone())
            .await
        {
            Ok(outcome) => spawn_outcome_result(call, outcome),
            Err(err) => coordinator_error_result(call, err),
        }
    }
}

struct ReadAgentTool {
    ctx: CollaborationContext,
}

#[async_trait]
impl Tool for ReadAgentTool {
    fn definition(&self) -> ToolDefinition {
        collaboration_tool_definitions()
            .into_iter()
            .nth(2)
            .expect("read_agent definition")
    }

    fn metadata(&self) -> ToolMetadata {
        crate::tools::managed_tool_metadata(Arc::new(crate::tools::ReadOnlyManagedTool))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return stopped_result(call, control);
        }
        let args: ReadAgentArgs = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(args) => args,
            Err(err) => {
                return error_result(
                    call,
                    ToolResultStatus::Rejected,
                    format!("invalid read_agent arguments: {err}"),
                );
            }
        };
        if args.operation_id.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "read_agent 'operation_id' must not be empty",
            );
        }
        if args.limit_bytes == Some(0) {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "read_agent 'limit_bytes' must be positive when set",
            );
        }

        let Some(entry) = self.ctx.entry.upgrade() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session instance is no longer active",
            );
        };
        let Some(coord) = entry.coordinator.get().cloned() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session coordinator is unavailable",
            );
        };

        let offset = args.offset_bytes.unwrap_or(0);
        let limit = args.limit_bytes.unwrap_or(MAX_READ_AGENT_PAGE_BYTES);
        match coord.read_agent(&args.operation_id, offset, limit).await {
            Ok(res) => ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(json!({
                    "operation_id": res.operation_id,
                    "agent_id": res.target_agent_id,
                    "status": res.status.as_str(),
                    "content": res.page.content,
                    "offset_bytes": res.page.offset_bytes,
                    "next_offset_bytes": res.page.next_offset_bytes,
                    "total_bytes": res.page.total_bytes,
                    "has_more": res.page.has_more,
                    "error": res.error,
                })),
            ),
            Err(err) => coordinator_error_result(call, err),
        }
    }
}

struct CancelAgentTool {
    ctx: CollaborationContext,
}

#[async_trait]
impl Tool for CancelAgentTool {
    fn definition(&self) -> ToolDefinition {
        collaboration_tool_definitions()
            .into_iter()
            .nth(3)
            .expect("cancel_agent definition")
    }

    fn metadata(&self) -> ToolMetadata {
        crate::tools::managed_tool_metadata(Arc::new(crate::tools::ReadOnlyManagedTool))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return stopped_result(call, control);
        }
        let args: CancelAgentArgs = match serde_json::from_value(call.input.arguments.clone()) {
            Ok(args) => args,
            Err(err) => {
                return error_result(
                    call,
                    ToolResultStatus::Rejected,
                    format!("invalid cancel_agent arguments: {err}"),
                );
            }
        };
        if args.operation_id.trim().is_empty() {
            return error_result(
                call,
                ToolResultStatus::Rejected,
                "cancel_agent 'operation_id' must not be empty",
            );
        }

        let Some(entry) = self.ctx.entry.upgrade() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session instance is no longer active",
            );
        };
        let Some(coord) = entry.coordinator.get().cloned() else {
            return error_result(
                call,
                ToolResultStatus::Failed,
                "session coordinator is unavailable",
            );
        };

        match coord.cancel_agent(&args.operation_id).await {
            Ok(outcome) => ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(json!({
                    "operation_id": outcome.operation_id,
                    "agent_id": outcome.agent_id,
                    "status": outcome.status.as_str(),
                    "result_bytes": outcome.result_bytes,
                    "error": outcome.error,
                })),
            ),
            Err(err) => coordinator_error_result(call, err),
        }
    }
}

fn build_narrowing_request(
    caller_def: &SessionDefinition,
    caller_live_mode: PermissionMode,
    preset: &AgentPreset,
    model_override: Option<crate::definition::ChildModelOverride>,
) -> Result<ChildNarrowingRequest, String> {
    let inherited_live_mode = (permission_mode_rank(caller_live_mode)
        < permission_mode_rank(caller_def.initial_permission_mode()))
    .then_some(caller_live_mode);

    let max_model_rounds = preset
        .max_model_rounds
        .map(|r| r.min(caller_def.max_model_rounds));
    let work_timeout_secs = preset
        .timeout_secs
        .map(|t| match caller_def.work_timeout_secs {
            Some(parent_t) => t.min(parent_t),
            None => t,
        });

    if preset.is_builtin_readonly() {
        let readonly_tools: Vec<WorkspaceTool> = caller_def
            .tools
            .iter()
            .copied()
            .filter(|t| {
                matches!(
                    t,
                    WorkspaceTool::ReadFile
                        | WorkspaceTool::ListDirectory
                        | WorkspaceTool::SearchText
                        | WorkspaceTool::FindFiles
                )
            })
            .collect();
        return Ok(ChildNarrowingRequest {
            model_override,
            workspace_root: None,
            tools: Some(readonly_tools),
            tool_limits: None,
            write_limits: Some(None),
            command: Some(None),
            code_execution: None,
            mcp_server_ids: Some(Vec::new()),
            max_model_rounds,
            max_tool_calls: None,
            work_timeout_secs,
            permission_mode: Some(PermissionMode::ReadOnly),
        });
    }

    if let Some(preset_tools) = &preset.tools {
        let mut ws_tools = Vec::new();
        let mut wants_run_code = false;
        for &tool in preset_tools {
            match tool {
                BuiltinTool::Workspace(ws) => ws_tools.push(ws),
                BuiltinTool::RunCode => {
                    if caller_def.code_execution.is_none() {
                        return Err(
                            "preset requests run_code, which is not enabled on the caller".into(),
                        );
                    }
                    wants_run_code = true;
                }
                BuiltinTool::LoadSkill => {
                    // `load_skill` is currently bound on entry agents; child narrowing validates
                    // that the caller's `prelude.skills_enabled` allows skill access.
                    if !caller_def.prelude.skills_enabled {
                        return Err(
                            "preset requests load_skill, which is not enabled on the caller".into(),
                        );
                    }
                }
                BuiltinTool::Recall => {
                    // `recall` follows session persistence/compaction settings on assembled
                    // agents rather than per-preset toggles.
                }
            }
        }
        let code_execution = if wants_run_code { None } else { Some(None) };
        return Ok(ChildNarrowingRequest {
            model_override,
            workspace_root: None,
            tools: Some(ws_tools),
            tool_limits: None,
            write_limits: None,
            command: None,
            code_execution,
            mcp_server_ids: None,
            max_model_rounds,
            max_tool_calls: None,
            work_timeout_secs,
            permission_mode: inherited_live_mode,
        });
    }

    Ok(ChildNarrowingRequest {
        model_override,
        workspace_root: None,
        tools: None,
        tool_limits: None,
        write_limits: None,
        command: None,
        code_execution: None,
        mcp_server_ids: None,
        max_model_rounds,
        max_tool_calls: None,
        work_timeout_secs,
        permission_mode: inherited_live_mode,
    })
}

fn spawn_outcome_result(call: &ToolCallContext, outcome: SpawnAgentOutcome) -> ToolResultPayload {
    match outcome {
        SpawnAgentOutcome::Background(receipt) => {
            let status_str = if receipt.status.is_active() {
                "running"
            } else {
                receipt.status.as_str()
            };
            ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(json!({
                    "agent_id": receipt.agent_id,
                    "operation_id": receipt.operation_id,
                    "status": status_str,
                    "replayed": receipt.replayed,
                })),
            )
        }
        SpawnAgentOutcome::Completed(op) => completed_outcome_result(call, op),
    }
}

fn completed_outcome_result(call: &ToolCallContext, op: OperationOutcome) -> ToolResultPayload {
    let status = match op.status {
        OperationStatus::Completed => ToolResultStatus::Succeeded,
        OperationStatus::Cancelled => ToolResultStatus::Cancelled,
        OperationStatus::Deadline => ToolResultStatus::TimedOut,
        OperationStatus::Failed
        | OperationStatus::Faulted
        | OperationStatus::Interrupted
        | OperationStatus::Creating
        | OperationStatus::Running => ToolResultStatus::Failed,
    };
    ToolResultPayload::new(
        call.call_block_id,
        status,
        ToolOutput::new(json!({
            "agent_id": op.agent_id,
            "operation_id": op.operation_id,
            "status": op.status.as_str(),
            "result_text": op.result_text,
            "result_bytes": op.result_bytes,
            "error": op.error,
        })),
    )
}

fn coordinator_error_result(call: &ToolCallContext, err: CoordinatorError) -> ToolResultPayload {
    let status = match &err {
        CoordinatorError::Cancelled => ToolResultStatus::Cancelled,
        CoordinatorError::Narrowing(_)
        | CoordinatorError::DepthExceeded { .. }
        | CoordinatorError::CapabilityMismatch(_)
        | CoordinatorError::DedupConflict { .. }
        | CoordinatorError::PromptTooLarge { .. }
        | CoordinatorError::InputTooLarge { .. }
        | CoordinatorError::SelfDelegation
        | CoordinatorError::DelegationCycle
        | CoordinatorError::NotReusable(_) => ToolResultStatus::Rejected,
        _ => ToolResultStatus::Failed,
    };
    error_result(call, status, err.to_string())
}

fn stopped_result(call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
    let status = if control.is_cancelled() {
        ToolResultStatus::Cancelled
    } else {
        ToolResultStatus::TimedOut
    };
    error_result(call, status, "operation stopped before completion")
}

fn error_result(
    call: &ToolCallContext,
    status: ToolResultStatus,
    message: impl Into<String>,
) -> ToolResultPayload {
    ToolResultPayload::new(
        call.call_block_id,
        status,
        ToolOutput::new(json!({ "error": message.into() })),
    )
}
