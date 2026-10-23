//! Permission processor applying mode policies and managing confirmation flows.

use super::approval::{
    ApprovalDecision, ApprovalReply, ApprovalRequest, ApprovalSubject, ApprovalsHub,
    format_mcp_arguments,
};
use super::mode::PermissionMode;
use super::precheck::resolve_call;
use super::view::PermissionView;
use crate::session::ToolReporter;
use crate::session::WorkRef;
use async_trait::async_trait;
use causa::kernel::{
    CallControl, InvocationId, ProcessorContext, ProcessorError, ToolBatch, ToolBatchProcessor,
    ToolResultStatus,
};
use std::sync::Arc;
use tokio::sync::oneshot;

/// Categorization of a tool call for permission mode checking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolPermissionClass {
    /// Read-only built-in tool or an MCP tool under writes declaring read_only without destructive.
    KnownReadOnly,
    /// Built-in workspace write tools: `write_file`, `edit_file`.
    BuiltinWrite,
    /// MCP tool requiring confirmation: prompt approval, or writes approval without read-only or with destructive.
    McpPrompt,
    /// MCP tool configured with approve approval.
    McpApprove,
}

/// Verdict returned when evaluating a tool category under an active permission mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionVerdict {
    /// Execute directly without asking.
    Execute,
    /// Ask for confirmation before execution.
    Ask,
    /// Reject the call without asking.
    Reject,
}

/// Evaluates a tool category against the active permission mode according to the policy table.
pub(crate) fn decide(mode: PermissionMode, category: ToolPermissionClass) -> PermissionVerdict {
    match mode {
        PermissionMode::ReadOnly => match category {
            ToolPermissionClass::KnownReadOnly => PermissionVerdict::Execute,
            ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::McpPrompt
            | ToolPermissionClass::McpApprove => PermissionVerdict::Reject,
        },
        PermissionMode::Ask => match category {
            ToolPermissionClass::KnownReadOnly | ToolPermissionClass::McpApprove => {
                PermissionVerdict::Execute
            }
            ToolPermissionClass::BuiltinWrite | ToolPermissionClass::McpPrompt => {
                PermissionVerdict::Ask
            }
        },
        PermissionMode::AcceptEdits => match category {
            ToolPermissionClass::KnownReadOnly
            | ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::McpApprove => PermissionVerdict::Execute,
            ToolPermissionClass::McpPrompt => PermissionVerdict::Ask,
        },
        PermissionMode::FullAccess => PermissionVerdict::Execute,
    }
}

/// The specific permission decision recorded for a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionDecision {
    /// Mode active when the decision was made.
    pub mode: PermissionMode,
    /// Revision of the permission state when the decision was made.
    pub revision: u64,
    /// Kind of permission decision rendered.
    pub kind: PermissionDecisionKind,
}

/// Kind of permission decision rendered for a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecisionKind {
    /// Direct execution without asking.
    DirectExecution,
    /// Execution approved after asking.
    AskedAndAllowed,
    /// Call rejected after asking.
    AskedAndDenied,
    /// Call rejected directly by permission mode policy.
    ModeRefused,
    /// Write call rejected because no prepared write record was found.
    Unprepared,
    /// Call rejected because no confirmation route was available.
    NoConfirmationRoute,
    /// Call rejected because the tool is unknown or unavailable.
    Unavailable,
}

/// Processor evaluating tool calls against active permission mode and requesting approvals.
pub(crate) struct PermissionProcessor {
    work: WorkRef,
    reporter: ToolReporter,
    hub: Arc<ApprovalsHub>,
    prepared: Arc<
        std::sync::Mutex<
            std::collections::HashMap<causa::kernel::BlockId, crate::tools::PreparedWrite>,
        >,
    >,
}

impl PermissionProcessor {
    pub(crate) fn new(
        work: WorkRef,
        reporter: ToolReporter,
        hub: Arc<ApprovalsHub>,
        prepared: Arc<
            std::sync::Mutex<
                std::collections::HashMap<causa::kernel::BlockId, crate::tools::PreparedWrite>,
            >,
        >,
    ) -> Self {
        Self {
            work,
            reporter,
            hub,
            prepared,
        }
    }
}

#[async_trait]
impl ToolBatchProcessor for PermissionProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        let invocation_id = InvocationId {
            turn_id: ctx.turn_id.clone(),
            round_id: ctx.round_id,
        };

        let view = ctx
            .data::<PermissionView>()
            .ok_or_else(|| ProcessorError::Failed("permission view not found in context".into()))?;

        for &call_block_id in ctx.declaration_order {
            if ctx.control.check().is_err() {
                return Ok(());
            }

            let pending_entry = batch
                .calls()
                .iter()
                .find(|entry| entry.call().call_block_id == call_block_id);
            let Some(entry) = pending_entry else {
                continue;
            };
            let call = entry.call().clone();
            let tool_name = call.input.tool_name.clone();

            let (mode, revision) = view.get();

            let Some(category) = view.classify_tool(&tool_name) else {
                self.reporter.record_permission_decision(
                    &call,
                    &invocation_id,
                    PermissionDecision {
                        mode,
                        revision,
                        kind: PermissionDecisionKind::Unavailable,
                    },
                );
                resolve_call(
                    batch,
                    call_block_id,
                    ToolResultStatus::Rejected,
                    serde_json::json!({
                        "error": format!("tool '{tool_name}' is not available")
                    }),
                )?;
                continue;
            };

            let verdict = decide(mode, category);
            match verdict {
                PermissionVerdict::Execute => {
                    self.reporter.record_permission_decision(
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind: PermissionDecisionKind::DirectExecution,
                        },
                    );
                }
                PermissionVerdict::Reject => {
                    self.reporter.record_permission_decision(
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind: PermissionDecisionKind::ModeRefused,
                        },
                    );
                    let msg = if category == ToolPermissionClass::BuiltinWrite {
                        format!("the current permission mode '{mode}' does not allow this write")
                    } else {
                        format!(
                            "the current permission mode '{mode}' does not allow this MCP tool call"
                        )
                    };
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": msg }),
                    )?;
                }
                PermissionVerdict::Ask => {
                    let (subject, is_mcp) = match category {
                        ToolPermissionClass::BuiltinWrite => {
                            let prepared = {
                                let guard = self.prepared.lock().unwrap();
                                guard.get(&call_block_id).cloned()
                            };
                            let Some(prepared) = prepared else {
                                self.reporter.record_permission_decision(
                                    &call,
                                    &invocation_id,
                                    PermissionDecision {
                                        mode,
                                        revision,
                                        kind: PermissionDecisionKind::Unprepared,
                                    },
                                );
                                resolve_call(
                                    batch,
                                    call_block_id,
                                    ToolResultStatus::Rejected,
                                    serde_json::json!({
                                        "error": "write operation was not prepared by precheck"
                                    }),
                                )?;
                                continue;
                            };
                            (
                                ApprovalSubject::FileWrite {
                                    path: prepared.path.clone(),
                                    action: prepared.action,
                                    diff: prepared.diff.clone(),
                                },
                                false,
                            )
                        }
                        ToolPermissionClass::McpPrompt
                        | ToolPermissionClass::KnownReadOnly
                        | ToolPermissionClass::McpApprove => {
                            let (server, tool) = view.mcp_tool_info(&tool_name).map_or_else(
                                || ("unknown".to_owned(), tool_name.clone()),
                                |info| (info.server_id, info.raw_tool_name),
                            );
                            let arguments = format_mcp_arguments(&call.input.arguments);
                            (
                                ApprovalSubject::McpCall {
                                    server,
                                    tool,
                                    arguments,
                                },
                                true,
                            )
                        }
                    };

                    let Some(sender) = self.hub.get_sender().filter(|sender| !sender.is_closed())
                    else {
                        self.reporter.record_permission_decision(
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                            },
                        );
                        resolve_call(
                            batch,
                            call_block_id,
                            ToolResultStatus::Rejected,
                            serde_json::json!({ "error": "no confirmation route available" }),
                        )?;
                        continue;
                    };

                    let (reply_tx, reply_rx) = oneshot::channel();
                    let request = ApprovalRequest {
                        work: self.work.clone(),
                        call_block_id,
                        tool_name: tool_name.clone(),
                        mode,
                        revision,
                        subject,
                        reply: ApprovalReply::new(reply_tx),
                    };

                    let send_res = tokio::select! {
                        biased;
                        _ = wait_for_control_stop(ctx.control) => return Ok(()),
                        res = sender.send(request) => res,
                    };

                    if send_res.is_err() {
                        self.reporter.record_permission_decision(
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                            },
                        );
                        resolve_call(
                            batch,
                            call_block_id,
                            ToolResultStatus::Rejected,
                            serde_json::json!({ "error": "no confirmation route available" }),
                        )?;
                        continue;
                    }

                    let reply_res = tokio::select! {
                        biased;
                        _ = wait_for_control_stop(ctx.control) => return Ok(()),
                        res = reply_rx => res,
                    };

                    match reply_res {
                        Ok(ApprovalDecision::Allow) => {
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                },
                            );
                        }
                        Ok(ApprovalDecision::Deny { reason }) => {
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndDenied,
                                },
                            );
                            let msg = if is_mcp {
                                match reason {
                                    Some(r) => format!("user declined this tool call: {r}"),
                                    None => "user declined this tool call".to_string(),
                                }
                            } else {
                                match reason {
                                    Some(r) => format!("user declined this write: {r}"),
                                    None => "user declined this write".to_string(),
                                }
                            };
                            resolve_call(
                                batch,
                                call_block_id,
                                ToolResultStatus::Rejected,
                                serde_json::json!({ "error": msg }),
                            )?;
                        }
                        Err(_) => {
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::NoConfirmationRoute,
                                },
                            );
                            resolve_call(
                                batch,
                                call_block_id,
                                ToolResultStatus::Rejected,
                                serde_json::json!({ "error": "no confirmation route available" }),
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

async fn wait_for_control_stop(control: &CallControl) {
    let cancel = control.cancellation_token().cancelled();
    if let Some(deadline) = control.deadline() {
        let sleep = tokio::time::sleep_until(deadline.into());
        tokio::select! {
            biased;
            _ = cancel => {}
            _ = sleep => {}
        }
    } else {
        cancel.await;
    }
}
