//! Unified pre-dispatch policy orchestrator (`ToolPolicyProcessor`).
//!
//! Combines unbound-call reconciliation, zero-I/O fast mode/rule gating,
//! per-call tool preparation, live mode/rule re-evaluation, approval routing,
//! and conditional staging commit in a single `before` processor.

use super::approval::{
    ApprovalDecision, ApprovalReply, ApprovalRequest, ApprovalSubject, CommandTarget,
};
use super::rules::{
    CommandRuleSet, EvaluatedPermission, apply_command_rule, suggest_command_prefixes,
};
use super::view::PermissionHub;
use crate::policy::{
    PermissionDecision, PermissionDecisionKind, PermissionMode, PermissionVerdict, RuleScope,
    RuleSource, ToolPermissionClass, decide,
};
use crate::session::{ToolReporter, WorkRef};
use crate::tools::{
    ApprovalPreview, BatchPrepareState, CommandPreviewTarget, ExecutionGrant, ManagedTool,
    PreparedCall, ToolCallIntent,
};
use async_trait::async_trait;
use causa::kernel::{
    BatchError, BlockId, CallControl, InvocationId, ProcessorContext, ProcessorError, ToolBatch,
    ToolBatchProcessor, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use std::sync::Arc;
use tokio::sync::oneshot;

const NO_CONFIRMATION_ROUTE_MESSAGE: &str = "no confirmation route available";
const UNPREPARED_PREVIEW_MESSAGE: &str = "tool call could not produce an approval preview";

fn unavailable_message(tool_name: &str) -> String {
    format!("tool '{tool_name}' is not available")
}

fn mode_refused_message(mode: PermissionMode, class: ToolPermissionClass) -> String {
    match class {
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

fn rule_denied_message(rule: Option<&RuleSource>) -> String {
    match rule.map(|source| source.scope) {
        Some(RuleScope::User) => "command rejected by user permission rule".to_owned(),
        Some(RuleScope::Workspace) => "command rejected by workspace permission rule".to_owned(),
        Some(RuleScope::Session) => "command rejected by session permission rule".to_owned(),
        None => "command rejected by permission rule".to_owned(),
    }
}

fn user_declined_message(class: ToolPermissionClass, reason: Option<&str>) -> String {
    let prefix = match class {
        ToolPermissionClass::CommandOutsideSandbox => {
            "user declined this command outside the sandbox"
        }
        ToolPermissionClass::BuiltinCommand => "user declined this command",
        ToolPermissionClass::BuiltinWrite => "user declined this write",
        _ => "user declined this tool call",
    };
    match reason {
        Some(r) => format!("{prefix}: {r}"),
        None => prefix.to_owned(),
    }
}

fn evaluate_intent(
    mode: PermissionMode,
    rules: &CommandRuleSet,
    intent: &ToolCallIntent,
) -> EvaluatedPermission {
    let base = decide(mode, intent.class);
    let matched = match intent.class {
        ToolPermissionClass::BuiltinCommand | ToolPermissionClass::CommandOutsideSandbox => {
            let outside_sandbox = intent.class == ToolPermissionClass::CommandOutsideSandbox;
            intent
                .command_subject
                .as_deref()
                .and_then(|cmd| rules.evaluate(cmd, outside_sandbox))
        }
        _ => None,
    };
    apply_command_rule(base, matched)
}

type CommandRuleInfo = (Vec<Vec<String>>, bool, bool);

fn build_approval_subject(
    permissions: &PermissionHub,
    preview: ApprovalPreview,
) -> (ApprovalSubject, Option<CommandRuleInfo>) {
    match preview {
        ApprovalPreview::FileWrite { path, action, diff } => {
            (ApprovalSubject::FileWrite { path, action, diff }, None)
        }
        ApprovalPreview::Command {
            command,
            workdir,
            timeout_secs,
            target,
        } => {
            let target = match target {
                CommandPreviewTarget::Sandboxed { network } => CommandTarget::Sandboxed { network },
                CommandPreviewTarget::Outside { justification } => {
                    CommandTarget::Outside { justification }
                }
            };
            let suggested_prefixes = suggest_command_prefixes(&command).unwrap_or_default();
            let can_save_workspace_rule =
                !suggested_prefixes.is_empty() && permissions.can_save_workspace_rule();
            let is_outside = matches!(target, CommandTarget::Outside { .. });
            let rule_info = (
                suggested_prefixes.clone(),
                can_save_workspace_rule,
                is_outside,
            );
            (
                ApprovalSubject::Command {
                    command,
                    workdir,
                    timeout_secs,
                    target,
                    suggested_prefixes,
                    can_save_workspace_rule,
                },
                Some(rule_info),
            )
        }
        ApprovalPreview::McpCall {
            server,
            tool,
            arguments,
        } => (
            ApprovalSubject::McpCall {
                server,
                tool,
                arguments,
            },
            None,
        ),
    }
}

/// Single `before` orchestrator handling binding checks, fast gating, preflight
/// preparation, live mode/rule re-evaluation, approval flows, and attachment commits.
pub(crate) struct ToolPolicyProcessor {
    work: WorkRef,
    reporter: ToolReporter,
    permissions: Arc<PermissionHub>,
    #[cfg(test)]
    after_prepare_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ToolPolicyProcessor {
    pub(crate) fn new(
        work: WorkRef,
        reporter: ToolReporter,
        permissions: Arc<PermissionHub>,
    ) -> Self {
        Self {
            work,
            reporter,
            permissions,
            #[cfg(test)]
            after_prepare_hook: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_after_prepare_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.after_prepare_hook = Some(hook);
        self
    }

    fn resolve_rejection(
        &self,
        batch: &mut ToolBatch,
        call: &causa::kernel::ToolCallContext,
        invocation_id: &InvocationId,
        decision: PermissionDecision,
        message: String,
    ) -> Result<(), ProcessorError> {
        self.reporter
            .record_permission_decision(call, invocation_id, decision);
        resolve_call(
            batch,
            call.call_block_id,
            ToolResultStatus::Rejected,
            serde_json::json!({ "error": message }),
        )
    }
}

#[async_trait]
impl ToolBatchProcessor for ToolPolicyProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        let invocation_id = InvocationId {
            turn_id: ctx.turn_id.clone(),
            round_id: ctx.round_id,
        };

        // Step 0: Reconcile calls already rejected by Causa's `reject_unbound`
        // so every unavailable tool (including unselected built-ins) gets the
        // exact same message and `Unavailable` permission decision.
        let (mode, revision) = self.permissions.get();
        for entry in batch.results_mut() {
            let call = entry.call().clone();
            let tool_name = &call.input.tool_name;
            if ctx.tool(tool_name).is_none() {
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
                if let Some(out) = entry.output_mut() {
                    *out = ToolOutput::new(serde_json::json!({
                        "error": unavailable_message(tool_name)
                    }));
                }
            }
        }

        // Step 1: Batch-wide zero-I/O Fast Gate in declaration order.
        let mut step2_calls: Vec<(BlockId, Arc<dyn ManagedTool>, ToolCallIntent)> = Vec::new();
        for &call_block_id in ctx.declaration_order {
            if ctx.control.check().is_err() {
                return Ok(());
            }

            let Some(entry) = batch
                .calls()
                .iter()
                .find(|entry| entry.call().call_block_id == call_block_id)
            else {
                continue;
            };
            let call = entry.call().clone();
            let tool_name = call.input.tool_name.clone();

            let (mode, revision) = self.permissions.get();
            let managed = ctx
                .tool_metadata::<Arc<dyn ManagedTool>>(&tool_name)
                .cloned();

            let Some(managed) = managed else {
                self.resolve_rejection(
                    batch,
                    &call,
                    &invocation_id,
                    PermissionDecision {
                        mode,
                        revision,
                        kind: PermissionDecisionKind::Unavailable,
                        rule: None,
                    },
                    unavailable_message(&tool_name),
                )?;
                continue;
            };

            let intent = managed.classify(&call.input.arguments);
            let rules = self.permissions.command_rules();
            let evaluated = evaluate_intent(mode, &rules, &intent);

            match evaluated.verdict {
                PermissionVerdict::Reject => {
                    let kind = evaluated
                        .fixed_kind
                        .unwrap_or(PermissionDecisionKind::ModeRefused);
                    let msg = if kind == PermissionDecisionKind::RuleDenied {
                        rule_denied_message(evaluated.rule.as_ref())
                    } else {
                        mode_refused_message(mode, intent.class)
                    };
                    self.resolve_rejection(
                        batch,
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind,
                            rule: evaluated.rule,
                        },
                        msg,
                    )?;
                }
                PermissionVerdict::Ask => {
                    let has_route = self
                        .permissions
                        .approvals()
                        .get_sender()
                        .is_some_and(|sender| !sender.is_closed());
                    if !has_route {
                        self.resolve_rejection(
                            batch,
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                                rule: evaluated.rule,
                            },
                            NO_CONFIRMATION_ROUTE_MESSAGE.to_owned(),
                        )?;
                    } else {
                        step2_calls.push((call_block_id, managed, intent));
                    }
                }
                PermissionVerdict::Execute => {
                    step2_calls.push((call_block_id, managed, intent));
                }
            }
        }

        // Step 2: Serial per-call prepare -> re-decide -> approve -> attach_prepared.
        let mut batch_state = BatchPrepareState::new();

        for (call_block_id, managed, intent) in step2_calls {
            if ctx.control.check().is_err() {
                clear_pending_attachments(batch);
                return Ok(());
            }

            let Some(entry) = batch
                .calls()
                .iter()
                .find(|entry| entry.call().call_block_id == call_block_id)
            else {
                continue;
            };
            let call = entry.call().clone();
            let tool_name = call.input.tool_name.clone();

            let prepared = match managed.prepare(&call, &mut batch_state, ctx.control).await {
                Ok(prepared) => prepared,
                Err(rejection) => {
                    if ctx.control.check().is_err() {
                        clear_pending_attachments(batch);
                        return Ok(());
                    }
                    resolve_call_payload(
                        batch,
                        call_block_id,
                        ToolResultPayload::new(call_block_id, rejection.status, rejection.output),
                    )?;
                    continue;
                }
            };

            #[cfg(test)]
            if let Some(hook) = &self.after_prepare_hook {
                hook();
            }

            let (mode, revision) = self.permissions.get();
            let rules = self.permissions.command_rules();
            let evaluated = evaluate_intent(mode, &rules, &intent);
            let rule_source = evaluated.rule.clone();
            let outside_grant = ExecutionGrant {
                allow_outside_sandbox: intent.class == ToolPermissionClass::CommandOutsideSandbox,
            };

            match evaluated.verdict {
                PermissionVerdict::Execute => {
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
                    attach_to_pending_call(
                        batch,
                        call_block_id,
                        managed.as_ref(),
                        prepared,
                        outside_grant,
                    );
                }
                PermissionVerdict::Reject => {
                    drop(prepared);
                    let kind = evaluated
                        .fixed_kind
                        .unwrap_or(PermissionDecisionKind::ModeRefused);
                    let msg = if kind == PermissionDecisionKind::RuleDenied {
                        rule_denied_message(rule_source.as_ref())
                    } else {
                        mode_refused_message(mode, intent.class)
                    };
                    self.resolve_rejection(
                        batch,
                        &call,
                        &invocation_id,
                        PermissionDecision {
                            mode,
                            revision,
                            kind,
                            rule: rule_source,
                        },
                        msg,
                    )?;
                }
                PermissionVerdict::Ask => {
                    let Some(preview) = managed.approval_preview(&call, &prepared) else {
                        drop(prepared);
                        let kind = if intent.class == ToolPermissionClass::CommandOutsideSandbox {
                            PermissionDecisionKind::UnpreparedOutsideSandbox
                        } else {
                            PermissionDecisionKind::Unprepared
                        };
                        self.resolve_rejection(
                            batch,
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind,
                                rule: rule_source,
                            },
                            UNPREPARED_PREVIEW_MESSAGE.to_owned(),
                        )?;
                        continue;
                    };

                    let (subject, command_rule_info) =
                        build_approval_subject(&self.permissions, preview);

                    let Some(sender) = self
                        .permissions
                        .approvals()
                        .get_sender()
                        .filter(|sender| !sender.is_closed())
                    else {
                        drop(prepared);
                        self.resolve_rejection(
                            batch,
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                                rule: rule_source,
                            },
                            NO_CONFIRMATION_ROUTE_MESSAGE.to_owned(),
                        )?;
                        continue;
                    };

                    let (reply_tx, reply_rx) = oneshot::channel();
                    let request = ApprovalRequest {
                        work: self.work.clone(),
                        call_block_id,
                        tool_name,
                        mode,
                        revision,
                        subject,
                        reply: ApprovalReply::new(reply_tx),
                    };

                    let send_res = tokio::select! {
                        biased;
                        _ = wait_for_control_stop(ctx.control) => {
                            drop(prepared);
                            clear_pending_attachments(batch);
                            return Ok(());
                        }
                        res = sender.send(request) => res,
                    };

                    if send_res.is_err() {
                        drop(prepared);
                        self.resolve_rejection(
                            batch,
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                                rule: rule_source,
                            },
                            NO_CONFIRMATION_ROUTE_MESSAGE.to_owned(),
                        )?;
                        continue;
                    }

                    let reply_res = tokio::select! {
                        biased;
                        _ = wait_for_control_stop(ctx.control) => {
                            drop(prepared);
                            clear_pending_attachments(batch);
                            return Ok(());
                        }
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
                                    rule: rule_source,
                                },
                            );
                            attach_to_pending_call(
                                batch,
                                call_block_id,
                                managed.as_ref(),
                                prepared,
                                outside_grant,
                            );
                        }
                        Ok(ApprovalDecision::AllowForSession) => {
                            if let Some((prefixes, _, outside)) = command_rule_info
                                && !prefixes.is_empty()
                            {
                                self.permissions.add_session_rules(&prefixes, outside);
                            }
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                    rule: rule_source,
                                },
                            );
                            attach_to_pending_call(
                                batch,
                                call_block_id,
                                managed.as_ref(),
                                prepared,
                                outside_grant,
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
                            self.reporter.record_permission_decision(
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndAllowed,
                                    rule: rule_source,
                                },
                            );
                            attach_to_pending_call(
                                batch,
                                call_block_id,
                                managed.as_ref(),
                                prepared,
                                outside_grant,
                            );
                        }
                        Ok(ApprovalDecision::Deny { reason }) => {
                            drop(prepared);
                            self.resolve_rejection(
                                batch,
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::AskedAndDenied,
                                    rule: rule_source,
                                },
                                user_declined_message(intent.class, reason.as_deref()),
                            )?;
                        }
                        Err(_) => {
                            drop(prepared);
                            self.resolve_rejection(
                                batch,
                                &call,
                                &invocation_id,
                                PermissionDecision {
                                    mode,
                                    revision,
                                    kind: PermissionDecisionKind::NoConfirmationRoute,
                                    rule: rule_source,
                                },
                                NO_CONFIRMATION_ROUTE_MESSAGE.to_owned(),
                            )?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

fn attach_to_pending_call(
    batch: &mut ToolBatch,
    call_block_id: BlockId,
    managed: &dyn ManagedTool,
    prepared: PreparedCall,
    grant: ExecutionGrant,
) {
    if let Some(entry) = batch
        .calls_mut()
        .iter_mut()
        .find(|entry| entry.call().call_block_id == call_block_id)
        && let Some(attachments) = entry.attachments_mut()
    {
        managed.attach_prepared(attachments, prepared, grant);
    }
}

fn clear_pending_attachments(batch: &mut ToolBatch) {
    for entry in batch.calls_mut() {
        if let Some(attachments) = entry.attachments_mut() {
            attachments.clear();
        }
    }
}

fn resolve_call(
    batch: &mut ToolBatch,
    call_block_id: BlockId,
    status: ToolResultStatus,
    content: serde_json::Value,
) -> Result<(), ProcessorError> {
    let payload = ToolResultPayload::new(call_block_id, status, ToolOutput::new(content));
    resolve_call_payload(batch, call_block_id, payload)
}

fn resolve_call_payload(
    batch: &mut ToolBatch,
    call_block_id: BlockId,
    mut payload: ToolResultPayload,
) -> Result<(), ProcessorError> {
    let pending = batch
        .calls()
        .iter()
        .position(|entry| entry.call().call_block_id == call_block_id);
    let Some(pending) = pending else {
        return Ok(());
    };
    let index = batch.completed_len() + pending;
    loop {
        match batch.resolve_at(index, BlockId::new(uuid::Uuid::now_v7()), payload) {
            Ok(()) => return Ok(()),
            Err(rejected) => match rejected.error {
                BatchError::DuplicateResultBlockId(_) => payload = rejected.result,
                error => return Err(ProcessorError::Failed(error.to_string())),
            },
        }
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
