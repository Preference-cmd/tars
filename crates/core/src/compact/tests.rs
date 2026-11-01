//! Compaction unit and low-level integration tests.

use super::estimate::{UsageTracker, UsageTrackingGateway, context_bytes, estimate_bytes};
use super::history::{HISTORY_FORMAT, HistorySegmentDocument, HistoryStore, validate_index};
use super::preparer::CompactionPreparer;
use super::summary;
use super::{CompactionRuntime, HistorySegment, HistoryTrigger};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::session::{Session, SessionConfig, SubmitRequest, WorkState};
use causa::kernel::{
    BlockContent, BlockId, CallControl, ContentPart, Context, ContextBlock, ModelGateway,
    ModelInvokeError, ModelOutput, ModelRequest, ModelResponse, ModelStopReason, Role,
    ToolCallContext, ToolCallPayload, ToolDefinition, ToolOutput, ToolResultPayload,
    ToolResultStatus,
};
use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnLimits, TurnRunOptions, TurnRunner};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn definition(context_window: Option<u64>, max_tokens: Option<u32>) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARS_FIXTURE_KEY".into()),
        work_timeout_secs: None,
        retained_work_capacity: 8,
        max_model_rounds: 3,
        max_tool_calls: 8,
        max_tokens,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window,
        auto_compact: true,
        compact_threshold: 70,
    }
}

fn big_block(bytes: usize) -> ContextBlock {
    ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "y".repeat(bytes),
    )
}

fn tool_change(tool: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolChange(causa::kernel::ToolChange::new(
            vec![ToolDefinition::new(
                tool,
                "fixture tool",
                serde_json::json!({"type": "object"}),
            )],
            Vec::new(),
        )),
        Default::default(),
    )
}

fn is_summary(request: &ModelRequest) -> bool {
    request.frame.blocks.iter().any(|block| {
        let BlockContent::Message { parts, .. } = block.content() else {
            return false;
        };
        parts.iter().any(|part| {
            let ContentPart::Text(text) = part else {
                return false;
            };
            text.0.contains("Prepare a continuation brief")
        })
    })
}

fn summary_output(text: &str) -> ModelOutput {
    let mut output = ModelOutput::new(
        ModelResponse {
            items: vec![causa::kernel::ModelItem::text(text)],
        },
        ModelStopReason::EndTurn,
    );
    output.usage = Some(causa::kernel::ModelUsage::new(200, 20));
    output
}

/// Round 0 answers with a tool call, later rounds finish; summaries answer text.
struct ToolThenDone {
    main_calls: AtomicUsize,
    summary_calls: Arc<AtomicUsize>,
    summary_inputs: Arc<std::sync::Mutex<Vec<Vec<ContextBlock>>>>,
    /// Whether the first ordinary round asks for the fixture tool.
    tool_call_first: bool,
}

#[async_trait::async_trait]
impl ModelGateway for ToolThenDone {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        if is_summary(request) {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            self.summary_inputs
                .lock()
                .unwrap()
                .push(request.frame.blocks.clone());
            return Ok(summary_output("Goal: continue with the fixture tool."));
        }
        let call = self.main_calls.fetch_add(1, Ordering::SeqCst);
        let mut output = if call == 0 && self.tool_call_first {
            ModelOutput::new(
                ModelResponse {
                    items: vec![causa::kernel::ModelItem::tool_call(
                        causa::kernel::ToolCallDraft::new(
                            "alpha",
                            serde_json::json!({"input": "fixture"}),
                        ),
                    )],
                },
                ModelStopReason::ToolUse,
            )
        } else {
            ModelOutput::new(
                ModelResponse {
                    items: vec![causa::kernel::ModelItem::text("done")],
                },
                ModelStopReason::EndTurn,
            )
        };
        output.usage = Some(causa::kernel::ModelUsage::new(100, 10));
        Ok(output)
    }
}

/// Round 0 asks for the fixture tool and waits for release; later rounds finish.
struct ControlledRoundGateway {
    main_calls: AtomicUsize,
    summary_calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl ModelGateway for ControlledRoundGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        if is_summary(request) {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(summary_output("Goal: continue with the fixture tool."));
        }
        let call = self.main_calls.fetch_add(1, Ordering::SeqCst);
        let mut output = if call == 0 {
            self.entered.notify_one();
            self.release.notified().await;
            ModelOutput::new(
                ModelResponse {
                    items: vec![causa::kernel::ModelItem::tool_call(
                        causa::kernel::ToolCallDraft::new(
                            "alpha",
                            serde_json::json!({"input": "fixture"}),
                        ),
                    )],
                },
                ModelStopReason::ToolUse,
            )
        } else {
            ModelOutput::new(
                ModelResponse {
                    items: vec![causa::kernel::ModelItem::text("done")],
                },
                ModelStopReason::EndTurn,
            )
        };
        output.usage = Some(causa::kernel::ModelUsage::new(100, 10));
        Ok(output)
    }
}

/// Same fixture call with a smaller result so round 1 lands between 50% and 70%.
struct SmallAlphaTool;

#[async_trait::async_trait]
impl causa::kernel::Tool for SmallAlphaTool {
    fn definition(&self) -> ToolDefinition {
        AlphaTool.definition()
    }

    async fn execute(
        &self,
        ctx: &ToolCallContext,
        _: &CallControl,
        _: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        ToolResultPayload::new(
            ctx.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("z".repeat(1_500))),
        )
    }
}

struct AlphaTool;

#[async_trait::async_trait]
impl causa::kernel::Tool for AlphaTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "alpha",
            "fixture tool",
            serde_json::json!({"type": "object"}),
        )
    }

    async fn execute(
        &self,
        ctx: &ToolCallContext,
        _: &CallControl,
        _: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> ToolResultPayload {
        ToolResultPayload::new(
            ctx.call_block_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("z".repeat(6_000))),
        )
    }
}

fn build_session(
    directory: &std::path::Path,
    definition: SessionDefinition,
    context: Context,
    gateway: Arc<dyn ModelGateway>,
    tracker: Arc<UsageTracker>,
) -> (Session, Arc<CompactionRuntime>) {
    build_session_with_tool(
        directory,
        definition,
        context,
        gateway,
        tracker,
        Arc::new(AlphaTool),
    )
}

fn build_session_with_tool(
    directory: &std::path::Path,
    definition: SessionDefinition,
    context: Context,
    gateway: Arc<dyn ModelGateway>,
    tracker: Arc<UsageTracker>,
    tool: Arc<dyn causa::kernel::Tool>,
) -> (Session, Arc<CompactionRuntime>) {
    let tracked: Arc<dyn ModelGateway> =
        Arc::new(UsageTrackingGateway::new(gateway, tracker.clone()));
    let runtime = CompactionRuntime::new(
        &definition,
        &"conversation".into(),
        Some(directory),
        Vec::new(),
        None,
        None,
        tracked.clone(),
        tracker,
    )
    .unwrap();
    let executor = Arc::new(ToolExecutor::new(vec![tool], ToolExecutorOptions::default()).unwrap());
    let runner = Arc::new(TurnRunner::new(tracked, executor));
    let runtime_for_work = runtime.clone();
    let session = Session::new_with_tools(
        "conversation".into(),
        context,
        move |_work, _control, reporter| {
            let runner = runner.clone();
            let runtime = runtime_for_work.clone();
            async move {
                let options = TurnRunOptions::new(causa::kernel::ModelRef::new("fixture"))
                    .with_limits(TurnLimits::new(3, 8))
                    .with_preparer(Arc::new(CompactionPreparer::new(runtime, reporter)));
                Ok((runner, options))
            }
        },
        SessionConfig {
            retained_work_capacity: 8,
            work_timeout: None,
        },
    )
    .with_compaction(runtime.clone());
    (session, runtime)
}

#[tokio::test]
async fn compaction_keeps_the_bound_tool_declaration_and_causa_accepts_the_frame() {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
    });
    let tracker = Arc::new(UsageTracker::default());
    let mut context = Context::new();
    context
        .edit()
        .append([tool_change("alpha"), big_block(1_200)])
        .commit()
        .unwrap();
    let (mut session, runtime) = build_session(
        directory.path(),
        definition(Some(100), None),
        context,
        gateway,
        tracker,
    );
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "compact".into(),
            blocks: Vec::new(),
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = &observation.state else {
        panic!("work unfinished")
    };
    assert!(matches!(
        &**result,
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    let blocks = session
        .with_context(|context| context.blocks().to_vec())
        .unwrap();
    assert!(matches!(blocks[0].content(), BlockContent::ToolChange(_)));
    assert_eq!(
        blocks[1].meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    let BlockContent::Message { role, .. } = blocks[2].content() else {
        panic!("final answer is a message")
    };
    assert_eq!(*role, Role::Assistant);
    let tools = causa::kernel::current_tools(&blocks);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "alpha");
    assert_eq!(runtime.entries().len(), 1);
    session.shutdown().await;
}

#[tokio::test]
async fn a_later_round_keeps_the_submitted_request_verbatim_in_the_summary() {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let summary_inputs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        summary_inputs: summary_inputs.clone(),
        tool_call_first: true,
    });
    let tracker = Arc::new(UsageTracker::default());
    let (mut session, _runtime) = build_session(
        directory.path(),
        definition(Some(1_000), None),
        Context::new(),
        gateway,
        tracker,
    );
    let handle = session.handle();
    let request_text = "please call the alpha fixture tool";
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "in-turn".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                request_text,
            )],
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = &observation.state else {
        panic!("work unfinished")
    };
    assert!(matches!(
        &**result,
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    let inputs = summary_inputs.lock().unwrap().clone();
    let last = inputs.last().expect("one summary request");
    let instruction = last.last().expect("instruction block");
    let BlockContent::Message { parts, .. } = instruction.content() else {
        panic!("instruction is a message")
    };
    let text = parts
        .iter()
        .find_map(|part| match part {
            ContentPart::Text(payload) => Some(payload.0.clone()),
            _ => None,
        })
        .expect("instruction text");
    assert!(
        text.contains(request_text),
        "round 1 keeps the original request: {text}"
    );
    session.shutdown().await;
}

#[test]
fn byte_estimation_is_one_token_per_three_rounded_up() {
    assert_eq!(estimate_bytes(0), 0);
    assert_eq!(estimate_bytes(1), 1);
    assert_eq!(estimate_bytes(3), 1);
    assert_eq!(estimate_bytes(4), 2);
    let context = Context::from_blocks(vec![big_block(300)]).unwrap();
    let bytes = context_bytes(&context);
    assert!(bytes >= 300);
}

#[test]
fn usage_estimation_uses_the_previous_round_and_resets_after_compaction() {
    let tracker = UsageTracker::default();
    assert_eq!(tracker.estimate(900), 300);
    tracker.note_prepare_bytes(900);
    tracker.record_output(&summary_output("x"));
    // The recorded output reported 200 input tokens; the delta since the last
    // preparation adds one token per three new bytes.
    assert_eq!(tracker.estimate(900 + 600), 400);
    tracker.reset();
    assert_eq!(tracker.estimate(900 + 600), 500);
}

#[test]
fn history_store_writes_reads_and_cleans_unreferenced_segments() {
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation", Vec::new());
    let segment_id = uuid::Uuid::now_v7().to_string();
    let summary_id = BlockId::new(uuid::Uuid::now_v7());
    let document = HistorySegmentDocument {
        format: HISTORY_FORMAT.into(),
        conversation_id: "conversation".into(),
        segment_id: segment_id.clone(),
        created_at_ms: 1,
        trigger: HistoryTrigger::Auto,
        estimate_before: 100,
        estimate_after: 10,
        summary_block_id: summary_id,
        context: Context::from_blocks(vec![big_block(30)]).unwrap(),
    };
    store.write(&document).unwrap();
    store.append(document.index_entry());
    let read = store.read(&segment_id).unwrap();
    assert_eq!(read.context.blocks().len(), 1);
    assert_eq!(read.trigger, HistoryTrigger::Auto);
    // Writing the same segment again never overwrites it.
    assert!(store.write(&document).is_err());
    // A stray file is removed; the referenced one stays.
    std::fs::write(
        directory
            .path()
            .join("conversation.history")
            .join("stray.json"),
        b"{}",
    )
    .unwrap();
    super::history::cleanup_segments(directory.path(), "conversation", &store.entries()).unwrap();
    assert!(
        !directory
            .path()
            .join("conversation.history")
            .join("stray.json")
            .exists()
    );
    assert!(
        directory
            .path()
            .join("conversation.history")
            .join(format!("{segment_id}.json"))
            .exists()
    );
    // A missing referenced file is reported on read, not on validation.
    store.remove(&segment_id);
    assert!(store.read(&segment_id).is_err());
}

#[test]
fn history_index_validation_rejects_duplicates_and_bad_identity() {
    let id = uuid::Uuid::now_v7().to_string();
    let entry = |id: &str| HistorySegment {
        id: id.into(),
        created_at_ms: 0,
        trigger: HistoryTrigger::Manual,
        blocks: 1,
        summary_block_id: BlockId::new(uuid::Uuid::now_v7()),
    };
    assert!(validate_index(&[entry(&id)]).is_ok());
    assert!(validate_index(&[entry(&id), entry(&id)]).is_err());
    assert!(validate_index(&[entry("not-a-uuid")]).is_err());
}

#[test]
fn summary_trimming_drops_the_earliest_blocks_and_orphan_results() {
    let call_id = BlockId::new(uuid::Uuid::now_v7());
    let call = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new("alpha", serde_json::json!({}))),
        Default::default(),
    );
    let result = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("ok")),
        )),
        Default::default(),
    );
    let blocks = vec![big_block(900), call, result, big_block(900)];
    let trimmed = summary::trim_to_limit(&blocks, 10);
    assert!(
        trimmed
            .iter()
            .all(|block| !matches!(block.content(), BlockContent::ToolResult(_))),
        "an orphan result is not sent"
    );
    // Replacement blocks declare the current tools and mark the summary.
    let replacement =
        summary::replacement_blocks(&blocks, BlockId::new(uuid::Uuid::now_v7()), "brief");
    assert_eq!(replacement.len(), 1);
    assert_eq!(
        replacement[0].meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    let with_tools = vec![tool_change("alpha")];
    let replacement =
        summary::replacement_blocks(&with_tools, BlockId::new(uuid::Uuid::now_v7()), "brief");
    assert_eq!(replacement.len(), 2);
    assert!(matches!(
        replacement[0].content(),
        BlockContent::ToolChange(_)
    ));
}

#[test]
fn instruction_is_structured_and_carries_the_current_request() {
    let text = summary::instruction(None);
    for heading in [
        "Goal:",
        "Decisions:",
        "Progress:",
        "Next steps:",
        "Relevant files:",
        "Important context:",
    ] {
        assert!(text.contains(heading), "missing {heading}");
    }
    assert!(!text.contains("verbatim:"));
    let with_request = summary::instruction(Some("finish the fixture"));
    assert!(with_request.contains("finish the fixture"));
}

#[test]
fn trigger_rules_use_the_threshold_the_output_reserve_and_the_force_flag() {
    let directory = tempfile::tempdir().unwrap();
    let tracker = Arc::new(UsageTracker::default());
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
    }) as Arc<dyn ModelGateway>;
    let runtime = CompactionRuntime::new(
        &definition(Some(1_000), None),
        &"conversation".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        gateway,
        tracker.clone(),
    )
    .unwrap();
    assert_eq!(runtime.trigger_for(699), None);
    assert_eq!(runtime.trigger_for(700), Some(HistoryTrigger::Auto));
    runtime.set_auto_compact(false);
    assert_eq!(runtime.trigger_for(999), None);
    runtime.request_force();
    assert_eq!(runtime.trigger_for(10), Some(HistoryTrigger::Passive));
    // The force flag is consumed once.
    assert_eq!(runtime.trigger_for(10), None);
    runtime.set_threshold(95);
    runtime.set_auto_compact(true);
    assert_eq!(runtime.trigger_for(949), None);
    assert_eq!(runtime.trigger_for(950), Some(HistoryTrigger::Auto));

    // The configured output reserve forces a compaction below the threshold.
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
    }) as Arc<dyn ModelGateway>;
    let reserve = CompactionRuntime::new(
        &definition(Some(1_000), Some(800)),
        &"conversation".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        gateway,
        tracker,
    )
    .unwrap();
    assert_eq!(reserve.trigger_for(100), None);
    assert_eq!(reserve.trigger_for(250), Some(HistoryTrigger::Auto));
}

#[test]
fn record_overrides_only_report_values_that_differ_from_the_definition() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
    }) as Arc<dyn ModelGateway>;
    let tracker = Arc::new(UsageTracker::default());
    let definition = definition(Some(1_000), None);
    let runtime = CompactionRuntime::new(
        &definition,
        &"conversation".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        gateway,
        tracker,
    )
    .unwrap();
    assert_eq!(runtime.record_overrides(&definition), (None, None));
    runtime.set_auto_compact(false);
    assert_eq!(runtime.record_overrides(&definition), (Some(false), None));
    runtime.set_threshold(50);
    assert_eq!(
        runtime.record_overrides(&definition),
        (Some(false), Some(50))
    );
}

#[test]
fn summary_block_has_a_fixed_prefix_and_provenance() {
    let block = summary::summary_block(BlockId::new(uuid::Uuid::now_v7()), "body");
    assert_eq!(
        block.meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    match block.content() {
        BlockContent::Message { role, parts } => {
            assert_eq!(*role, Role::User);
            assert!(parts.iter().any(|part| matches!(
                part,
                ContentPart::Text(payload)
                    if payload.0.starts_with(summary::SUMMARY_PREFIX) && payload.0.contains("body")
            )));
        }
        _ => panic!("summary is a message"),
    }
}

#[tokio::test]
async fn disabling_automatic_compaction_during_a_work_applies_to_the_next_round() {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(ControlledRoundGateway {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let tracker = Arc::new(UsageTracker::default());
    let (mut session, runtime) = build_session(
        directory.path(),
        definition(Some(1_000), None),
        Context::new(),
        gateway.clone(),
        tracker,
    );
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "runtime-change".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "run the fixture tool",
            )],
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gateway.entered.notified())
        .await
        .unwrap();
    // The next round would otherwise cross the threshold because of the tool
    // result; the running switch changed while the work was active.
    runtime.set_auto_compact(false);
    gateway.release.notify_one();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 0);
    assert!(
        session
            .with_context(|context| context.blocks().len())
            .unwrap()
            > 2
    );

    // Enabling it again and completing another round compacts as expected.
    runtime.set_auto_compact(true);
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "after-change".into(),
            blocks: vec![big_block(1_200), big_block(1_200), big_block(1_200)],
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    session.shutdown().await;
}

#[tokio::test]
async fn lowering_the_threshold_during_a_work_applies_to_the_next_round() {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(ControlledRoundGateway {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let tracker = Arc::new(UsageTracker::default());
    let (mut session, runtime) = build_session_with_tool(
        directory.path(),
        definition(Some(1_000), None),
        Context::new(),
        gateway.clone(),
        tracker,
        Arc::new(SmallAlphaTool),
    );
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "threshold-change".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "run the fixture tool",
            )],
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gateway.entered.notified())
        .await
        .unwrap();
    // 50% of the window triggers on the next round; 70% would not.
    runtime.set_threshold(50);
    gateway.release.notify_one();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    session.shutdown().await;
}
