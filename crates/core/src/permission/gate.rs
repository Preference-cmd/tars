//! Mode gate processor rejecting calls forbidden by permission mode or lacking confirmation routes.

use super::approval::ApprovalsHub;
use super::precheck::resolve_call;
use super::processor::{
    PermissionDecision, PermissionDecisionKind, PermissionVerdict, mode_refused_message,
    rule_denied_message,
};
use super::view::PermissionView;
use crate::session::ToolReporter;
use async_trait::async_trait;
use causa::kernel::{
    InvocationId, ProcessorContext, ProcessorError, ToolBatch, ToolBatchProcessor, ToolResultStatus,
};
use std::sync::Arc;

/// Frontmost processor in the execution pipeline applying pure permission mode gating.
pub(crate) struct ModeGateProcessor {
    reporter: ToolReporter,
    hub: Arc<ApprovalsHub>,
}

impl ModeGateProcessor {
    pub(crate) fn new(reporter: ToolReporter, hub: Arc<ApprovalsHub>) -> Self {
        Self { reporter, hub }
    }
}

#[async_trait]
impl ToolBatchProcessor for ModeGateProcessor {
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

        let (mode, revision) = view.get();

        for entry in batch.results_mut() {
            let call = entry.call().clone();
            let tool_name = &call.input.tool_name;
            if view
                .classify_call(tool_name, &call.input.arguments)
                .is_none()
            {
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
                    *out = causa::kernel::ToolOutput::new(serde_json::json!({
                        "error": format!("tool '{tool_name}' is not available")
                    }));
                }
            }
        }

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

            match evaluated.verdict {
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
                            rule: evaluated.rule.clone(),
                        },
                    );
                    let msg = if kind == PermissionDecisionKind::RuleDenied {
                        rule_denied_message(evaluated.rule.as_ref())
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
                    let has_route = self.hub.get_sender().is_some_and(|s| !s.is_closed());
                    if !has_route {
                        self.reporter.record_permission_decision(
                            &call,
                            &invocation_id,
                            PermissionDecision {
                                mode,
                                revision,
                                kind: PermissionDecisionKind::NoConfirmationRoute,
                                rule: evaluated.rule,
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
                PermissionVerdict::Execute => {}
            }
        }

        Ok(())
    }
}
