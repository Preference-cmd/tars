//! Mode gate processor rejecting calls forbidden by permission mode or lacking confirmation routes.

use super::approval::ApprovalsHub;
use super::precheck::resolve_call;
use super::processor::{
    PermissionDecision, PermissionDecisionKind, PermissionVerdict, ToolPermissionClass, decide,
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

            let Some(category) = view.classify_call(&tool_name, &call.input.arguments) else {
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
                    let msg = match category {
                        ToolPermissionClass::BuiltinWrite => {
                            format!(
                                "the current permission mode '{mode}' does not allow this write"
                            )
                        }
                        ToolPermissionClass::BuiltinCommand => format!(
                            "the current permission mode '{mode}' does not allow this command"
                        ),
                        ToolPermissionClass::CommandOutsideSandbox => format!(
                            "the current permission mode '{mode}' does not allow running a \
                             command outside the sandbox"
                        ),
                        _ => format!(
                            "the current permission mode '{mode}' does not allow this MCP tool call"
                        ),
                    };
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": msg }),
                    )?;
                }
                PermissionVerdict::Ask => {
                    let has_route = self.hub.get_sender().map_or(false, |s| !s.is_closed());
                    if !has_route {
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
                PermissionVerdict::Execute => {}
            }
        }

        Ok(())
    }
}
