//! Permission processor applying mode policies and managing confirmation flows.

use super::approval::{
    ApprovalDecision, ApprovalReply, ApprovalRequest, ApprovalSubject, CommandTarget,
    format_mcp_arguments,
};
use super::mode::PermissionMode;
use super::precheck::resolve_call;
use super::rules::{RuleScope, RuleSource, suggest_command_prefixes};
use super::view::{PermissionHub, PermissionView};
use crate::definition::{CommandSettings, WorkspaceTool};
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
    /// Built-in sandboxed command tool.
    BuiltinCommand,
    /// Built-in command tool requested to run outside the sandbox.
    CommandOutsideSandbox,
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
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::CommandOutsideSandbox
            | ToolPermissionClass::McpPrompt
            | ToolPermissionClass::McpApprove => PermissionVerdict::Reject,
        },
        PermissionMode::Ask => match category {
            ToolPermissionClass::KnownReadOnly | ToolPermissionClass::McpApprove => {
                PermissionVerdict::Execute
            }
            ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::CommandOutsideSandbox
            | ToolPermissionClass::McpPrompt => PermissionVerdict::Ask,
        },
        PermissionMode::AcceptEdits => match category {
            ToolPermissionClass::KnownReadOnly
            | ToolPermissionClass::BuiltinWrite
            | ToolPermissionClass::BuiltinCommand
            | ToolPermissionClass::McpApprove => PermissionVerdict::Execute,
            ToolPermissionClass::CommandOutsideSandbox | ToolPermissionClass::McpPrompt => {
                PermissionVerdict::Ask
            }
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
    /// Command rule provenance when a rule governed or modified the decision.
    pub rule: Option<RuleSource>,
}

impl PermissionDecision {
    /// Human-readable provenance summary when a command rule governed or
    /// prompted this decision, suitable for tool progress and transcript lines.
    pub fn rule_summary(&self) -> Option<String> {
        let source = self.rule.as_ref()?;
        match self.kind {
            PermissionDecisionKind::RuleAllowed => Some(format!("allowed by {source}")),
            PermissionDecisionKind::RuleDenied => Some(format!("denied by {source}")),
            PermissionDecisionKind::AskedAndAllowed => Some(format!("asked by {source} (allowed)")),
            PermissionDecisionKind::AskedAndDenied => Some(format!("asked by {source} (denied)")),
            PermissionDecisionKind::NoConfirmationRoute => {
                Some(format!("asked by {source} (no confirmation route)"))
            }
            _ => None,
        }
    }
}

/// Kind of permission decision rendered for a tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecisionKind {
    /// Direct execution without asking.
    DirectExecution,
    /// Execution allowed without asking by a command rule.
    RuleAllowed,
    /// Execution approved after asking.
    AskedAndAllowed,
    /// Call rejected after asking.
    AskedAndDenied,
    /// Call rejected directly by permission mode policy.
    ModeRefused,
    /// Call rejected directly by a command rule.
    RuleDenied,
    /// Write call rejected because no prepared write record was found.
    Unprepared,
    /// Outside-sandbox call rejected because its parameters could not be
    /// decoded for permission handling.
    UnpreparedOutsideSandbox,
    /// Call rejected because no confirmation route was available.
    NoConfirmationRoute,
    /// Call rejected because the tool is unknown or unavailable.
    Unavailable,
}

pub(super) fn mode_refused_message(mode: PermissionMode, category: ToolPermissionClass) -> String {
    match category {
        ToolPermissionClass::BuiltinWrite => {
            format!("the current permission mode '{mode}' does not allow this write")
        }
        ToolPermissionClass::BuiltinCommand => {
            format!("the current permission mode '{mode}' does not allow this command")
        }
        ToolPermissionClass::CommandOutsideSandbox => format!(
            "the current permission mode '{mode}' does not allow running a \
             command outside the sandbox"
        ),
        _ => format!("the current permission mode '{mode}' does not allow this MCP tool call"),
    }
}

pub(super) fn rule_denied_message(rule: Option<&RuleSource>) -> String {
    match rule.map(|source| source.scope) {
        Some(RuleScope::User) => "command rejected by user permission rule".to_owned(),
        Some(RuleScope::Workspace) => "command rejected by workspace permission rule".to_owned(),
        Some(RuleScope::Session) => "command rejected by session permission rule".to_owned(),
        None => "command rejected by permission rule".to_owned(),
    }
}

/// Processor evaluating tool calls against active permission mode and requesting approvals.
pub(crate) struct PermissionProcessor {
    work: WorkRef,
    reporter: ToolReporter,
    permissions: Arc<PermissionHub>,
    command_settings: CommandSettings,
    prepared: Arc<
        std::sync::Mutex<
            std::collections::HashMap<causa::kernel::BlockId, crate::tools::PreparedWrite>,
        >,
    >,
    grants: Arc<super::OutsideSandboxGrants>,
}

impl PermissionProcessor {
    pub(crate) fn new(
        work: WorkRef,
        reporter: ToolReporter,
        permissions: Arc<PermissionHub>,
        command_settings: CommandSettings,
        prepared: Arc<
            std::sync::Mutex<
                std::collections::HashMap<causa::kernel::BlockId, crate::tools::PreparedWrite>,
            >,
        >,
        grants: Arc<super::OutsideSandboxGrants>,
    ) -> Self {
        Self {
            work,
            reporter,
            permissions,
            command_settings,
            prepared,
            grants,
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
        // A cancelled call can leave its grant until the next batch starts.
        self.grants.clear();

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

            let Some((category, evaluated)) =
                view.evaluate_call(mode, &tool_name, &call.input.arguments)
            else {
                self.reporter.record_permission_decision(
                    &call,
                    &invocation_id,
                    PermissionDecision {
                        mode,
                        revision,
                        kind: PermissionDecisionKind::Unavailable,
                        rule: None,
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

            let rule_source = evaluated.rule.clone();
            match evaluated.verdict {
                PermissionVerdict::Execute => {
                    if category == ToolPermissionClass::CommandOutsideSandbox {
                        self.grants.grant(call_block_id);
                    }
                    self.reporter.record_permission_decision(
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind: evaluated
                                .fixed_kind
                                .unwrap_or(PermissionDecisionKind::DirectExecution),
                            rule: rule_source,
                        },
                    );
                }
                PermissionVerdict::Reject => {
                    let kind = evaluated
                        .fixed_kind
                        .unwrap_or(PermissionDecisionKind::ModeRefused);
                    self.reporter.record_permission_decision(
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind,
                            rule: rule_source.clone(),
                        },
                    );
                    let msg = if kind == PermissionDecisionKind::RuleDenied {
                        rule_denied_message(rule_source.as_ref())
                    } else {
                        mode_refused_message(mode, category)
                    };
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": msg }),
                    )?;
                }
                PermissionVerdict::Ask => {
                    let (subject, is_mcp, is_command) = match category {
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
                                        rule: rule_source.clone(),
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
                                false,
                            )
                        }
                        ToolPermissionClass::BuiltinCommand
                        | ToolPermissionClass::CommandOutsideSandbox => {
                            let outside = category == ToolPermissionClass::CommandOutsideSandbox;
                            let decoded = crate::tools::parameters::Params::decode(
                                WorkspaceTool::RunCommand,
                                &call.input.arguments,
                                // The precheck already validated these values; a
                                // failure here means the call changed underneath it.
                                &crate::definition::WorkspaceToolLimits::default(),
                                &crate::definition::WriteToolLimits::default(),
                                self.command_settings,
                            );
                            let Ok(crate::tools::parameters::Params::Command(params)) = decoded
                            else {
                                self.reporter.record_permission_decision(
                                    &call,
                                    &invocation_id,
                                    PermissionDecision {
                                        mode,
                                        revision,
                                        kind: if outside {
                                            PermissionDecisionKind::UnpreparedOutsideSandbox
                                        } else {
                                            PermissionDecisionKind::Unprepared
                                        },
                                        rule: rule_source.clone(),
                                    },
                                );
                                resolve_call(
                                    batch,
                                    call_block_id,
                                    ToolResultStatus::Rejected,
                                    serde_json::json!({
                                        "error": "command parameters were not prepared by precheck"
                                    }),
                                )?;
                                continue;
                            };
                            let target = if outside {
                                CommandTarget::Outside {
                                    justification: params.justification.unwrap_or_default(),
                                }
                            } else {
                                CommandTarget::Sandboxed {
                                    network: self.command_settings.network,
                                }
                            };
                            let suggested_prefixes =
                                suggest_command_prefixes(&params.command).unwrap_or_default();
                            let can_save_workspace_rule = !suggested_prefixes.is_empty()
                                && self.permissions.can_save_workspace_rule();
                            (
                                ApprovalSubject::Command {
                                    command: params.command,
                                    workdir: params.workdir.unwrap_or_else(|| ".".into()),
                                    timeout_secs: params
                                        .timeout_secs
                                        .unwrap_or(self.command_settings.default_timeout_secs),
                                    target,
                                    suggested_prefixes,
                                    can_save_workspace_rule,
                                },
                                false,
                                true,
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
                                false,
                            )
                        }
                    };

                    let command_rule_info = match &subject {
                        ApprovalSubject::Command {
                            target,
                            suggested_prefixes,
                            can_save_workspace_rule,
                            ..
                        } => Some((
                            suggested_prefixes.clone(),
                            *can_save_workspace_rule,
                            matches!(target, CommandTarget::Outside { .. }),
                        )),
                        _ => None,
                    };

                    let Some(sender) = self
                        .permissions
                        .approvals()
                        .get_sender()
                        .filter(|sender| !sender.is_closed())
                    else {
                        self.reporter.record_permission_decision(
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                                rule: rule_source.clone(),
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
                                rule: rule_source.clone(),
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
                            if category == ToolPermissionClass::CommandOutsideSandbox {
                                self.grants.grant(call_block_id);
                            }
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                    rule: rule_source.clone(),
                                },
                            );
                        }
                        Ok(ApprovalDecision::AllowForSession) => {
                            if let Some((prefixes, _, outside)) = command_rule_info
                                && !prefixes.is_empty()
                            {
                                self.permissions.add_session_rules(&prefixes, outside);
                            }
                            if category == ToolPermissionClass::CommandOutsideSandbox {
                                self.grants.grant(call_block_id);
                            }
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                    rule: rule_source.clone(),
                                },
                            );
                        }
                        Ok(ApprovalDecision::AllowInWorkspace) => {
                            if let Some((prefixes, can_save, outside)) = command_rule_info
                                && can_save
                                && !prefixes.is_empty()
                            {
                                let permissions = self.permissions.clone();
                                // Ignore errors (e.g., TOCTOU external modification) and fall
                                // back to allowing this single call without trusting/overwriting.
                                let _ = tokio::task::spawn_blocking(move || {
                                    permissions.append_workspace_rules(&prefixes, outside)
                                })
                                .await;
                            }
                            if category == ToolPermissionClass::CommandOutsideSandbox {
                                self.grants.grant(call_block_id);
                            }
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                    rule: rule_source.clone(),
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
                                    rule: rule_source.clone(),
                                },
                            );
                            let msg = if category == ToolPermissionClass::CommandOutsideSandbox {
                                match reason {
                                    Some(r) => format!(
                                        "user declined this command outside the sandbox: {r}"
                                    ),
                                    None => {
                                        "user declined this command outside the sandbox".to_string()
                                    }
                                }
                            } else if is_command {
                                match reason {
                                    Some(r) => format!("user declined this command: {r}"),
                                    None => "user declined this command".to_string(),
                                }
                            } else if is_mcp {
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
                                    rule: rule_source,
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
