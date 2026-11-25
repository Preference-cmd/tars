use super::host::{BuiltinBindings, SubcallHost};
use super::sandbox::{SandboxOutcome, bound_output_payload, run_quickjs_blocking};
use crate::definition::{BuiltinTool, CodeExecutionSettings};
use crate::policy::ToolPermissionClass;
use crate::session::ToolReporter;
use crate::tools::lifecycle::{
    BatchPrepareState, ManagedTool, PrepareRejection, PreparedCall, ToolCallIntent,
    managed_tool_metadata,
};
use async_trait::async_trait;
use causa::kernel::{
    CallAttachments, CallControl, Tool, ToolCallContext, ToolDefinition, ToolMetadata, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Maximum accepted script length in bytes (64 KiB).
pub(crate) const MAX_CODE_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCodeParams {
    code: String,
}

impl RunCodeParams {
    fn parse(value: &Value) -> Result<Self, String> {
        let params: Self = serde_json::from_value(value.clone())
            .map_err(|e| format!("invalid run_code parameters: {e}"))?;
        if params.code.trim().is_empty() {
            return Err("code must be a nonempty string".to_owned());
        }
        if params.code.len() > MAX_CODE_BYTES {
            return Err(format!("code exceeds {MAX_CODE_BYTES} bytes"));
        }
        if params.code.contains('\0') {
            return Err("code must not contain NUL bytes".to_owned());
        }
        Ok(params)
    }
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Non-persisted call attachment carrying validated JavaScript source into `Tool::execute`.
struct AttachedCode(String);

/// Built-in read-only `run_code` tool executing ES2020 scripts inside QuickJS-NG.
#[derive(Clone)]
pub(crate) struct RunCodeTool {
    host: Arc<SubcallHost>,
    settings: CodeExecutionSettings,
    reporter: ToolReporter,
}

impl RunCodeTool {
    /// Construct a `run_code` tool bound to the session's read-only subcall host.
    pub(crate) fn new(
        host: Arc<SubcallHost>,
        settings: CodeExecutionSettings,
        reporter: ToolReporter,
    ) -> Self {
        Self {
            host,
            settings,
            reporter,
        }
    }

    fn finish(&self, call: &ToolCallContext, payload: ToolResultPayload) -> ToolResultPayload {
        self.reporter.returned(call, payload.status.clone());
        payload
    }
}

/// Build the static `ToolDefinition` for `run_code` from the session's frozen bindings.
pub(crate) fn run_code_tool_definition(bindings: BuiltinBindings) -> ToolDefinition {
    let mut helpers = Vec::new();
    if bindings.list_directory {
        helpers.push(
            "- `await list_directory({ path, limit })` -> `{ path, entries: [{ name, type }], truncated, limit_reason }`",
        );
    }
    if bindings.read_file {
        helpers.push(
            "- `await read_file({ path, start_line, max_lines })` -> `{ path, start_line, end_line, text, scanned_bytes, truncated, limit_reason }`",
        );
    }
    if bindings.search_text {
        helpers.push(
            "- `await search_text({ path, query, max_matches, regex, ignore_case, glob, context_lines, include_ignored })` -> `{ path, matches, issues, complete, truncated, limit_reason }`",
        );
    }
    if bindings.find_files {
        helpers.push(
            "- `await find_files({ path, pattern, max_results, max_depth, ignore_case, include_ignored })` -> `{ path, pattern, files, scanned_dirs, complete, truncated, limit_reason, issues }`",
        );
    }
    if bindings.recall {
        helpers.push(
            "- `await recall({ id, query, regex, case_sensitive, offset, limit, context, role, kind })` -> string",
        );
    }
    helpers.push(
        "- `await tools.list()` -> `[{ name, description, source }]`, `await tools.describe(name)` -> `{ name, description, parameters, source }`, `await tools.call(name, args)` -> result (discover and call bound built-in or read-only MCP tools)",
    );

    let description = format!(
        "Execute sandboxed JavaScript (ES2020) inside an async IIFE with read-only session helpers. \
         Each call runs in a fresh stateless sandbox (variables do not persist across calls), so combine multi-step reads and aggregations into one script when possible. \
         Your code runs inside `(async () => {{ <code> }})()` — use top-level `await`, `Promise.all(...)`, and `return <value>` (if `return` is omitted, buffered `console.log` output is returned). \
         Available async helpers (on both global scope and `tools.*`, accepting an object or positional args):\n{}\n\
         No `fs`, `process`, `require`, `import()`, or `fetch` exists.",
        helpers.join("\n")
    );

    ToolDefinition::new(
        BuiltinTool::RunCode.name(),
        description,
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_CODE_BYTES,
                    "description": "JavaScript (ES2020) body executed inside `(async () => { ... })()`."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    )
}

#[async_trait]
impl ManagedTool for RunCodeTool {
    fn classify(&self, _arguments: &Value) -> ToolCallIntent {
        ToolCallIntent {
            class: ToolPermissionClass::KnownReadOnly,
            command_subject: None,
        }
    }

    async fn prepare(
        &self,
        call: &ToolCallContext,
        _batch_state: &mut BatchPrepareState,
        _control: &CallControl,
    ) -> Result<PreparedCall, PrepareRejection> {
        if call.input.tool_name != BuiltinTool::RunCode.name() {
            return Err(PrepareRejection::rejected(
                "tool name does not match bound capability",
            ));
        }
        let params =
            RunCodeParams::parse(&call.input.arguments).map_err(PrepareRejection::rejected)?;
        Ok(PreparedCall::new(params))
    }

    fn attach_prepared(
        &self,
        attachments: &mut CallAttachments,
        prepared: PreparedCall,
        _grant: crate::tools::ExecutionGrant,
    ) {
        if let Some(params) = prepared.downcast::<RunCodeParams>() {
            attachments.insert(AttachedCode(params.code));
        }
    }
}

fn stopped_payload(call: &ToolCallContext, control: &CallControl) -> ToolResultPayload {
    let status = if control.is_cancelled() {
        ToolResultStatus::Cancelled
    } else {
        ToolResultStatus::TimedOut
    };
    ToolResultPayload::new(
        call.call_block_id,
        status,
        ToolOutput::new(json!({
            "error": "operation stopped before obtaining a result"
        })),
    )
}

fn rejected_payload(call: &ToolCallContext, message: impl Into<String>) -> ToolResultPayload {
    ToolResultPayload::new(
        call.call_block_id,
        ToolResultStatus::Rejected,
        ToolOutput::new(json!({ "error": message.into() })),
    )
}

fn failed_payload(call: &ToolCallContext, message: impl Into<String>) -> ToolResultPayload {
    ToolResultPayload::new(
        call.call_block_id,
        ToolResultStatus::Failed,
        ToolOutput::new(json!({ "error": message.into() })),
    )
}

fn outcome_to_payload(
    call: &ToolCallContext,
    outcome: SandboxOutcome,
    control: &CallControl,
    max_output_bytes: usize,
) -> ToolResultPayload {
    if outcome.ok {
        let (output, truncated) = if outcome.truncated {
            (outcome.output, true)
        } else {
            bound_output_payload(outcome.output, &outcome.stats, max_output_bytes)
        };
        return ToolResultPayload::new(
            call.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(json!({
                "output": output,
                "truncated": truncated,
                "stats": outcome.stats,
            })),
        );
    }

    let status = if outcome.cancelled || control.is_cancelled() {
        ToolResultStatus::Cancelled
    } else if outcome.timed_out {
        ToolResultStatus::TimedOut
    } else {
        ToolResultStatus::Failed
    };
    let error = outcome
        .error
        .unwrap_or_else(|| "code execution failed".to_owned());
    let mut obj = serde_json::Map::new();
    obj.insert("error".to_owned(), Value::String(error));
    if !outcome.output.is_empty() {
        obj.insert("output".to_owned(), Value::String(outcome.output));
        obj.insert("truncated".to_owned(), Value::Bool(outcome.truncated));
    }
    obj.insert(
        "stats".to_owned(),
        serde_json::to_value(&outcome.stats).unwrap_or(Value::Null),
    );
    ToolResultPayload::new(
        call.call_block_id,
        status,
        ToolOutput::new(Value::Object(obj)),
    )
}

#[async_trait]
impl Tool for RunCodeTool {
    fn definition(&self) -> ToolDefinition {
        run_code_tool_definition(self.host.bindings())
    }

    fn metadata(&self) -> ToolMetadata {
        managed_tool_metadata(Arc::new(self.clone()))
    }

    async fn execute(
        &self,
        call: &ToolCallContext,
        control: &CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        if control.check().is_err() {
            return self.finish(call, stopped_payload(call, control));
        }
        if call.input.tool_name != BuiltinTool::RunCode.name() {
            return self.finish(
                call,
                rejected_payload(call, "tool name does not match bound capability"),
            );
        }

        let code = match call.attachment::<AttachedCode>() {
            Some(attached) => attached.0.clone(),
            None => match RunCodeParams::parse(&call.input.arguments) {
                Ok(params) => params.code,
                Err(error) => return self.finish(call, rejected_payload(call, error)),
            },
        };

        let call_control = control.with_timeout(Duration::from_secs(self.settings.timeout_secs));
        self.reporter.waiting(call);

        let run_token = call_control.cancellation_token().child_token();
        let _cancel = CancelOnDrop(run_token.clone());
        let exec_control = CallControl::new(run_token.clone(), call_control.deadline());

        let host = Arc::clone(&self.host);
        let settings = self.settings;
        let reporter = self.reporter.clone();
        let call_owned = call.clone();
        let outer_handle = tokio::runtime::Handle::current();

        let task = self.reporter.spawn_blocking(move || {
            if exec_control.check().is_err() {
                return SandboxOutcome {
                    ok: false,
                    output: String::new(),
                    truncated: false,
                    error: Some("operation stopped before obtaining a result".to_owned()),
                    stats: Default::default(),
                    cancelled: exec_control.is_cancelled(),
                    timed_out: !exec_control.is_cancelled(),
                };
            }
            reporter.running(&call_owned);
            run_quickjs_blocking(host, code, settings, exec_control, outer_handle)
        });

        let deadline = async {
            match call_control.deadline() {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };

        let payload = tokio::select! {
            biased;
            _ = call_control.cancellation_token().cancelled() => {
                run_token.cancel();
                stopped_payload(call, &call_control)
            }
            _ = deadline => {
                run_token.cancel();
                stopped_payload(call, &call_control)
            }
            result = task => match result {
                Ok(outcome) => outcome_to_payload(call, outcome, &call_control, self.settings.max_output_bytes),
                Err(_) => failed_payload(call, "code execution worker panicked"),
            }
        };
        self.finish(call, payload)
    }
}
