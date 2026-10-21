//! Precheck processor performing parameter validation and write staging before permissions.

use crate::definition::WorkspaceTool;
use crate::session::ToolReporter;
use crate::tools::operations::{OperationControl, precheck_edit, precheck_write};
use crate::tools::parameters::Params;
use crate::tools::resources::OperationGuard;
use crate::tools::{WorkspaceTools, result};
use crate::workspace::Workspace;
use async_trait::async_trait;
use causa::kernel::{
    BatchError, BlockId, ProcessorContext, ProcessorError, ToolBatch, ToolBatchProcessor,
    ToolOutput, ToolResultPayload, ToolResultStatus,
};
use std::collections::HashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Processor performing parameter decoding, conflict detection, and write staging.
pub(crate) struct PrecheckProcessor {
    tools: Arc<WorkspaceTools>,
    reporter: ToolReporter,
    prepared:
        Arc<std::sync::Mutex<std::collections::HashMap<BlockId, crate::tools::PreparedWrite>>>,
}

impl PrecheckProcessor {
    pub(crate) fn new(
        tools: Arc<WorkspaceTools>,
        reporter: ToolReporter,
        prepared: Arc<
            std::sync::Mutex<std::collections::HashMap<BlockId, crate::tools::PreparedWrite>>,
        >,
    ) -> Self {
        Self {
            tools,
            reporter,
            prepared,
        }
    }
}

#[async_trait]
impl ToolBatchProcessor for PrecheckProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        let mut seen_write_paths = HashSet::new();

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

            let kind = match call.input.tool_name.as_str() {
                "write_file" => WorkspaceTool::WriteFile,
                "edit_file" => WorkspaceTool::EditFile,
                _ => continue,
            };

            let params = Params::decode(
                kind,
                &call.input.arguments,
                self.tools.limits(),
                self.tools.write_limits(),
            );
            let params = match params {
                Ok(params) => params,
                Err(error) => {
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": error.to_string() }),
                    )?;
                    continue;
                }
            };

            let (raw_path, tool_label) = match &params {
                Params::Write(p) => (&p.path, "write_file"),
                Params::Edit(p) => (&p.path, "edit_file"),
                _ => continue,
            };

            let components = match Workspace::components(raw_path) {
                Ok(components) if !components.is_empty() => components,
                Ok(_) => {
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": format!("{tool_label} requires a file path") }),
                    )?;
                    continue;
                }
                Err(err) => {
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Rejected,
                        serde_json::json!({ "error": err.to_string() }),
                    )?;
                    continue;
                }
            };

            let normalized = components.join("/");
            if !seen_write_paths.insert(normalized) {
                resolve_call(
                    batch,
                    call_block_id,
                    ToolResultStatus::Rejected,
                    serde_json::json!({
                        "error": "the same file can only be modified once in a single batch"
                    }),
                )?;
                continue;
            }

            // Perform inspection under supervised file operations permits.
            let acquire = async {
                let local = self.tools.local().acquire_owned().await.ok()?;
                let host = self.tools.host().slots.clone().acquire_owned().await.ok()?;
                Some((host, local))
            };
            let deadline = async {
                if let Some(deadline) = ctx.control.deadline() {
                    tokio::time::sleep_until(deadline.into()).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let permits = tokio::select! {
                biased;
                _ = ctx.control.cancellation_token().cancelled() => None,
                _ = deadline => None,
                permits = acquire => permits,
            };
            let Some((host, local)) = permits else {
                return Ok(());
            };
            if ctx.control.check().is_err() {
                return Ok(());
            }

            let dropstop = CancellationToken::new();
            let _cancel = CancelOnDrop(dropstop.clone());
            let worker_control = OperationControl {
                call: ctx.control.clone(),
                dropstop: dropstop.clone(),
                counters: self.tools.counters(),
                host_counters: self.tools.host().counters.clone(),
            };
            let tools = self.tools.clone();
            let params = params.clone();
            let call_owned = call.clone();
            let worker = self.reporter.spawn_blocking(move || {
                if worker_control.check().is_err() {
                    return Err(crate::tools::operations::OperationError::Control(
                        causa::kernel::ControlError::Cancelled,
                    ));
                }
                let guard = OperationGuard {
                    counters: tools.counters(),
                    host_counters: tools.host().counters.clone(),
                    _local: local,
                    _host: host,
                };
                guard.start();
                match &params {
                    Params::Write(p) => precheck_write(
                        tools.workspace(),
                        p,
                        tools.limits(),
                        tools.write_limits(),
                        tools.observations(),
                        &worker_control,
                    ),
                    Params::Edit(p) => precheck_edit(
                        tools.workspace(),
                        p,
                        tools.limits(),
                        tools.write_limits(),
                        tools.observations(),
                        &worker_control,
                    ),
                    _ => unreachable!(),
                }
            });

            match worker.await {
                Ok(Ok(prepared)) => {
                    self.prepared
                        .lock()
                        .expect("prepared lock")
                        .insert(call_block_id, prepared);
                }
                Ok(Err(err)) => {
                    let mut payload = result(
                        &call_owned,
                        err.status(),
                        serde_json::json!({ "error": err.to_string() }),
                    );
                    if serde_json::to_vec(&payload).map_or(true, |bytes| {
                        bytes.len() > self.tools.limits().max_output_bytes
                    }) {
                        payload.output.content = serde_json::json!({
                            "truncated": true,
                            "complete": false,
                            "limit_reason": "output_bytes"
                        });
                    }
                    resolve_call_payload(batch, call_block_id, payload)?;
                }
                Err(_) => {
                    resolve_call(
                        batch,
                        call_block_id,
                        ToolResultStatus::Failed,
                        serde_json::json!({ "error": "filesystem worker failed" }),
                    )?;
                }
            }
        }

        Ok(())
    }
}

pub(crate) fn resolve_call(
    batch: &mut ToolBatch,
    call_block_id: BlockId,
    status: ToolResultStatus,
    content: serde_json::Value,
) -> Result<(), ProcessorError> {
    let payload = ToolResultPayload::new(call_block_id, status, ToolOutput::new(content));
    resolve_call_payload(batch, call_block_id, payload)
}

pub(crate) fn resolve_call_payload(
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
