//! Compaction unit and low-level integration tests.

use super::estimate::{UsageTracker, UsageTrackingGateway, context_bytes, estimate_bytes};
use super::history::{
    HISTORY_FORMAT, HistoryRecord, HistoryStore, RangeReplacement, RewriteKind, validate_index,
};
use super::preparer::CompactionPreparer;
use super::summary;
use super::{CompactionRuntime, HistoryRecordEntry, HistoryTrigger};
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
        prompt_cache: true,
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
    /// Cache read tokens reported with every ordinary round.
    cache_read_tokens: Option<usize>,
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
        let mut usage = causa::kernel::ModelUsage::new(100, 10);
        usage.cache_read_tokens = self.cache_read_tokens;
        output.usage = Some(usage);
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
        cache_read_tokens: None,
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
        cache_read_tokens: None,
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
    // The replacement carries the request verbatim too, so the rest of the
    // turn does not depend on the brief restating it.
    let blocks = session
        .with_context(|context| context.blocks().to_vec())
        .unwrap();
    let summary_text = blocks
        .iter()
        .find(|block| block.meta().source.as_deref() == Some(summary::SUMMARY_SOURCE))
        .and_then(|block| match block.content() {
            BlockContent::Message { parts, .. } => parts.iter().find_map(|part| match part {
                ContentPart::Text(payload) => Some(payload.0.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("summary block text");
    assert!(
        summary_text.contains("Goal: continue with the fixture tool."),
        "{summary_text}"
    );
    assert!(
        summary_text.ends_with(&format!(
            "The user's current request, verbatim:\n---\n{request_text}\n---"
        )),
        "{summary_text}"
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
fn a_round_without_usage_keeps_counting_its_material_from_bytes() {
    let tracker = UsageTracker::default();
    tracker.note_prepare_bytes(900);
    tracker.record_output(&summary_output("x"));
    assert_eq!(tracker.estimate(900), 200);
    // The next round prepares 1_500 bytes but its response has no usage.
    tracker.note_prepare_bytes(1_500);
    let mut silent = summary_output("x");
    silent.usage = None;
    tracker.record_output(&silent);
    // The base stays 200 tokens at 900 bytes; the 900 bytes added since,
    // including that round's 600, count at one token per three.
    assert_eq!(tracker.estimate(1_800), 500);
    // Usage without a noted preparation has no size to pair with.
    tracker.reset();
    tracker.record_output(&summary_output("x"));
    assert_eq!(tracker.estimate(900), 300);
}

#[test]
fn usage_estimation_includes_cache_read_tokens_in_prompt_base() {
    let tracker = UsageTracker::default();
    tracker.note_prepare_bytes(900);
    let mut output = summary_output("x");
    let mut usage = causa::kernel::ModelUsage::new(100, 10);
    usage.cache_read_tokens = Some(900);
    output.usage = Some(usage);
    tracker.record_output(&output);
    // Base is prompt total (100 input + 900 cache read = 1000), not ~100.
    assert!(tracker.estimate(900) >= 1_000);
    assert_eq!(tracker.estimate(900), 1_000);
    assert!(tracker.estimate(900 + 30) >= 1_000);
    assert_eq!(tracker.estimate(900 + 30), 1_010);
}

#[test]
fn usage_estimation_includes_cache_write_tokens_in_prompt_base() {
    let tracker = UsageTracker::default();
    tracker.note_prepare_bytes(900);
    let mut output = summary_output("x");
    let mut usage = causa::kernel::ModelUsage::new(100, 10);
    usage.cache_write_tokens = Some(50);
    output.usage = Some(usage);
    tracker.record_output(&output);
    // Base is prompt total (100 input + 0 cache read + 50 cache write = 150).
    assert!(tracker.estimate(900) >= 150);
    assert_eq!(tracker.estimate(900), 150);
}

#[test]
fn usage_estimation_defaults_to_input_tokens_when_cache_tokens_are_none() {
    let tracker = UsageTracker::default();
    tracker.note_prepare_bytes(900);
    let mut output = summary_output("x");
    let usage = causa::kernel::ModelUsage::new(100, 10);
    output.usage = Some(usage);
    tracker.record_output(&output);
    // Read and write are None -> base equals input tokens.
    assert_eq!(tracker.estimate(900), 100);
}

#[test]
fn history_store_writes_reads_and_cleans_unreferenced_records() {
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation", Vec::new());
    let record_id_value = uuid::Uuid::now_v7().to_string();
    let summary_id = BlockId::new(uuid::Uuid::now_v7());
    let document = HistoryRecord {
        format: HISTORY_FORMAT.into(),
        conversation_id: "conversation".into(),
        record_id: record_id_value.clone(),
        created_at_ms: 1,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 100,
        estimate_after: 10,
        replacements: vec![RangeReplacement {
            range: 0..1,
            originals: vec![big_block(30)],
            inserted_ids: vec![summary_id],
        }],
    };
    store.write(&document).unwrap();
    store.append(document.index_entry());
    let read = store.read(&record_id_value).unwrap();
    assert_eq!(read.replacements[0].originals.len(), 1);
    assert_eq!(read.trigger, HistoryTrigger::Auto);
    // Writing the same record again never overwrites it.
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
    super::history::cleanup_records(directory.path(), "conversation", &store.entries()).unwrap();
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
            .join(format!("{record_id_value}.json"))
            .exists()
    );
    // A missing referenced file is reported on read, not on validation.
    store.remove(&record_id_value);
    assert!(store.read(&record_id_value).is_err());
}

#[test]
fn history_index_validation_rejects_duplicates_and_bad_identity() {
    let id = uuid::Uuid::now_v7().to_string();
    let entry = |id: &str| HistoryRecordEntry {
        record_id: id.into(),
        created_at_ms: 0,
        trigger: HistoryTrigger::Manual,
        kind: RewriteKind::Summarize,
        removed_block_count: 1,
        inserted_block_ids: vec![BlockId::new(uuid::Uuid::now_v7())],
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
fn trimmed_summary_input_starts_with_the_tool_declaration_and_a_user_message() {
    let assistant =
        |text: &str| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::Assistant, text);
    let user =
        |text: &str| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, text);
    let blocks = vec![
        tool_change("alpha"),
        big_block(900),
        assistant("first answer"),
        user("second question"),
        assistant("second answer"),
    ];
    // The cut falls after the large first message, on an assistant block.
    let trimmed = summary::trim_to_limit(&blocks, 200);
    assert_eq!(trimmed.len(), 3, "{trimmed:?}");
    let BlockContent::ToolChange(_) = trimmed[0].content() else {
        panic!("the dropped declaration is restated first")
    };
    assert_eq!(causa::kernel::current_tools(&trimmed)[0].name, "alpha");
    let BlockContent::Message { role, .. } = trimmed[1].content() else {
        panic!("a message follows the declaration")
    };
    assert_eq!(*role, Role::User);
    assert_eq!(trimmed[1].id(), blocks[3].id());
    // Nothing is restated when the input already fits.
    assert_eq!(summary::trim_to_limit(&blocks, 10_000).len(), blocks.len());
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
        cache_read_tokens: None,
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
        cache_read_tokens: None,
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
        cache_read_tokens: None,
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

/// Same fixture call with a tiny result, so round 1 adds only a few tokens.
struct TinyAlphaTool;

#[async_trait::async_trait]
impl causa::kernel::Tool for TinyAlphaTool {
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
            ToolOutput::new(serde_json::json!("ok")),
        )
    }
}

/// Runs one tool round reporting 100 uncached input tokens plus `cache_read_tokens`
/// with a 1,200-token window and the default 70% threshold (840 tokens), and
/// returns how many summary requests were made.
async fn summary_calls_with_cache_read(cache_read_tokens: Option<usize>) -> usize {
    let directory = tempfile::tempdir().unwrap();
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: true,
        cache_read_tokens,
    });
    let (mut session, _runtime) = build_session_with_tool(
        directory.path(),
        definition(Some(1_200), None),
        Context::new(),
        gateway,
        Arc::new(UsageTracker::default()),
        Arc::new(TinyAlphaTool),
    );
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "cache-compaction".into(),
            blocks: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "run the fixture tool",
            )],
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    session.shutdown().await;
    summary_calls.load(Ordering::SeqCst)
}

#[tokio::test]
async fn auto_compaction_triggers_when_cache_read_usage_crosses_threshold() {
    // 100 input + 900 cache read = 1,000 prompt tokens, above 840.
    assert_eq!(summary_calls_with_cache_read(Some(900)).await, 1);
}

#[tokio::test]
async fn auto_compaction_stays_below_threshold_on_uncached_input_alone() {
    // Same rounds with 100 prompt tokens stay below 840.
    assert_eq!(summary_calls_with_cache_read(None).await, 0);
}

fn history_files(directory: &std::path::Path, conversation: &str) -> Vec<String> {
    match std::fs::read_dir(directory.join(format!("{conversation}.history"))) {
        Ok(listing) => {
            let mut names = listing
                .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                .collect::<Vec<_>>();
            names.sort();
            names
        }
        Err(_) => Vec::new(),
    }
}

fn summarize_rewrite(
    replacements: Vec<super::history::PlannedReplacement>,
) -> super::history::Rewrite {
    super::history::Rewrite {
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements,
    }
}

#[test]
fn test_be01_shared_commit_path_v1_1_format_multi_range_and_abandonment() {
    use super::history::{PlannedReplacement, RewriteError};
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation-1", Vec::new());

    let b0 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "b0");
    let b1 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::Assistant, "b1");
    let b2 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "b2");
    let b3 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::Assistant, "b3");
    let mut context =
        Context::from_blocks(vec![b0.clone(), b1.clone(), b2.clone(), b3.clone()]).unwrap();

    let r0_id = BlockId::new(uuid::Uuid::now_v7());
    let r0 = ContextBlock::text(r0_id, Role::User, "r0-replacement");
    let r2a_id = BlockId::new(uuid::Uuid::now_v7());
    let r2a = ContextBlock::text(r2a_id, Role::User, "r2-replacement-a");
    let r2b_id = BlockId::new(uuid::Uuid::now_v7());
    let r2b = ContextBlock::text(r2b_id, Role::User, "r2-replacement-b");

    // Two ranges around a kept block go through one edit.
    let entry = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(vec![
                PlannedReplacement {
                    range: 0..1,
                    with: vec![r0.clone()],
                },
                PlannedReplacement {
                    range: 2..4,
                    with: vec![r2a.clone(), r2b.clone()],
                },
            ]),
        )
        .unwrap();
    let ids = context
        .blocks()
        .iter()
        .map(ContextBlock::id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![r0_id, b1.id(), r2a_id, r2b_id]);

    let record_id = entry.record_id.clone();
    let file_path = directory
        .path()
        .join("conversation-1.history")
        .join(format!("{record_id}.json"));
    let file_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file_path).unwrap()).unwrap();
    assert_eq!(file_json["format"], "tars-history-v1.1");
    assert_eq!(file_json["kind"], "summarize");
    assert_eq!(file_json["record_id"], record_id.as_str());
    let replacements = file_json["replacements"].as_array().unwrap();
    assert_eq!(replacements.len(), 2);
    assert_eq!(
        replacements[0]["range"],
        serde_json::json!({"start": 0, "end": 1})
    );
    assert_eq!(
        replacements[0]["originals"],
        serde_json::to_value(vec![&b0]).unwrap()
    );
    assert_eq!(
        replacements[0]["inserted_ids"],
        serde_json::to_value(vec![r0_id]).unwrap()
    );
    assert_eq!(
        replacements[1]["range"],
        serde_json::json!({"start": 2, "end": 4})
    );
    assert_eq!(
        replacements[1]["originals"],
        serde_json::to_value(vec![&b2, &b3]).unwrap()
    );
    assert_eq!(
        replacements[1]["inserted_ids"],
        serde_json::to_value(vec![r2a_id, r2b_id]).unwrap()
    );
    assert_eq!(store.read(&record_id).unwrap().index_entry(), entry);

    assert_eq!(entry.kind, RewriteKind::Summarize);
    assert_eq!(entry.removed_block_count, 3);
    assert_eq!(entry.inserted_block_ids, vec![r0_id, r2a_id, r2b_id]);
    assert_eq!(store.entries(), vec![entry.clone()]);

    // Refused before writing: a range past the end, overlapping ranges, or an
    // empty insertion leave no file, no index entry and the same Context.
    let before = context.blocks().to_vec();
    for replacements in [
        vec![PlannedReplacement {
            range: 3..9,
            with: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "x",
            )],
        }],
        vec![
            PlannedReplacement {
                range: 0..2,
                with: vec![ContextBlock::text(
                    BlockId::new(uuid::Uuid::now_v7()),
                    Role::User,
                    "x",
                )],
            },
            PlannedReplacement {
                range: 1..3,
                with: vec![ContextBlock::text(
                    BlockId::new(uuid::Uuid::now_v7()),
                    Role::User,
                    "y",
                )],
            },
        ],
        vec![PlannedReplacement {
            range: 1..2,
            with: Vec::new(),
        }],
        Vec::new(),
    ] {
        let error = store
            .commit_rewrite(&mut context, summarize_rewrite(replacements))
            .unwrap_err();
        assert!(matches!(error, RewriteError::Rejected(_)), "{error}");
    }
    assert_eq!(context.blocks(), before.as_slice());
    assert_eq!(
        history_files(directory.path(), "conversation-1"),
        vec![format!("{record_id}.json")]
    );
    assert_eq!(store.entries(), vec![entry.clone()]);

    // Refused by Causa after the record was written: reusing a kept block's
    // identity with other content. The written file is removed again.
    let error = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(vec![PlannedReplacement {
                range: 0..1,
                with: vec![ContextBlock::text(b1.id(), Role::User, "changed")],
            }]),
        )
        .unwrap_err();
    assert!(matches!(error, RewriteError::Rejected(_)), "{error}");
    assert_eq!(context.blocks(), before.as_slice());
    assert_eq!(
        history_files(directory.path(), "conversation-1"),
        vec![format!("{record_id}.json")]
    );
    assert_eq!(store.entries(), vec![entry]);

    // An unindexed file left by an interrupted rewrite is removed on reopen.
    let stray_path = directory
        .path()
        .join("conversation-1.history")
        .join("stray-unindexed.json");
    std::fs::write(&stray_path, b"{}").unwrap();
    super::history::cleanup_records(directory.path(), "conversation-1", &store.entries()).unwrap();
    assert!(!stray_path.exists(), "unindexed file cleaned up on reopen");
    assert!(file_path.exists(), "referenced file remains");
}

#[tokio::test]
async fn test_be03_whole_context_compaction_structure_triggers_and_abandon_conditions() {
    // 1. Structure: whole-context replacement replaces 0..len, kept tail empty,
    // result is tool declaration + one continuation brief.
    let original_blocks = vec![
        tool_change("fixture_tool"),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::User,
            "User prompt 1",
        ),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::Assistant,
            "Assistant reply 1",
        ),
    ];
    let summary_id = BlockId::new(uuid::Uuid::now_v7());
    let replacement =
        summary::replacement_blocks(&original_blocks, summary_id, "Goal: carry out task.");
    assert_eq!(replacement.len(), 2);
    assert!(matches!(
        replacement[0].content(),
        BlockContent::ToolChange(_)
    ));
    assert_eq!(
        causa::kernel::current_tools(&replacement)[0].name,
        "fixture_tool"
    );
    assert_eq!(
        replacement[1].meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    assert_eq!(replacement[1].id(), summary_id);

    // 2. Triggers: Auto, Manual, Passive
    let directory = tempfile::tempdir().unwrap();
    let tracker = Arc::new(UsageTracker::default());
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
        cache_read_tokens: None,
    }) as Arc<dyn ModelGateway>;
    let runtime = CompactionRuntime::new(
        &definition(Some(1_000), None),
        &"conversation-be03".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        gateway,
        tracker,
    )
    .unwrap();
    assert_eq!(runtime.trigger_for(700), Some(HistoryTrigger::Auto));
    assert_eq!(runtime.trigger_for(699), None);
    runtime.request_force();
    assert_eq!(runtime.trigger_for(10), Some(HistoryTrigger::Passive));

    // 3. Abandon conditions:
    // a. Unpaired materials abandon
    let unpaired_context = Context::from_blocks(vec![ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new("tool", serde_json::json!({}))),
        Default::default(),
    )])
    .unwrap();
    let control = CallControl::new(causa::kernel::CancellationToken::new(), None);
    let plan_res = runtime
        .plan(
            &unpaired_context,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
            None,
            &control,
        )
        .await;
    assert_eq!(
        plan_res.unwrap_err(),
        super::CompactionAbandon::UnpairedMaterials
    );

    // b. Empty summary abandon
    struct EmptyGateway;
    #[async_trait::async_trait]
    impl ModelGateway for EmptyGateway {
        async fn invoke(
            &self,
            _: &ModelRequest,
            _: &CallControl,
        ) -> Result<ModelOutput, ModelInvokeError> {
            Ok(summary_output("   "))
        }
    }
    let empty_runtime = CompactionRuntime::new(
        &definition(Some(1_000), None),
        &"conversation-empty".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        Arc::new(EmptyGateway),
        Arc::new(UsageTracker::default()),
    )
    .unwrap();
    let valid_context = Context::from_blocks(vec![big_block(2_000)]).unwrap();
    let empty_res = empty_runtime
        .plan(
            &valid_context,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
            None,
            &control,
        )
        .await;
    assert_eq!(
        empty_res.unwrap_err(),
        super::CompactionAbandon::EmptySummary
    );
}

#[test]
fn test_be04_expansion_fixture_summarize_elide_merge_nesting_and_lazy_reading() {
    use super::expansion::{BlockStatus, expand_conversation};

    let directory = tempfile::tempdir().unwrap();
    let session_dir = directory.path().join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    let conv_id = "test-conv-be04";

    // 1. Build unrewritten conversation of 8 blocks:
    let b0 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "User prompt",
    );
    let b1 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "Assistant reply with tool call",
    );
    let b2 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            b1.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("Large tool output")),
        )),
        Default::default(),
    );
    let b3 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "Assistant post-tool answer",
    );
    let b4 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "User follow-up",
    );
    let b5 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "Assistant follow-up",
    );
    let b6 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "User final question",
    );
    let b7 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "Assistant final answer",
    );

    let original_blocks = [
        b0.clone(),
        b1.clone(),
        b2.clone(),
        b3.clone(),
        b4.clone(),
        b5.clone(),
        b6.clone(),
        b7.clone(),
    ];

    // 2. Hand-built records with summarize + elide + merge + nesting:
    // Record 1 (Elide): Elides B2 (tool result) in place -> replaced by E1
    let rec1_id = uuid::Uuid::now_v7().to_string();
    let e1 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            b1.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("[elided]")),
        )),
        Default::default(),
    );
    let rec1 = HistoryRecord {
        format: HISTORY_FORMAT.into(),
        conversation_id: conv_id.into(),
        record_id: rec1_id.clone(),
        created_at_ms: 1000,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Elide,
        estimate_before: 1000,
        estimate_after: 200,
        replacements: vec![RangeReplacement {
            range: 2..3,
            originals: vec![b2.clone()],
            inserted_ids: vec![e1.id()],
        }],
    };

    // Record 2 (Summarize - nested): Summarizes B0, B1, E1, B3 -> replaced by S1
    let rec2_id = uuid::Uuid::now_v7().to_string();
    let s1 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::System, "Brief 1");
    let rec2 = HistoryRecord {
        format: HISTORY_FORMAT.into(),
        conversation_id: conv_id.into(),
        record_id: rec2_id.clone(),
        created_at_ms: 2000,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 800,
        estimate_after: 150,
        replacements: vec![RangeReplacement {
            range: 0..4,
            originals: vec![b0.clone(), b1.clone(), e1.clone(), b3.clone()],
            inserted_ids: vec![s1.id()],
        }],
    };

    // Record 3 (Summarize): Summarizes S1, B4, B5 -> replaced by S2
    let rec3_id = uuid::Uuid::now_v7().to_string();
    let s2 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::System, "Brief 2");
    let rec3 = HistoryRecord {
        format: HISTORY_FORMAT.into(),
        conversation_id: conv_id.into(),
        record_id: rec3_id.clone(),
        created_at_ms: 3000,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 900,
        estimate_after: 180,
        replacements: vec![RangeReplacement {
            range: 0..3,
            originals: vec![s1.clone(), b4.clone(), b5.clone()],
            inserted_ids: vec![s2.id()],
        }],
    };

    // Record 4 (Merge): Merges S2 into M1
    let rec4_id = uuid::Uuid::now_v7().to_string();
    let m1 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::System,
        "Merged Brief M1",
    );
    let rec4 = HistoryRecord {
        format: HISTORY_FORMAT.into(),
        conversation_id: conv_id.into(),
        record_id: rec4_id.clone(),
        created_at_ms: 4000,
        trigger: HistoryTrigger::Manual,
        kind: RewriteKind::Merge,
        estimate_before: 180,
        estimate_after: 100,
        replacements: vec![RangeReplacement {
            range: 0..1,
            originals: vec![s2.clone()],
            inserted_ids: vec![m1.id()],
        }],
    };

    // Store records in HistoryStore
    let index = vec![
        rec1.index_entry(),
        rec2.index_entry(),
        rec3.index_entry(),
        rec4.index_entry(),
    ];
    let store = HistoryStore::new(&session_dir, conv_id, index.clone());
    store.write(&rec1).unwrap();
    store.write(&rec2).unwrap();
    store.write(&rec3).unwrap();
    store.write(&rec4).unwrap();

    // Context currently holds [M1, B6, B7]
    let current_context_blocks = vec![m1.clone(), b6.clone(), b7.clone()];

    // 3. Opening reads 0 record files (check via test hook read_count)
    assert_eq!(store.read_count(), 0);

    // 4. Expand conversation fully
    let expanded = expand_conversation(&current_context_blocks, &index, None, |id| store.read(id));
    assert!(expanded.unreadable_records.is_empty());
    assert_eq!(expanded.blocks.len(), 8);

    // Block for block equality with unrewritten conversation:
    for (i, orig) in original_blocks.iter().enumerate() {
        assert_eq!(
            expanded.blocks[i].block.id(),
            orig.id(),
            "block {i} ID mismatch"
        );
        assert_eq!(
            &expanded.blocks[i].block, orig,
            "block {i} content mismatch"
        );
    }

    // Verify block statuses:
    assert_eq!(
        expanded.blocks[0].status,
        BlockStatus::Summarized(rec2_id.clone())
    );
    assert_eq!(
        expanded.blocks[1].status,
        BlockStatus::Summarized(rec2_id.clone())
    );
    assert_eq!(
        expanded.blocks[2].status,
        BlockStatus::Elided(rec1_id.clone())
    );
    assert_eq!(
        expanded.blocks[3].status,
        BlockStatus::Summarized(rec2_id.clone())
    );
    assert_eq!(
        expanded.blocks[4].status,
        BlockStatus::Summarized(rec3_id.clone())
    );
    assert_eq!(
        expanded.blocks[5].status,
        BlockStatus::Summarized(rec3_id.clone())
    );
    assert_eq!(expanded.blocks[6].status, BlockStatus::Original);
    assert_eq!(expanded.blocks[7].status, BlockStatus::Original);

    // 5. Test lazy on-demand reading:
    // Requesting last 2 blocks (B6, B7) needs 0 record reads!
    let reads_before = store.read_count();
    let last_2 = expand_conversation(&current_context_blocks, &index, Some(2), |id| {
        store.read(id)
    });
    assert_eq!(last_2.blocks.len(), 2);
    assert_eq!(last_2.blocks[0].block.id(), b6.id());
    assert_eq!(last_2.blocks[1].block.id(), b7.id());
    assert_eq!(
        store.read_count(),
        reads_before,
        "lazy expansion of original blocks reads 0 records"
    );

    // 6. Test unreadable record:
    // Corrupt record 1, expand again: reports unreadable record without failing the rest
    let rec1_file = session_dir
        .join(format!("{conv_id}.history"))
        .join(format!("{rec1_id}.json"));
    std::fs::write(&rec1_file, b"corrupted content").unwrap();
    let corrupted_expansion =
        expand_conversation(&current_context_blocks, &index, None, |id| store.read(id));
    assert_eq!(corrupted_expansion.unreadable_records.len(), 1);
    assert_eq!(corrupted_expansion.unreadable_records[0].record_id, rec1_id);
    // The elision stub stands in for its unreadable originals; the rest expands.
    let shown = corrupted_expansion
        .blocks
        .iter()
        .map(|expanded| (expanded.block.id(), expanded.status.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        shown,
        vec![
            (b0.id(), BlockStatus::Summarized(rec2_id.clone())),
            (b1.id(), BlockStatus::Summarized(rec2_id.clone())),
            (e1.id(), BlockStatus::Unreadable(rec1_id.clone())),
            (b3.id(), BlockStatus::Summarized(rec2_id.clone())),
            (b4.id(), BlockStatus::Summarized(rec3_id.clone())),
            (b5.id(), BlockStatus::Summarized(rec3_id.clone())),
            (b6.id(), BlockStatus::Original),
            (b7.id(), BlockStatus::Original),
        ]
    );

    // The newest three blocks need the merge and the outer summary only.
    let reads_before = store.read_count();
    let last_3 = expand_conversation(&current_context_blocks, &index, Some(3), |id| {
        store.read(id)
    });
    let ids = last_3
        .blocks
        .iter()
        .map(|b| b.block.id())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec![b5.id(), b6.id(), b7.id()]);
    assert_eq!(store.read_count() - reads_before, 2);
}

#[test]
fn expansion_puts_back_each_range_of_a_multi_range_record_once() {
    use super::expansion::{BlockStatus, expand_conversation};
    let directory = tempfile::tempdir().unwrap();
    let conversation = "conversation-multi";
    let text =
        |role, body: &str| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), role, body);
    let originals = vec![
        text(Role::User, "a"),
        text(Role::Assistant, "b"),
        text(Role::User, "c"),
        text(Role::Assistant, "d"),
        text(Role::User, "e"),
        text(Role::Assistant, "f"),
    ];
    let x = text(Role::User, "x");
    let z = text(Role::User, "z");
    let y1 = text(Role::User, "y1");
    let y2 = text(Role::User, "y2");
    let store = HistoryStore::new(directory.path(), conversation, Vec::new());
    let mut context = Context::from_blocks(originals.clone()).unwrap();
    // Two adjacent ranges (0..1, 1..2) and one after a kept block (3..5).
    let entry = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(vec![
                super::history::PlannedReplacement {
                    range: 0..1,
                    with: vec![x.clone()],
                },
                super::history::PlannedReplacement {
                    range: 1..2,
                    with: vec![z.clone()],
                },
                super::history::PlannedReplacement {
                    range: 3..5,
                    with: vec![y1.clone(), y2.clone()],
                },
            ]),
        )
        .unwrap();
    let current = context.blocks().to_vec();
    let ids = current.iter().map(ContextBlock::id).collect::<Vec<_>>();
    assert_eq!(
        ids,
        vec![
            x.id(),
            z.id(),
            originals[2].id(),
            y1.id(),
            y2.id(),
            originals[5].id()
        ]
    );

    let index = store.entries();
    let expanded = expand_conversation(&current, &index, None, |id| store.read(id));
    assert!(expanded.unreadable_records.is_empty());
    let blocks = expanded
        .blocks
        .iter()
        .map(|b| b.block.clone())
        .collect::<Vec<_>>();
    assert_eq!(blocks, originals);
    let summarized = BlockStatus::Summarized(entry.record_id.clone());
    let statuses = expanded
        .blocks
        .iter()
        .map(|b| b.status.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![
            summarized.clone(),
            summarized.clone(),
            BlockStatus::Original,
            summarized.clone(),
            summarized,
            BlockStatus::Original,
        ]
    );
    assert_eq!(store.read_count(), 1, "one record is read once");

    // The newest block is untouched material: no record read.
    let newest = expand_conversation(&current, &index, Some(1), |id| store.read(id));
    assert_eq!(newest.blocks.len(), 1);
    assert_eq!(newest.blocks[0].block, originals[5]);
    assert_eq!(store.read_count(), 1);
}

/// Whole-Context compactions in a row, built through the shared commit path.
///
/// Returns the current Context and, per record, the blocks it removed that no
/// other record inserted (the visible conversation of that span).
fn whole_context_chain(
    store: &HistoryStore,
    records: usize,
) -> (Context, Vec<ContextBlock>, Vec<String>) {
    let text =
        |body: String| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, body);
    let first = text("span 0".into());
    let mut conversation = vec![first.clone()];
    let mut context = Context::from_blocks(vec![first]).unwrap();
    let mut ids = Vec::new();
    for position in 0..records {
        let brief = super::summary::summary_block(
            super::summary::new_block_id(),
            &format!("brief {position}"),
        );
        let range = 0..context.blocks().len();
        let entry = store
            .commit_rewrite(
                &mut context,
                summarize_rewrite(vec![super::history::PlannedReplacement {
                    range,
                    with: vec![brief],
                }]),
            )
            .unwrap();
        ids.push(entry.record_id);
        let next = text(format!("span {}", position + 1));
        conversation.push(next.clone());
        context.edit().append([next]).commit().unwrap();
    }
    (context, conversation, ids)
}

#[test]
fn cursor_continues_past_an_unreadable_record_with_the_older_ones() {
    use super::expansion::{BlockStatus, ExpansionCursor, ExpansionStep};
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation-chain", Vec::new());
    let (context, conversation, ids) = whole_context_chain(&store, 4);

    // Intact: the expansion is the conversation with no rewrites, and every
    // record gives one divider mark before its originals.
    let mut cursor = ExpansionCursor::new(context.blocks(), store.entries());
    let mut blocks = Vec::new();
    let mut marks = Vec::new();
    loop {
        match cursor.next() {
            ExpansionStep::Block(block) => blocks.push(block.block),
            ExpansionStep::Rewrite(mark) => marks.push(mark.record_id),
            ExpansionStep::NeedRecord(id) => {
                let record = store.read(&id).map_err(|error| error.to_string());
                cursor.supply(&id, record);
            }
            ExpansionStep::Unreadable(unreadable) => panic!("{unreadable:?}"),
            ExpansionStep::End => break,
        }
    }
    blocks.reverse();
    assert_eq!(blocks, conversation);
    assert_eq!(marks, ids.iter().rev().cloned().collect::<Vec<_>>());
    assert_eq!(cursor.unread(), 0);

    // Record 2 is missing: its brief stands in, then records 1 and 0 are
    // still expanded by their place in the index.
    std::fs::remove_file(
        directory
            .path()
            .join("conversation-chain.history")
            .join(format!("{}.json", ids[2])),
    )
    .unwrap();
    let expanded =
        super::expansion::expand_conversation(context.blocks(), &store.entries(), None, |id| {
            store.read(id)
        });
    assert_eq!(expanded.unreadable_records.len(), 1);
    assert_eq!(expanded.unreadable_records[0].record_id, ids[2]);
    let shown = expanded
        .blocks
        .iter()
        .map(|block| (block.block.id(), block.status.clone()))
        .collect::<Vec<_>>();
    let brief_2 = store.entries()[2].inserted_block_ids[0];
    assert_eq!(
        shown,
        vec![
            (
                conversation[0].id(),
                BlockStatus::Summarized(ids[0].clone())
            ),
            (
                conversation[1].id(),
                BlockStatus::Summarized(ids[1].clone())
            ),
            (brief_2, BlockStatus::Unreadable(ids[2].clone())),
            (
                conversation[3].id(),
                BlockStatus::Summarized(ids[3].clone())
            ),
            (conversation[4].id(), BlockStatus::Original),
        ]
    );
}

#[test]
fn cursor_adopts_a_rewrite_and_continues_with_the_older_records() {
    use super::expansion::{ExpansionCursor, ExpansionStep};
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation-adopt", Vec::new());
    let (_, _, ids) = whole_context_chain(&store, 2);
    let mut entries = store.entries();
    let newest = entries.pop().unwrap();
    // A display opened before the newest rewrite: its prefix is the older brief.
    let prefix = store.read(&ids[1]).unwrap().replacements[0].originals[..1].to_vec();
    let mut cursor = ExpansionCursor::new(&prefix, entries);
    assert_eq!(cursor.unread(), 1);
    cursor.adopt(newest);
    assert_eq!(cursor.unread(), 1, "an adopted record needs no read");
    assert_eq!(cursor.next(), ExpansionStep::NeedRecord(ids[0].clone()));
}
