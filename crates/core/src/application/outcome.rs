use crate::assembly::AssemblyError;
use crate::mcp::McpStartError;
use crate::session::{ModelFailure, WorkResult, WorkerFault};
use causa::kernel::ControlError;
use causa::runtime::{TurnInterruption, TurnResult};

/// Unified, presentation-ready description of one finished work result.
///
/// Defined once in `tars-core` so entry points share a single vocabulary for
/// machine-readable kinds, user-facing summaries, actionable hints and raw
/// diagnostic details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkOutcome {
    /// Stable lower-case outcome kind for machine output and exit-code mapping.
    pub kind: &'static str,
    /// One-line user-facing summary of what happened.
    pub summary: String,
    /// Optional actionable suggestion for resolving or following up on the outcome.
    pub hint: Option<String>,
    /// Underlying model failure or unrecognized stop details, when applicable.
    pub model_failure: Option<ModelFailure>,
    /// Optional raw underlying error detail; entry points escape before display.
    pub detail: Option<String>,
}

impl WorkOutcome {
    /// Describe any terminal [`WorkResult`] using the shared core vocabulary.
    pub fn of(result: &WorkResult) -> Self {
        match result {
            WorkResult::Run(TurnResult::Completed { .. }) => Self::simple("completed", "Completed"),
            WorkResult::StoppedBeforeRun(ControlError::Cancelled)
            | WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. },
            }) => Self::simple("cancelled", "Cancelled"),
            WorkResult::StoppedBeforeRun(ControlError::TimedOut)
            | WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::DeadlineExceeded { .. },
            }) => Self {
                kind: "timed_out",
                summary: "Timed out".to_owned(),
                hint: Some("work deadline exceeded".to_owned()),
                model_failure: None,
                detail: None,
            },
            WorkResult::Run(TurnResult::Interrupted { cause }) => Self::from_interruption(cause),
            WorkResult::AssemblyFailed(error) => {
                if let Some(assembly_error) = error.downcast_ref::<AssemblyError>() {
                    Self::from_assembly_error(assembly_error)
                } else if let Some(mcp_error) = error.downcast_ref::<McpStartError>() {
                    Self::from_mcp_start_error(mcp_error)
                } else {
                    Self {
                        kind: "assembly_failed",
                        summary: "Session assembly failed".to_owned(),
                        hint: Some(
                            "check session configuration, tools and runtime resources".to_owned(),
                        ),
                        model_failure: None,
                        detail: Some(error.to_string()),
                    }
                }
            }
            WorkResult::Faulted(fault) => Self::from_fault(fault),
        }
    }

    /// Describe a typed [`AssemblyError`] without string flattening.
    pub fn from_assembly_error(error: &AssemblyError) -> Self {
        match error {
            AssemblyError::Definition(definition_error) => Self {
                kind: "assembly_failed",
                summary: "Invalid session definition".to_owned(),
                hint: Some("check the session definition or profile settings".to_owned()),
                model_failure: None,
                detail: Some(definition_error.to_string()),
            },
            AssemblyError::CredentialUnavailable => Self {
                kind: "assembly_failed",
                summary: "Credential environment variable is missing, non-Unicode or blank"
                    .to_owned(),
                hint: Some(
                    "check the configured API key environment variable (e.g. OPENAI_API_KEY or ANTHROPIC_API_KEY)"
                        .to_owned(),
                ),
                model_failure: None,
                detail: None,
            },
            AssemblyError::Workspace(workspace_error) => Self {
                kind: "assembly_failed",
                summary: "Workspace resource unavailable".to_owned(),
                hint: Some(
                    "check that the workspace root directory exists and is accessible".to_owned(),
                ),
                model_failure: None,
                detail: Some(workspace_error.to_string()),
            },
            AssemblyError::CommandUnavailable(reason) => Self {
                kind: "assembly_failed",
                summary: "Command tool unavailable".to_owned(),
                hint: Some("check platform sandbox support or disable run_command".to_owned()),
                model_failure: None,
                detail: Some(reason.clone()),
            },
            AssemblyError::SearchUnavailable(reason) => Self {
                kind: "assembly_failed",
                summary: "Search tool unavailable".to_owned(),
                hint: Some(
                    "check external ripgrep installation (tars tools status) or workspace access"
                        .to_owned(),
                ),
                model_failure: None,
                detail: Some(reason.clone()),
            },
            AssemblyError::McpStart(mcp_error) => Self::from_mcp_start_error(mcp_error),
        }
    }

    fn from_mcp_start_error(error: &McpStartError) -> Self {
        Self {
            kind: "assembly_failed",
            summary: format!(
                "Required MCP server '{}' failed to start",
                error.server_id()
            ),
            hint: Some(format!(
                "check server '{}' in mcp.json or mark it non-required",
                error.server_id()
            )),
            model_failure: None,
            detail: Some(error.reason().to_owned()),
        }
    }

    fn from_interruption(cause: &TurnInterruption) -> Self {
        match cause {
            TurnInterruption::Cancelled { .. } => Self::simple("cancelled", "Cancelled"),
            TurnInterruption::DeadlineExceeded { .. } => Self {
                kind: "timed_out",
                summary: "Timed out".to_owned(),
                hint: Some("work deadline exceeded".to_owned()),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ModelMaxTokens { .. } => Self {
                kind: "max_tokens",
                summary: "Output reached max_tokens".to_owned(),
                hint: Some("raise max_tokens and submit new work".to_owned()),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ModelContextWindowExceeded { .. } => Self {
                kind: "context_window_exceeded",
                summary: "Conversation no longer fits the model's context window".to_owned(),
                hint: Some("compact the session before submitting new work".to_owned()),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ModelRefusal { .. } => Self {
                kind: "refused",
                summary: "The model refused the request".to_owned(),
                hint: None,
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ModelStopUnrecognized { output, .. } => {
                let failure = ModelFailure::unrecognized_stop(output);
                Self {
                    kind: "stop_unrecognized",
                    summary: "Provider stopped for an unrecognized reason".to_owned(),
                    hint: Some(failure.hint.to_owned()),
                    detail: Some(failure.detail.clone()),
                    model_failure: Some(failure),
                }
            }
            TurnInterruption::MaxModelRounds { limit, .. } => Self {
                kind: "max_model_rounds",
                summary: "Model round limit reached".to_owned(),
                hint: Some(format!(
                    "limit of {limit} model rounds reached; submit new work to continue"
                )),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::MaxToolCalls { limit, .. } => Self {
                kind: "max_tool_calls",
                summary: "Tool call limit reached".to_owned(),
                hint: Some(format!(
                    "limit of {limit} tool calls exceeded; submit new work to continue"
                )),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ModelCallFailed { error, .. } => {
                let failure = ModelFailure::of(error);
                let summary = model_failure_summary(failure.kind).to_owned();
                Self {
                    kind: "model_failed",
                    summary,
                    hint: Some(failure.hint.to_owned()),
                    detail: Some(failure.detail.clone()),
                    model_failure: Some(failure),
                }
            }
            TurnInterruption::PrepareFailed { error, .. } => Self {
                kind: "prepare_failed",
                summary: "Context preparation failed".to_owned(),
                hint: Some("check context materials and compaction settings".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ToolCatalogFailed { error, .. } => Self {
                kind: "tool_catalog_failed",
                summary: "Tool binding failed".to_owned(),
                hint: Some("check enabled workspace tools and MCP tool definitions".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ToolProcessingFailed { error, .. } => Self {
                kind: "tool_processing_failed",
                summary: "Tool processing failed".to_owned(),
                hint: Some("inspect pending tool calls or repair interrupted materials".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::UnknownToolOutcome { .. } => Self {
                kind: "unknown_tool_outcome",
                summary: "A tool call ended with an unknown outcome".to_owned(),
                hint: Some("verify any external side effects before retrying".to_owned()),
                model_failure: None,
                detail: None,
            },
            TurnInterruption::ToolChangeCommitFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Tool change could not be committed".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ModelConversionFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Model output could not be converted to context blocks".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ModelBatchFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Model tool calls could not form a batch".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ModelCommitFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Model output could not be committed".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ToolResultValidationFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Tool results failed validation".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            TurnInterruption::ToolCommitFailed { error, .. } => Self {
                kind: "context_commit_failed",
                summary: "Tool results could not be committed".to_owned(),
                hint: Some("inspect or repair session materials before continuing".to_owned()),
                model_failure: None,
                detail: Some(error.to_string()),
            },
            // Future Causa TurnInterruption variants fall back to a generic failure.
            _ => Self {
                kind: "failed",
                summary: "Work interrupted for an unrecognized reason".to_owned(),
                hint: None,
                model_failure: None,
                detail: Some(cause.to_string()),
            },
        }
    }

    fn from_fault(fault: &WorkerFault) -> Self {
        match fault {
            WorkerFault::Panicked { message } => Self {
                kind: "faulted",
                summary: "Worker fault; materials may be unavailable".to_owned(),
                hint: Some("reopen or inspect the session before submitting new work".to_owned()),
                model_failure: None,
                detail: Some(message.clone()),
            },
            WorkerFault::Cancelled => Self {
                kind: "faulted",
                summary: "Worker fault; materials may be unavailable".to_owned(),
                hint: Some(
                    "worker task was cancelled abnormally; reopen the session before submitting new work"
                        .to_owned(),
                ),
                model_failure: None,
                detail: None,
            },
        }
    }

    fn simple(kind: &'static str, summary: &str) -> Self {
        Self {
            kind,
            summary: summary.to_owned(),
            hint: None,
            model_failure: None,
            detail: None,
        }
    }
}

fn model_failure_summary(kind: &'static str) -> &'static str {
    match kind {
        "transient" => "Temporary provider failure",
        "timed_out" => "Model request timed out",
        "cancelled" => "Model request cancelled",
        "invalid_request" => "Model request rejected as invalid",
        "unknown_outcome" => "Response stream ended early; outcome unknown",
        "undecodable" => "Provider response could not be read",
        "credential_rejected" => "Credential rejected by provider",
        "not_found" => "Model or endpoint not found",
        "permanent" => "Model request refused by provider",
        _ => "Model request failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::DefinitionError;
    use crate::workspace::WorkspaceError;
    use async_trait::async_trait;
    use causa::kernel::{
        ArtifactStore, BlockContent, BlockId, BlockMeta, CallControl, Context, ContextBlock,
        ContextFrame, ContextPreparer, DynamicToolSource, ModelGateway, ModelInvokeError,
        ModelInvokeErrorKind, ModelItem, ModelOutput, ModelRef, ModelRequest, ModelResponse,
        ModelStopReason, PrepareError, ProcessorContext, ProcessorError, RoundInfo, SourceError,
        Tool, ToolBatch, ToolBatchProcessor, ToolCallContext, ToolCallDraft, ToolDefinition,
        ToolExecutionError, ToolResultPayload, ToolResultStatus, TurnId,
    };
    use causa::runtime::{
        ToolExecutor, ToolExecutorOptions, TurnLimits, TurnRunOptions, TurnRunner,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    struct ScriptedGateway {
        label: &'static str,
        results: Mutex<VecDeque<Result<ModelOutput, ModelInvokeError>>>,
    }

    impl ScriptedGateway {
        fn new(results: Vec<Result<ModelOutput, ModelInvokeError>>) -> Arc<Self> {
            Self::named("scripted", results)
        }

        fn named(
            label: &'static str,
            results: Vec<Result<ModelOutput, ModelInvokeError>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                label,
                results: Mutex::new(results.into()),
            })
        }
    }

    #[async_trait]
    impl ModelGateway for ScriptedGateway {
        async fn invoke(
            &self,
            _: &ModelRequest,
            _: &CallControl,
        ) -> Result<ModelOutput, ModelInvokeError> {
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("scripted gateway response missing for {}", self.label))
        }
    }

    fn text_output(text: &str, stop_reason: ModelStopReason) -> ModelOutput {
        ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(text)],
            },
            stop_reason,
        )
    }

    fn tool_call_output(names: &[&str]) -> ModelOutput {
        let items = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                ModelItem::tool_call(
                    ToolCallDraft::new(*name, serde_json::json!({}))
                        .with_provider_call_id(format!("call_{i}")),
                )
            })
            .collect();
        ModelOutput::new(ModelResponse { items }, ModelStopReason::ToolUse)
    }

    async fn run_turn(
        gateway: Arc<dyn ModelGateway>,
        executor: Arc<ToolExecutor>,
        context: Context,
        options: TurnRunOptions,
        control: CallControl,
    ) -> WorkResult {
        let runner = TurnRunner::new(gateway, executor);
        let outcome = runner
            .run_streaming(
                TurnId::new(uuid::Uuid::now_v7().to_string()),
                context,
                options,
                control,
            )
            .await;
        WorkResult::Run(outcome.result)
    }

    struct FailingPreparer;
    #[async_trait]
    impl ContextPreparer for FailingPreparer {
        async fn prepare(
            &self,
            _: &mut Context,
            _: &RoundInfo<'_>,
            _: &CallControl,
        ) -> Result<ContextFrame, PrepareError> {
            Err(PrepareError::new("preparer failure"))
        }
    }

    struct FailingDynamicSource;
    #[async_trait]
    impl DynamicToolSource for FailingDynamicSource {
        fn id(&self) -> &str {
            "failing-source"
        }
        async fn list(&self) -> Result<Vec<ToolDefinition>, SourceError> {
            Err(SourceError::Unavailable("dynamic catalog failure".into()))
        }
        async fn invoke(
            &self,
            _: &ToolCallContext,
            _: &CallControl,
            _: Option<&dyn ArtifactStore>,
        ) -> Result<ToolResultPayload, ToolExecutionError> {
            unreachable!("list fails before invoke")
        }
    }

    struct DummyTool(&'static str);
    #[async_trait]
    impl Tool for DummyTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(self.0, "dummy", serde_json::json!({"type": "object"}))
        }
        async fn execute(
            &self,
            call: &ToolCallContext,
            _: &CallControl,
            _: Option<&dyn causa::kernel::ArtifactStore>,
        ) -> ToolResultPayload {
            ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::Succeeded,
                causa::kernel::ToolOutput::new(serde_json::json!({})),
            )
        }
    }

    struct UnknownOutcomeTool;
    #[async_trait]
    impl Tool for UnknownOutcomeTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                "unknown_tool",
                "unknown",
                serde_json::json!({"type": "object"}),
            )
        }
        async fn execute(
            &self,
            call: &ToolCallContext,
            _: &CallControl,
            _: Option<&dyn causa::kernel::ArtifactStore>,
        ) -> ToolResultPayload {
            ToolResultPayload::new(
                call.call_block_id,
                ToolResultStatus::UnknownOutcome,
                causa::kernel::ToolOutput::new(serde_json::json!({})),
            )
        }
    }

    struct FailingProcessor;
    #[async_trait]
    impl ToolBatchProcessor for FailingProcessor {
        async fn process(
            &self,
            _: &mut ToolBatch,
            _: &ProcessorContext<'_>,
        ) -> Result<(), ProcessorError> {
            Err(ProcessorError::Failed("processor failure".into()))
        }
    }

    struct CollidingResultProcessor(BlockId);
    #[async_trait]
    impl ToolBatchProcessor for CollidingResultProcessor {
        async fn process(
            &self,
            batch: &mut ToolBatch,
            _: &ProcessorContext<'_>,
        ) -> Result<(), ProcessorError> {
            let call = batch.calls()[0].call().clone();
            batch
                .resolve_at(
                    0,
                    self.0,
                    ToolResultPayload::new(
                        call.call_block_id,
                        ToolResultStatus::Rejected,
                        causa::kernel::ToolOutput::new(serde_json::json!({})),
                    ),
                )
                .unwrap();
            Ok(())
        }
    }

    #[derive(Debug)]
    struct ExternalAssemblyError(&'static str);
    impl std::fmt::Display for ExternalAssemblyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for ExternalAssemblyError {}

    #[test]
    fn maps_pre_run_fault_and_assembly_variants() {
        let cases = [
            (
                WorkResult::StoppedBeforeRun(ControlError::Cancelled),
                "cancelled",
                false,
            ),
            (
                WorkResult::StoppedBeforeRun(ControlError::TimedOut),
                "timed_out",
                false,
            ),
            (
                WorkResult::Faulted(WorkerFault::Panicked {
                    message: "boom".into(),
                }),
                "faulted",
                true,
            ),
            (
                WorkResult::Faulted(WorkerFault::Cancelled),
                "faulted",
                false,
            ),
        ];
        for (result, expected_kind, has_detail) in cases {
            let outcome = WorkOutcome::of(&result);
            assert_eq!(outcome.kind, expected_kind);
            assert!(!outcome.summary.is_empty());
            assert!(outcome.model_failure.is_none());
            assert_eq!(outcome.detail.is_some(), has_detail);
        }

        let assembly_errors = vec![
            (
                AssemblyError::Definition(DefinitionError::BlankModel),
                "definition",
                true,
            ),
            (AssemblyError::CredentialUnavailable, "API key", false),
            (
                AssemblyError::Workspace(Arc::new(WorkspaceError::InvalidPath)),
                "workspace",
                true,
            ),
            (
                AssemblyError::CommandUnavailable("no seatbelt".into()),
                "sandbox",
                true,
            ),
            (
                AssemblyError::SearchUnavailable("missing rg".into()),
                "ripgrep",
                true,
            ),
            (
                AssemblyError::McpStart(Arc::new(McpStartError {
                    server_id: "docs-srv".into(),
                    reason: "connection refused".into(),
                })),
                "docs-srv",
                true,
            ),
        ];
        for (err, expected_fragment, has_detail) in assembly_errors {
            let direct = WorkOutcome::from_assembly_error(&err);
            let boxed = WorkOutcome::of(&WorkResult::AssemblyFailed(Box::new(err)));
            assert_eq!(direct, boxed);
            assert_eq!(boxed.kind, "assembly_failed");
            assert!(!boxed.summary.is_empty());
            let combined = format!(
                "{} {} {}",
                boxed.summary,
                boxed.hint.as_deref().unwrap_or(""),
                boxed.detail.as_deref().unwrap_or("")
            );
            assert!(
                combined.contains(expected_fragment),
                "missing {expected_fragment:?} in {combined:?}"
            );
            assert_eq!(boxed.detail.is_some(), has_detail);
        }

        let mcp_direct = WorkOutcome::of(&WorkResult::AssemblyFailed(Box::new(McpStartError {
            server_id: "git-srv".into(),
            reason: "timed out".into(),
        })));
        assert_eq!(mcp_direct.kind, "assembly_failed");
        assert!(mcp_direct.summary.contains("git-srv"));
        assert!(mcp_direct.hint.as_deref().unwrap().contains("git-srv"));
        assert_eq!(mcp_direct.detail.as_deref(), Some("timed out"));

        let external = WorkOutcome::of(&WorkResult::AssemblyFailed(Box::new(
            ExternalAssemblyError("custom external failure"),
        )));
        assert_eq!(external.kind, "assembly_failed");
        assert!(!external.summary.is_empty());
        assert!(external.hint.is_some());
        assert_eq!(external.detail.as_deref(), Some("custom external failure"));
    }

    #[tokio::test]
    async fn maps_all_turn_result_and_interruption_variants() {
        let empty_executor = Arc::new(ToolExecutor::new(vec![], Default::default()).unwrap());
        let base_options = || TurnRunOptions::new(ModelRef::new("fixture"));
        let open_control = || CallControl::new(CancellationToken::new(), None);

        // 1. Completed
        let completed = run_turn(
            ScriptedGateway::new(vec![Ok(text_output("ok", ModelStopReason::EndTurn))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&completed);
        assert_eq!(outcome.kind, "completed");
        assert!(!outcome.summary.is_empty());

        // 2. Cancelled
        let cancelled_token = CancellationToken::new();
        cancelled_token.cancel();
        let cancelled = run_turn(
            ScriptedGateway::new(vec![]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            CallControl::new(cancelled_token, None),
        )
        .await;
        assert_eq!(WorkOutcome::of(&cancelled).kind, "cancelled");

        // 3. DeadlineExceeded
        let timed_out = run_turn(
            ScriptedGateway::new(vec![]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            CallControl::new(
                CancellationToken::new(),
                Some(Instant::now() - Duration::from_secs(1)),
            ),
        )
        .await;
        assert_eq!(WorkOutcome::of(&timed_out).kind, "timed_out");

        // 4. ModelMaxTokens
        let max_tokens = run_turn(
            ScriptedGateway::new(vec![Ok(text_output("cut", ModelStopReason::MaxTokens))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&max_tokens);
        assert_eq!(outcome.kind, "max_tokens");
        assert!(outcome.hint.as_deref().unwrap().contains("max_tokens"));

        // 5. ModelContextWindowExceeded
        let ctx_exceeded = run_turn(
            ScriptedGateway::new(vec![Ok(text_output(
                "cut",
                ModelStopReason::ContextWindowExceeded,
            ))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&ctx_exceeded);
        assert_eq!(outcome.kind, "context_window_exceeded");
        assert!(!outcome.summary.is_empty());
        assert!(outcome.hint.as_deref().unwrap().contains("compact"));

        // 6. ModelRefusal
        let refused = run_turn(
            ScriptedGateway::new(vec![Ok(text_output("no", ModelStopReason::Refusal))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(WorkOutcome::of(&refused).kind, "refused");

        // 7. ModelStopUnrecognized
        let mut unrecognized_output = text_output("raw", ModelStopReason::Unrecognized);
        unrecognized_output.raw_stop_reason = Some("weird_stop".into());
        let stop_unrecognized = run_turn(
            ScriptedGateway::new(vec![Ok(unrecognized_output)]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&stop_unrecognized);
        assert_eq!(outcome.kind, "stop_unrecognized");
        assert_eq!(
            outcome.model_failure.as_ref().unwrap().kind,
            "unrecognized_stop"
        );
        assert!(outcome.detail.as_deref().unwrap().contains("weird_stop"));

        // 8. MaxModelRounds
        let mut zero_round_opts = base_options();
        zero_round_opts.limits = TurnLimits::new(0, zero_round_opts.limits.max_tool_calls);
        let max_rounds = run_turn(
            ScriptedGateway::new(vec![]),
            empty_executor.clone(),
            Context::new(),
            zero_round_opts,
            open_control(),
        )
        .await;
        assert_eq!(WorkOutcome::of(&max_rounds).kind, "max_model_rounds");

        // 9. MaxToolCalls
        let executor_with_dummy = Arc::new(
            ToolExecutor::new(vec![Arc::new(DummyTool("dummy"))], Default::default()).unwrap(),
        );
        let mut zero_tool_opts = base_options();
        zero_tool_opts.limits = TurnLimits::new(zero_tool_opts.limits.max_model_rounds, 0);
        let max_tools = run_turn(
            ScriptedGateway::new(vec![Ok(tool_call_output(&["dummy"]))]),
            executor_with_dummy.clone(),
            Context::new(),
            zero_tool_opts,
            open_control(),
        )
        .await;
        assert_eq!(WorkOutcome::of(&max_tools).kind, "max_tool_calls");

        // 10. ModelCallFailed
        let model_failed = run_turn(
            ScriptedGateway::new(vec![Err(ModelInvokeError::new(
                ModelInvokeErrorKind::Permanent,
                "bad key",
            )
            .with_status(401))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&model_failed);
        assert_eq!(outcome.kind, "model_failed");
        assert_eq!(
            outcome.model_failure.as_ref().unwrap().kind,
            "credential_rejected"
        );
        assert_eq!(outcome.detail.as_deref(), Some("bad key"));

        // 11. PrepareFailed
        let prepare_failed = run_turn(
            ScriptedGateway::new(vec![]),
            empty_executor.clone(),
            Context::new(),
            base_options().with_preparer(Arc::new(FailingPreparer)),
            open_control(),
        )
        .await;
        let outcome = WorkOutcome::of(&prepare_failed);
        assert_eq!(outcome.kind, "prepare_failed");
        assert!(
            outcome
                .detail
                .as_deref()
                .unwrap()
                .contains("preparer failure")
        );

        // 12. ToolCatalogFailed
        let failing_catalog = ToolExecutor::new(vec![], Default::default()).unwrap();
        failing_catalog
            .register_dynamic(Arc::new(FailingDynamicSource))
            .unwrap();
        let catalog_failed = run_turn(
            ScriptedGateway::new(vec![]),
            Arc::new(failing_catalog),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(WorkOutcome::of(&catalog_failed).kind, "tool_catalog_failed");

        // 13. ToolProcessingFailed
        let proc_failed_exec = Arc::new(
            ToolExecutor::new(
                vec![Arc::new(DummyTool("dummy"))],
                ToolExecutorOptions::default().with_before(vec![Arc::new(FailingProcessor)]),
            )
            .unwrap(),
        );
        let processing_failed = run_turn(
            ScriptedGateway::new(vec![Ok(tool_call_output(&["dummy"]))]),
            proc_failed_exec,
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(
            WorkOutcome::of(&processing_failed).kind,
            "tool_processing_failed"
        );

        // 14. UnknownToolOutcome
        let unknown_exec = Arc::new(
            ToolExecutor::new(vec![Arc::new(UnknownOutcomeTool)], Default::default()).unwrap(),
        );
        let unknown_tool = run_turn(
            ScriptedGateway::new(vec![Ok(tool_call_output(&["unknown_tool"]))]),
            unknown_exec,
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(WorkOutcome::of(&unknown_tool).kind, "unknown_tool_outcome");

        // 15. Context commit pipeline variants -> "context_commit_failed"
        // 15a. ModelConversionFailed
        let conversion_failed = run_turn(
            ScriptedGateway::new(vec![Ok(tool_call_output(&[" "]))]),
            empty_executor.clone(),
            Context::new(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(
            WorkOutcome::of(&conversion_failed).kind,
            "context_commit_failed"
        );

        // 15b. ToolCommitFailed (result block id collides with existing block)
        let existing_id = BlockId::new(uuid::Uuid::now_v7());
        let existing = ContextBlock::new(
            existing_id,
            BlockContent::text(causa::kernel::Role::User, "seed"),
            BlockMeta::default(),
        );
        let colliding_result_exec = Arc::new(
            ToolExecutor::new(
                vec![Arc::new(DummyTool("dummy"))],
                ToolExecutorOptions::default()
                    .with_before(vec![Arc::new(CollidingResultProcessor(existing_id))]),
            )
            .unwrap(),
        );
        let tool_commit_failed = run_turn(
            ScriptedGateway::named("15e_tool_commit", vec![Ok(tool_call_output(&["dummy"]))]),
            colliding_result_exec,
            Context::from_blocks(vec![existing]).unwrap(),
            base_options(),
            open_control(),
        )
        .await;
        assert_eq!(
            WorkOutcome::of(&tool_commit_failed).kind,
            "context_commit_failed"
        );
    }
}
