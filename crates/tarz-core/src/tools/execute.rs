#[cfg(test)]
use super::ToolMetrics;
use super::operations::{OperationControl, run};
use super::parameters::Params;
use super::resources::{Counters, OperationGuard, ToolResources};
use crate::definition::{WorkspaceTool, WorkspaceToolLimits};
use crate::session::ToolReporter;
use crate::workspace::Workspace;
use causa::kernel::{
    CallControl, Tool, ToolCallContext, ToolDefinition, ToolOutput, ToolResultPayload,
    ToolResultStatus,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Fixed workspace resources reused by independently bound work wrappers.
pub struct WorkspaceTools {
    workspace: Arc<Workspace>,
    limits: WorkspaceToolLimits,
    pub(super) local: Arc<Semaphore>,
    host: Arc<ToolResources>,
    counters: Arc<Counters>,
    #[cfg(test)]
    pub(super) gate: std::sync::Mutex<Option<Arc<TestGate>>>,
}

impl WorkspaceTools {
    /// Bind a selected root to frozen application limits and shared host slots.
    pub fn new(
        workspace: Arc<Workspace>,
        limits: WorkspaceToolLimits,
        host: Arc<ToolResources>,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace,
            local: Arc::new(Semaphore::new(limits.max_concurrent_operations)),
            limits,
            host,
            counters: Arc::new(Counters::default()),
            #[cfg(test)]
            gate: std::sync::Mutex::new(None),
        })
    }

    /// Produce new per-work tool wrappers. Reporters never carry over into the next work.
    pub fn bind(
        self: &Arc<Self>,
        selected: &[WorkspaceTool],
        reporter: ToolReporter,
    ) -> Vec<Arc<dyn Tool>> {
        selected
            .iter()
            .map(|kind| {
                Arc::new(WorkspaceToolImpl {
                    shared: self.clone(),
                    kind: *kind,
                    reporter: reporter.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    /// Observe actual filesystem resource counters without reading tool materials.
    #[cfg(test)]
    pub fn metrics(&self) -> ToolMetrics {
        self.counters.snapshot()
    }
}

struct WorkspaceToolImpl {
    shared: Arc<WorkspaceTools>,
    kind: WorkspaceTool,
    reporter: ToolReporter,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(crate) fn result(
    call: &ToolCallContext,
    status: ToolResultStatus,
    content: serde_json::Value,
) -> ToolResultPayload {
    ToolResultPayload::new(call.call_block_id, status, ToolOutput::new(content))
}

fn stopped(call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
    let status = if control.is_cancelled() {
        ToolResultStatus::Cancelled
    } else {
        ToolResultStatus::TimedOut
    };
    result(
        call,
        status,
        serde_json::json!({"error":"operation stopped before obtaining a result"}),
    )
}

#[async_trait::async_trait]
impl Tool for WorkspaceToolImpl {
    fn definition(&self) -> ToolDefinition {
        use serde_json::json;
        let (name, description, fields) = match self.kind {
            WorkspaceTool::ListDirectory => (
                "list_directory",
                "List a single workspace-relative directory without following links.",
                json!({"path":{"type":"string","maxLength":4096},"limit":{"type":"integer","minimum":1,"maximum":self.shared.limits.max_directory_entries}}),
            ),
            WorkspaceTool::ReadFile => (
                "read_file",
                "Read UTF-8 lines from a regular workspace-relative file; line numbers start at one.",
                json!({"path":{"type":"string","maxLength":4096},"start_line":{"type":"integer","minimum":1},"max_lines":{"type":"integer","minimum":1,"maximum":2000}}),
            ),
            WorkspaceTool::SearchText => (
                "search_text",
                "Search single-line literal text (at most 1024 UTF-8 bytes) in bounded workspace files; partial scans are explicitly marked.",
                json!({"path":{"type":"string","maxLength":4096},"query":{"type":"string","minLength":1,"maxLength":1024},"max_matches":{"type":"integer","minimum":1,"maximum":self.shared.limits.max_search_matches}}),
            ),
        };
        let required = if matches!(self.kind, WorkspaceTool::SearchText) {
            json!(["path", "query"])
        } else {
            json!(["path"])
        };
        ToolDefinition::new(
            name,
            description,
            json!({"type":"object","properties":fields,"required":required,"additionalProperties":false}),
        )
    }

    async fn execute(&self, call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
        let params = if call.input.tool_name == self.definition().name {
            Params::decode(self.kind, &call.input.arguments, &self.shared.limits)
        } else {
            Err("tool name does not match bound capability")
        };
        let params = match params {
            Ok(params) => params,
            Err(error) => {
                let output = result(
                    call,
                    ToolResultStatus::Rejected,
                    serde_json::json!({"error":error}),
                );
                self.reporter.returned(call, output.status.clone());
                return output;
            }
        };
        let control =
            control.with_timeout(Duration::from_secs(self.shared.limits.call_timeout_secs));
        self.reporter.waiting(call);
        #[cfg(test)]
        self.shared
            .host
            .counters
            .test_wrappers_entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let acquire = async {
            let local = self.shared.local.clone().acquire_owned().await.ok()?;
            let host = self.shared.host.slots.clone().acquire_owned().await.ok()?;
            Some((host, local))
        };
        let deadline = async {
            if let Some(deadline) = control.deadline() {
                tokio::time::sleep_until(deadline.into()).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let permits = tokio::select! {
            biased;
            _ = control.cancellation_token().cancelled() => None,
            _ = deadline => None,
            permits = acquire => permits,
        };
        let Some((host, local)) = permits else {
            return stopped(call, &control);
        };
        if control.check().is_err() {
            return stopped(call, &control);
        }
        let dropstop = CancellationToken::new();
        let _cancel = CancelOnDrop(dropstop.clone());
        let worker_control = OperationControl {
            call: control.clone(),
            dropstop,
            counters: self.shared.counters.clone(),
            host_counters: self.shared.host.counters.clone(),
        };
        let shared = self.shared.clone();
        let call_owned = call.clone();
        let reporter = self.reporter.clone();
        let worker = self.reporter.spawn_blocking(move || {
            if worker_control.check().is_err() { return stopped(&call_owned, &worker_control.call); }
            let guard = OperationGuard { counters: shared.counters.clone(), host_counters: shared.host.counters.clone(), _local: local, _host: host };
            guard.start();
            reporter.running(&call_owned);
            #[cfg(test)]
            if let Some(gate) = shared.gate.lock().expect("gate lock").clone() {
                gate.pause(false);
            }
            let output = run(&shared.workspace, &params, &shared.limits, &worker_control);
            let mut output = match output {
                Ok(content) => result(&call_owned, ToolResultStatus::Succeeded, content),
                Err(error) => result(&call_owned, error.status(), serde_json::json!({"error":error.to_string()})),
            };
            // Bound the entire serialized result, including identity and JSON escaping.
            if serde_json::to_vec(&output).map_or(true, |bytes| bytes.len() > shared.limits.max_output_bytes) {
                output.output.content = serde_json::json!({"truncated":true,"complete":false,"limit_reason":"output_bytes"});
            }
            #[cfg(test)]
            if let Some(gate) = shared.gate.lock().expect("gate lock").clone() {
                gate.pause(true);
            }
            reporter.returned(&call_owned, output.status.clone());
            output
        });
        match worker.await {
            Ok(output) => output,
            Err(_) => result(
                call,
                ToolResultStatus::Failed,
                serde_json::json!({"error":"filesystem worker failed"}),
            ),
        }
    }
}

#[cfg(test)]
pub(super) struct TestGate {
    pub before_return: bool,
    pub reached: tokio::sync::Notify,
    pub released: std::sync::Mutex<bool>,
    pub changed: std::sync::Condvar,
}
#[cfg(test)]
impl TestGate {
    pub fn new(before_return: bool) -> Arc<Self> {
        Arc::new(Self {
            before_return,
            reached: tokio::sync::Notify::new(),
            released: std::sync::Mutex::new(false),
            changed: std::sync::Condvar::new(),
        })
    }
    fn pause(&self, before_return: bool) {
        if self.before_return != before_return {
            return;
        }
        self.reached.notify_one();
        let mut released = self.released.lock().expect("gate release lock");
        while !*released {
            released = self.changed.wait(released).expect("gate wait");
        }
    }
    pub fn release(&self) {
        *self.released.lock().expect("gate release lock") = true;
        self.changed.notify_all();
    }
}
