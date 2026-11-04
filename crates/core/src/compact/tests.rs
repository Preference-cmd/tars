//! Compaction unit and low-level integration tests.

use super::estimate::{UsageTracker, UsageTrackingGateway, context_bytes, estimate_bytes};
use super::history::{
    HISTORY_FORMAT, HistoryRecord, HistoryStore, RangeReplacement, RewriteKind, validate_index,
};
use super::preparer::CompactionPreparer;
use super::segment;
use super::summary;
use super::{CompactionRuntime, HistoryRecordEntry, HistoryTrigger};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::session::{Session, SessionConfig, SubmitRequest, ToolReporter, WorkRef, WorkState};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, CallControl, CancellationToken, ContentPart, Context,
    ContextBlock, ModelGateway, ModelInvokeError, ModelOutput, ModelRequest, ModelResponse,
    ModelStopReason, Role, Tool, ToolCallContext, ToolCallPayload, ToolDefinition, ToolOutput,
    ToolResultPayload, ToolResultStatus, TurnId,
};
use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnLimits, TurnRunOptions, TurnRunner};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

mod partial;

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
        .append([tool_change("alpha"), big_block(3_000)])
        .commit()
        .unwrap();
    let (mut session, runtime) = build_session(
        directory.path(),
        definition(Some(1_000), None),
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
    assert_eq!(blocks.len(), 3, "{blocks:?}");
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
async fn a_later_round_keeps_the_submitted_request_as_its_own_block_after_the_brief() {
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
    let request = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, request_text);
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "in-turn".into(),
            blocks: vec![request.clone()],
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
    // Round 1 compacts the large tool result.
    assert_eq!(summary_calls.load(Ordering::SeqCst), 1);
    let inputs = summary_inputs.lock().unwrap().clone();
    let last = inputs.last().expect("one summary request");
    // The brief request is the normal frame up to the cut point: the
    // declaration, then the request, then the round's tool material.
    assert!(matches!(last[0].content(), BlockContent::ToolChange(_)));
    assert_eq!(last[1], request, "the request stands where it stood");
    let text = message_text(last.last().expect("instruction block"));
    assert!(text.contains("request still in progress"), "{text}");
    assert!(!text.contains(request_text), "{text}");
    // The request stays as its own block, with its own id, right after the
    // brief; the brief does not copy it.
    let blocks = session
        .with_context(|context| context.blocks().to_vec())
        .unwrap();
    let brief = blocks
        .iter()
        .position(|block| block.meta().source.as_deref() == Some(summary::SUMMARY_SOURCE))
        .expect("brief block");
    assert_eq!(blocks[brief + 1], request);
    let summary_text = message_text(&blocks[brief]);
    assert!(
        summary_text.contains("Goal: continue with the fixture tool."),
        "{summary_text}"
    );
    assert!(!summary_text.contains(request_text), "{summary_text}");
    let last_line = summary_text.lines().last().unwrap();
    assert!(
        last_line.starts_with("Original messages: recall id="),
        "{summary_text}"
    );
    session.shutdown().await;
}

/// The text of a message block.
fn message_text(block: &ContextBlock) -> String {
    let BlockContent::Message { parts, .. } = block.content() else {
        panic!("not a message: {block:?}")
    };
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(payload) => Some(payload.0.as_str()),
            _ => None,
        })
        .collect()
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
    // Trimming restarts only at a cut point, so a result never loses its call.
    let trimmed = summary::request_material(&blocks, 0, blocks.len(), &[], Some(10));
    assert!(
        trimmed
            .iter()
            .all(|block| !matches!(block.content(), BlockContent::ToolResult(_))),
        "an orphan result is not sent: {trimmed:?}"
    );
    let last_fits = segment::blocks_estimate(&blocks[3..]);
    let trimmed = summary::request_material(&blocks, 0, blocks.len(), &[], Some(last_fits));
    assert_eq!(trimmed.len(), 1, "restarts at the last user message");
    assert_eq!(trimmed[0].id(), blocks[3].id());
    // The brief marks its provenance; a tool declaration is restated only
    // when the summarized part changed the tool state.
    let brief = summary::summary_block(BlockId::new(uuid::Uuid::now_v7()), "brief");
    assert_eq!(
        brief.meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    assert!(segment::tool_delta(&[], &blocks).is_none());
    let with_tools = vec![tool_change("alpha")];
    let delta = segment::tool_delta(&[], &with_tools).expect("full declaration");
    assert_eq!(causa::kernel::current_tools(&[delta])[0].name, "alpha");
    assert!(segment::tool_delta(&with_tools, &with_tools).is_none());
}

#[test]
fn trimmed_summary_input_starts_with_the_tool_declaration_and_a_user_message() {
    let assistant =
        |text: &str| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::Assistant, text);
    let user =
        |text: &str| ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, text);
    let alpha = tool_change("alpha");
    let blocks = vec![
        big_block(900),
        alpha.clone(),
        assistant("first answer"),
        user("second question"),
        assistant("second answer"),
    ];
    // The cut falls after the large first message, which also held the
    // declaration; the dropped declaration is restated first.
    let trimmed = summary::request_material(&blocks, 0, blocks.len(), &[], Some(200));
    assert_eq!(trimmed.len(), 3, "{trimmed:?}");
    let BlockContent::ToolChange(_) = trimmed[0].content() else {
        panic!("the dropped declaration is restated first")
    };
    assert_ne!(trimmed[0].id(), alpha.id());
    assert_eq!(causa::kernel::current_tools(&trimmed)[0].name, "alpha");
    let BlockContent::Message { role, .. } = trimmed[1].content() else {
        panic!("a message follows the declaration")
    };
    assert_eq!(role, &Role::User);
    assert_eq!(trimmed[1].id(), blocks[3].id());
    // The head is never trimmed: a declaration in the head stays as it is.
    let head = vec![
        tool_change("alpha"),
        big_block(900),
        assistant("first answer"),
        user("second question"),
        assistant("second answer"),
    ];
    let head_end = segment::head_end(&head);
    assert_eq!(head_end, 1);
    let trimmed = summary::request_material(&head, head_end, head.len(), &[], Some(200));
    assert_eq!(
        trimmed.iter().map(ContextBlock::id).collect::<Vec<_>>(),
        vec![head[0].id(), head[3].id(), head[4].id()]
    );
    // Nothing is restated when the input already fits.
    assert_eq!(
        summary::request_material(&blocks, 0, blocks.len(), &[], Some(10_000)),
        blocks
    );
    assert_eq!(
        summary::request_material(&blocks, 0, blocks.len(), &[], None),
        blocks
    );
}

#[test]
fn instruction_is_structured_and_marks_the_request_in_progress() {
    let text = summary::instruction(false);
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
    assert!(text.starts_with("Prepare a continuation brief"));
    assert!(text.contains("Earlier briefs above are background"));
    assert!(text.contains("The most recent messages are kept verbatim after the brief."));
    assert!(!text.contains("In progress"));
    let with_request = summary::instruction(true);
    assert!(with_request.contains("section \"In progress\""));
    assert!(with_request.contains("request still in progress"));
    assert!(with_request.contains("do not copy it"));
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

#[test]
fn test_be08_summary_prefix_names_recall_and_omits_only_record() {
    assert!(!summary::SUMMARY_PREFIX.contains("only record"));
    assert!(summary::SUMMARY_PREFIX.contains("recall"));
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
        record_id: super::history::new_record_id(),
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

    // 1. Structure: when no tail fits the budget, the summary replaces
    // everything after the head. A declaration in the head stays as it is,
    // so no new one is needed; the result is the head plus one brief.
    let original_blocks = vec![
        tool_change("fixture_tool"),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::User,
            "User prompt 1 ".repeat(100),
        ),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::Assistant,
            "Assistant reply 1",
        ),
    ];
    let mut whole = Context::from_blocks(original_blocks.clone()).unwrap();
    let control = CallControl::new(causa::kernel::CancellationToken::new(), None);
    let rewrite = runtime
        .plan(
            &whole,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
            &control,
        )
        .await
        .unwrap();
    assert_eq!(rewrite.replacements.len(), 1);
    assert_eq!(rewrite.replacements[0].range, 1..3);
    assert_eq!(rewrite.replacements[0].with.len(), 1);
    let summary_id = rewrite.replacements[0].with[0].id();
    runtime.commit_rewrite(&mut whole, rewrite).unwrap();
    let replaced = whole.blocks();
    assert_eq!(replaced.len(), 2);
    assert_eq!(replaced[0], original_blocks[0], "the head keeps its block");
    assert_eq!(
        causa::kernel::current_tools(replaced)[0].name,
        "fixture_tool"
    );
    assert_eq!(
        replaced[1].meta().source.as_deref(),
        Some(summary::SUMMARY_SOURCE)
    );
    assert_eq!(replaced[1].id(), summary_id);

    // 3. Abandon conditions:
    // a. Unpaired materials abandon
    let unpaired_context = Context::from_blocks(vec![ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new("tool", serde_json::json!({}))),
        Default::default(),
    )])
    .unwrap();
    let plan_res = runtime
        .plan(
            &unpaired_context,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
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
            &control,
        )
        .await;
    assert_eq!(
        empty_res.unwrap_err(),
        super::CompactionAbandon::EmptySummary
    );
}

#[tokio::test]
async fn test_be06_planned_summary_commits_under_the_record_id_allocated_before_the_brief() {
    struct BriefGateway;
    #[async_trait::async_trait]
    impl ModelGateway for BriefGateway {
        async fn invoke(
            &self,
            _: &ModelRequest,
            _: &CallControl,
        ) -> Result<ModelOutput, ModelInvokeError> {
            Ok(summary_output("brief of the earlier work"))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = CompactionRuntime::new(
        &definition(Some(1_000), None),
        &"conversation-record-id".into(),
        Some(directory.path()),
        Vec::new(),
        None,
        None,
        Arc::new(BriefGateway),
        Arc::new(UsageTracker::default()),
    )
    .unwrap();
    let mut context = Context::from_blocks(vec![big_block(2_000)]).unwrap();
    let control = CallControl::new(causa::kernel::CancellationToken::new(), None);
    let rewrite = runtime
        .plan(
            &context,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
            &control,
        )
        .await
        .unwrap();
    let allocated = rewrite.record_id.clone();
    assert!(super::history::is_canonical_uuid(&allocated));
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    assert_eq!(entry.record_id, allocated);
    let record = runtime.history().unwrap().read(&allocated).unwrap();
    assert_eq!(record.record_id, allocated);
    assert_eq!(record.index_entry(), entry);
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

#[test]
fn test_be06_block_to_text_all_kinds() {
    use super::block_text::block_to_text;
    use causa::kernel::{
        BlockContent, BlockId, BlockMeta, ContentPart, ContextBlock, MediaRef, Role, SealedPart,
        TextPayload, ToolCallPayload, ToolChange, ToolDefinition, ToolOutput, ToolResultPayload,
        ToolResultStatus,
    };

    // 1. Message with text, media (with name and without name), and sealed
    let msg_block = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![
                ContentPart::Text(TextPayload::new("First line of message")),
                ContentPart::Media(
                    MediaRef::new("image/png", "sha256:abcd").with_name("photo.png"),
                ),
                ContentPart::Media(MediaRef::new("audio/mp3", "sha256:ef01")),
                ContentPart::Sealed(SealedPart::new(
                    "openai",
                    serde_json::json!({"reasoning": "secret"}),
                )),
                ContentPart::Text(TextPayload::new("Second text part")),
            ],
        },
        BlockMeta::default(),
    );
    let rendered_msg = block_to_text(&msg_block);
    assert_eq!(
        rendered_msg,
        "First line of message\n[media image/png photo.png]\n[media audio/mp3 sha256:ef01]\nSecond text part"
    );

    // 2. ToolCall
    let call_block = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(
            "test_tool",
            serde_json::json!({
                "path": "file.txt",
                "count": 42
            }),
        )),
        BlockMeta::default(),
    );
    let rendered_call = block_to_text(&call_block);
    let expected_args = serde_json::to_string_pretty(&serde_json::json!({
        "path": "file.txt",
        "count": 42
    }))
    .unwrap();
    assert_eq!(rendered_call, format!("call test_tool\n{expected_args}"));

    // 3. ToolResult: string content as-is
    let res_str_block = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_block.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::Value::String(
                "raw output text\nsecond line".into(),
            )),
        )),
        BlockMeta::default(),
    );
    assert_eq!(
        block_to_text(&res_str_block),
        "raw output text\nsecond line"
    );

    // ToolResult: non-string pretty JSON with media
    let res_json_block = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(
            ToolResultPayload::new(
                call_block.id(),
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({"status": "ok", "items": [1, 2]})),
            )
            .with_media(vec![
                MediaRef::new("image/jpeg", "ref-1234"),
                MediaRef::new("image/png", "ref-5678").with_name("chart.png"),
            ]),
        ),
        BlockMeta::default(),
    );
    let expected_json =
        serde_json::to_string_pretty(&serde_json::json!({"status": "ok", "items": [1, 2]}))
            .unwrap();
    assert_eq!(
        block_to_text(&res_json_block),
        format!("{expected_json}\n[media image/jpeg ref-1234]\n[media image/png chart.png]")
    );

    // 4. ToolChange
    let change_block = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolChange(ToolChange::new(
            vec![
                ToolDefinition::new("tool_a", "desc a", serde_json::json!({})),
                ToolDefinition::new("tool_b", "desc b", serde_json::json!({})),
            ],
            vec!["tool_c".into()],
        )),
        BlockMeta::default(),
    );
    let rendered_change = block_to_text(&change_block);
    assert!(rendered_change.contains("tool_a"));
    assert!(rendered_change.contains("tool_b"));
    assert!(rendered_change.contains("tool_c"));
    assert!(
        !rendered_change.contains('\n'),
        "ToolChange description must be a single line"
    );
}

#[tokio::test]
async fn test_be06_history_artifacts_read_range_and_errors() {
    use super::HistoryTrigger;
    use super::artifacts::HistoryArtifacts;
    use super::history::{HistoryStore, PlannedReplacement, Rewrite, RewriteKind};
    use causa::kernel::{
        ArtifactHint, ArtifactKind, ArtifactStore, BlockContent, BlockId, BlockMeta, ContentPart,
        Context, ContextBlock, MediaRef, Role, StoreError, TextPayload, ToolCallPayload,
        ToolOutput, ToolResultPayload, ToolResultStatus,
    };

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(HistoryStore::new(
        dir.path(),
        "conversation-be06",
        Vec::new(),
    ));
    let artifacts = HistoryArtifacts::new(store.clone());

    let b0 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::User,
            parts: vec![
                ContentPart::Text(TextPayload::new("user prompt")),
                ContentPart::Media(MediaRef::new("image/png", "img-1").with_name("spec.png")),
            ],
        },
        BlockMeta::default(),
    );
    let b1 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(
            "search",
            serde_json::json!({"q": "rust"}),
        )),
        BlockMeta::default(),
    );
    let b2 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            b1.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::Value::String("string search output".into())),
        )),
        BlockMeta::default(),
    );
    let b3 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(
            ToolResultPayload::new(
                b1.id(),
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({"found": 3})),
            )
            .with_media(vec![MediaRef::new("application/pdf", "report.pdf")]),
        ),
        BlockMeta::default(),
    );

    let mut context = Context::new();
    let edit = context
        .edit()
        .append(vec![b0.clone(), b1.clone(), b2.clone(), b3.clone()]);
    edit.commit().unwrap();

    let brief = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "compact brief",
    );
    let record_id = super::history::new_record_id();
    let rewrite = Rewrite {
        record_id: record_id.clone(),
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 1000,
        estimate_after: 200,
        replacements: vec![PlannedReplacement {
            range: 0..4,
            with: vec![brief.clone()],
        }],
    };

    store.commit_rewrite(&mut context, rewrite).unwrap();

    // Verify reading with range = None returns exact block-to-text bytes for each original
    for block in &[&b0, &b1, &b2, &b3] {
        let expected_text = super::block_to_text(block);
        let id = format!("{record_id}/{}", block.id().0);
        let bytes = artifacts
            .read(&id, None)
            .await
            .expect("read should succeed");
        assert_eq!(String::from_utf8(bytes).unwrap(), expected_text);
    }

    // Range testing on b2
    let b2_text = super::block_to_text(&b2);
    let b2_bytes = b2_text.as_bytes();
    let b2_id = format!("{record_id}/{}", b2.id().0);
    let len = b2_bytes.len() as u64;

    // Exact subslice
    let slice = artifacts.read(&b2_id, Some(2..7)).await.unwrap();
    assert_eq!(slice, &b2_bytes[2..7]);

    // Clamped end: end past length is clamped
    let clamped = artifacts.read(&b2_id, Some(3..(len + 50))).await.unwrap();
    assert_eq!(clamped, &b2_bytes[3..]);

    // start > len fails with StoreError::Read
    let err_start_gt_len = artifacts
        .read(&b2_id, Some((len + 1)..(len + 10)))
        .await
        .unwrap_err();
    assert!(matches!(err_start_gt_len, StoreError::Read(_)));

    // start > end fails with StoreError::Read
    let err_start_gt_end = artifacts
        .read(&b2_id, Some(std::ops::Range { start: 10, end: 5 }))
        .await
        .unwrap_err();
    assert!(matches!(err_start_gt_end, StoreError::Read(_)));

    // Errors:
    // 1. Malformed id
    let err_malformed = artifacts.read("not-a-valid-id", None).await.unwrap_err();
    assert!(matches!(err_malformed, StoreError::Read(_)));

    // 2. Record id not in index while a file with that name exists in history directory
    let stray_id = uuid::Uuid::now_v7().to_string();
    let stray_path = dir
        .path()
        .join("conversation-be06.history")
        .join(format!("{stray_id}.json"));
    std::fs::write(&stray_path, b"{}").unwrap();
    let err_not_in_index = artifacts
        .read(&format!("{stray_id}/{}", b0.id().0), None)
        .await
        .unwrap_err();
    assert!(matches!(err_not_in_index, StoreError::Read(_)));

    // 3. Block id not in record (e.g. brief id or random UUID)
    let err_missing_block = artifacts
        .read(&format!("{record_id}/{}", brief.id().0), None)
        .await
        .unwrap_err();
    assert!(matches!(err_missing_block, StoreError::Read(_)));
    let random_block_id = uuid::Uuid::now_v7();
    let err_random_block = artifacts
        .read(&format!("{record_id}/{random_block_id}"), None)
        .await
        .unwrap_err();
    assert!(matches!(err_random_block, StoreError::Read(_)));

    // 4. Record in index but file deleted
    let record_file = dir
        .path()
        .join("conversation-be06.history")
        .join(format!("{record_id}.json"));
    std::fs::remove_file(&record_file).unwrap();
    let err_file_deleted = artifacts
        .read(&format!("{record_id}/{}", b0.id().0), None)
        .await
        .unwrap_err();
    assert!(matches!(err_file_deleted, StoreError::Read(_)));

    // 5. persist always fails with StoreError::Persist
    let hint = ArtifactHint::new("tool", b1.id(), ArtifactKind::FullOutput);
    let err_persist = artifacts.persist(b"data", hint).await.unwrap_err();
    assert!(matches!(err_persist, StoreError::Persist(_)));
    assert!(
        err_persist
            .to_string()
            .contains("history artifacts are written only by context rewrites")
    );
}

#[test]
fn test_be06_record_id_allocation_and_commit_rewrite_validation() {
    use super::HistoryTrigger;
    use super::history::{HistoryStore, PlannedReplacement, Rewrite, RewriteError, RewriteKind};

    let dir = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(dir.path(), "conversation-rec-alloc", Vec::new());

    let b0 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "msg");
    let mut context = Context::new();
    context.edit().append(vec![b0.clone()]).commit().unwrap();

    let brief = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "brief");
    let allocated_id = super::history::new_record_id();
    let rewrite = Rewrite {
        record_id: allocated_id.clone(),
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 100,
        estimate_after: 50,
        replacements: vec![PlannedReplacement {
            range: 0..1,
            with: vec![brief.clone()],
        }],
    };

    let entry = store
        .commit_rewrite(&mut context, rewrite)
        .expect("commit should succeed");
    assert_eq!(entry.record_id, allocated_id);
    let written_file = dir
        .path()
        .join("conversation-rec-alloc.history")
        .join(format!("{allocated_id}.json"));
    assert!(written_file.exists());

    // Duplicate record id is rejected without leaving a new file
    let b1 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "another");
    let brief2 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "brief2");
    context.edit().append(vec![b1.clone()]).commit().unwrap();

    let dup_rewrite = Rewrite {
        record_id: allocated_id.clone(),
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 100,
        estimate_after: 50,
        replacements: vec![PlannedReplacement {
            range: 1..2,
            with: vec![brief2.clone()],
        }],
    };
    let err_dup = store.commit_rewrite(&mut context, dup_rewrite).unwrap_err();
    assert!(matches!(err_dup, RewriteError::Rejected(_)));

    // Non-canonical record id is rejected without leaving a file
    let bad_id = "NOT-A-CANONICAL-UUID";
    let bad_file = dir
        .path()
        .join("conversation-rec-alloc.history")
        .join(format!("{bad_id}.json"));
    let bad_rewrite = Rewrite {
        record_id: bad_id.to_string(),
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 100,
        estimate_after: 50,
        replacements: vec![PlannedReplacement {
            range: 1..2,
            with: vec![brief2.clone()],
        }],
    };
    let err_bad = store.commit_rewrite(&mut context, bad_rewrite).unwrap_err();
    assert!(matches!(err_bad, RewriteError::Rejected(_)));
    assert!(!bad_file.exists());
}

fn setup_recall_test(
    dir: &std::path::Path,
) -> (
    Arc<super::history::HistoryStore>,
    Arc<super::CompactionRuntime>,
    super::RecallTool,
) {
    let definition = definition(Some(100), None);
    let tracker = Arc::new(UsageTracker::default());
    let gateway: Arc<dyn ModelGateway> = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
        cache_read_tokens: None,
    });
    let runtime = super::CompactionRuntime::new(
        &definition,
        &"conversation-recall".into(),
        Some(dir),
        Vec::new(),
        None,
        None,
        gateway,
        tracker,
    )
    .unwrap();
    let history = runtime.history().unwrap();
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "conversation-recall".into(),
            turn_id,
        },
        tokio::sync::broadcast::channel(128).0,
    );
    let tool = super::RecallTool::new(runtime.clone(), reporter, 60);
    (history, runtime, tool)
}

fn recall_control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

fn recall_call(args: serde_json::Value) -> causa::kernel::ToolCallContext {
    causa::kernel::ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::ToolCallPayload::new(super::recall::TOOL_NAME, args),
    )
}

#[tokio::test]
async fn test_be07_recall_list_and_paging() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());

    let b0 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "first original message with some text",
    );
    let b1 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "second original response",
    );
    let mut context = Context::new();
    context
        .edit()
        .append(vec![b0.clone(), b1.clone()])
        .commit()
        .unwrap();

    let brief = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "brief replacing two blocks",
    );
    let record_id = super::history::new_record_id();
    let rewrite = super::history::Rewrite {
        record_id: record_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..2,
            with: vec![brief],
        }],
    };
    store.commit_rewrite(&mut context, rewrite).unwrap();

    let control = recall_control();

    // 1. Full list
    let call = recall_call(serde_json::json!({ "id": record_id }));
    let result = tool.execute(&call, &control, None).await;
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    let content = match &result.output.content {
        serde_json::Value::String(s) => s.as_str(),
        other => panic!("expected string output, got {other:?}"),
    };
    let lines: Vec<&str> = content.lines().collect();
    // Header line has record id, kind, trigger, UTC timestamp, number of blocks
    assert!(lines[0].contains(&record_id));
    assert!(lines[0].contains("summarize"));
    assert!(lines[0].contains("auto"));
    assert!(lines[0].contains("UTC"));
    assert!(lines[0].contains("2 blocks"));

    // Two block lines
    assert!(lines[1].starts_with("0 user/message "));
    assert!(lines[1].contains(&format!("{record_id}/{}", b0.id().0)));
    assert!(lines[1].contains("first original message with some text"));

    assert!(lines[2].starts_with("1 assistant/message "));
    assert!(lines[2].contains(&format!("{record_id}/{}", b1.id().0)));
    assert!(lines[2].contains("second original response"));
    assert_eq!(lines.len(), 3); // No more line when all fit

    // 2. Paged list: offset 0, limit 1
    let call_paged = recall_call(serde_json::json!({ "id": record_id, "offset": 0, "limit": 1 }));
    let result_paged = tool.execute(&call_paged, &control, None).await;
    assert_eq!(result_paged.status, ToolResultStatus::Succeeded);
    let paged_content = match &result_paged.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let paged_lines: Vec<&str> = paged_content.lines().collect();
    assert_eq!(paged_lines.len(), 3);
    assert!(paged_lines[1].starts_with("0 user"));
    assert_eq!(
        paged_lines[2],
        format!("more: recall id={record_id} offset=1")
    );

    // 3. Paged list: offset 1, limit 1
    let call_paged2 = recall_call(serde_json::json!({ "id": record_id, "offset": 1, "limit": 1 }));
    let result_paged2 = tool.execute(&call_paged2, &control, None).await;
    let paged_content2 = match &result_paged2.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let paged_lines2: Vec<&str> = paged_content2.lines().collect();
    assert_eq!(paged_lines2.len(), 2);
    assert!(paged_lines2[1].starts_with("1 assistant"));
}

#[tokio::test]
async fn test_be07_recall_read_and_paging_cjk() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());

    let cjk_text = "你好世界！这是TARS上下文压缩测试。Hello, World!";
    let b0 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, cjk_text);
    let mut context = Context::new();
    context.edit().append(vec![b0.clone()]).commit().unwrap();

    let record_id = super::history::new_record_id();
    let brief = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "brief");
    let rewrite = super::history::Rewrite {
        record_id: record_id.clone(),
        trigger: super::HistoryTrigger::Manual,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..1,
            with: vec![brief],
        }],
    };
    store.commit_rewrite(&mut context, rewrite).unwrap();

    let control = recall_control();
    let block_ref = format!("{record_id}/{}", b0.id().0);

    // 1. Full read
    let call = recall_call(serde_json::json!({ "id": block_ref }));
    let result = tool.execute(&call, &control, None).await;
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    let content = match &result.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let lines: Vec<&str> = content.lines().collect();
    assert!(lines[0].contains(&block_ref));
    assert!(lines[0].contains(" user/message "));
    assert!(lines[0].contains(&format!("{} chars", cjk_text.chars().count())));
    assert_eq!(lines[1], cjk_text);

    // 2. Character paging with CJK
    let call_paged = recall_call(serde_json::json!({
        "id": block_ref,
        "offset": 0,
        "limit": 5,
    }));
    let result_paged = tool.execute(&call_paged, &control, None).await;
    let paged_content = match &result_paged.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let paged_lines: Vec<&str> = paged_content.lines().collect();
    assert_eq!(paged_lines[1], "你好世界！");
    assert_eq!(
        paged_lines[2],
        format!("more: recall id={block_ref} offset=5")
    );

    // 3. Offset past the end
    let call_past = recall_call(serde_json::json!({
        "id": block_ref,
        "offset": 1000,
    }));
    let result_past = tool.execute(&call_past, &control, None).await;
    let past_content = match &result_past.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let past_lines: Vec<&str> = past_content.lines().collect();
    assert_eq!(past_lines.len(), 1); // Header only, empty text slice, no more line
}

#[tokio::test]
async fn test_be07_recall_search() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());

    // Record 1 (older)
    let b_res = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            BlockId::new(uuid::Uuid::now_v7()),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("Alpha query result data")),
        )),
        BlockMeta::default(),
    );
    let b_change = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolChange(causa::kernel::ToolChange::between(&[], &[])),
        BlockMeta::default(),
    );
    let mut context = Context::new();
    context
        .edit()
        .append(vec![b_res.clone(), b_change.clone()])
        .commit()
        .unwrap();

    let r1_id = super::history::new_record_id();
    let r1_rewrite = super::history::Rewrite {
        record_id: r1_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..2,
            with: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "b1",
            )],
        }],
    };
    store.commit_rewrite(&mut context, r1_rewrite).unwrap();

    // Record 2 (newer)
    let b_user = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "User says Alpha test here\nsecond line of Alpha",
    );
    let b_call = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "test_tool",
            serde_json::json!({"cmd": "alpha"}),
        )),
        BlockMeta::default(),
    );
    context
        .edit()
        .append(vec![b_user.clone(), b_call.clone()])
        .commit()
        .unwrap();

    let r2_id = super::history::new_record_id();
    let r2_rewrite = super::history::Rewrite {
        record_id: r2_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 1..3,
            with: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "b2",
            )],
        }],
    };
    store.commit_rewrite(&mut context, r2_rewrite).unwrap();

    let control = recall_control();

    // 1. Default case-insensitive search: newest record first, inside record last block first
    // Expected order of hits matching "alpha":
    // R2: b_call (tool_call), then b_user (message)
    // R1: b_res (tool_result)
    let call = recall_call(serde_json::json!({ "query": "alpha" }));
    let result = tool.execute(&call, &control, None).await;
    assert_eq!(result.status, ToolResultStatus::Succeeded);
    let content = match &result.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let hits: Vec<&str> = content.split("\n\n").collect();
    assert_eq!(hits.len(), 3);
    assert!(hits[0].contains(&format!("{r2_id}/{}", b_call.id().0)));
    assert!(hits[0].contains(" assistant/tool_call"));
    assert!(hits[1].contains(&format!("{r2_id}/{}", b_user.id().0)));
    assert!(hits[1].contains(" user/message"));
    assert!(hits[1].contains("L1: User says Alpha test here"));
    assert!(hits[1].contains("L2: second line of Alpha"));
    assert!(hits[2].contains(&format!("{r1_id}/{}", b_res.id().0)));
    assert!(hits[2].contains(" tool/tool_result"));

    // 2. Case-sensitive search: "Alpha" matches b_user and b_res, but not b_call (lowercase cmd)
    let call_cs = recall_call(serde_json::json!({ "query": "Alpha", "case_sensitive": true }));
    let res_cs = tool.execute(&call_cs, &control, None).await;
    let content_cs = match &res_cs.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let hits_cs: Vec<&str> = content_cs.split("\n\n").collect();
    assert_eq!(hits_cs.len(), 2);
    assert!(hits_cs[0].contains(&format!("{r2_id}/{}", b_user.id().0)));
    assert!(hits_cs[1].contains(&format!("{r1_id}/{}", b_res.id().0)));

    // 3. Role filter: only "tool"
    let call_role = recall_call(serde_json::json!({ "query": "alpha", "role": "tool" }));
    let res_role = tool.execute(&call_role, &control, None).await;
    let content_role = match &res_role.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let hits_role: Vec<&str> = content_role.split("\n\n").collect();
    assert_eq!(hits_role.len(), 1);
    assert!(hits_role[0].contains(&format!("{r1_id}/{}", b_res.id().0)));

    // 4. Kind filter: only "tool_call"
    let call_kind = recall_call(serde_json::json!({ "query": "alpha", "kind": "tool_call" }));
    let res_kind = tool.execute(&call_kind, &control, None).await;
    let content_kind = match &res_kind.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    let hits_kind: Vec<&str> = content_kind.split("\n\n").collect();
    assert_eq!(hits_kind.len(), 1);
    assert!(hits_kind[0].contains(&format!("{r2_id}/{}", b_call.id().0)));

    // 5. Context option
    let call_ctx =
        recall_call(serde_json::json!({ "query": "Alpha", "role": "user", "context": 1 }));
    let res_ctx = tool.execute(&call_ctx, &control, None).await;
    let content_ctx = match &res_ctx.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_ctx.contains(&format!(
        "  after: {r2_id}/{} assistant/tool_call: ",
        b_call.id().0
    )));

    // 6. Limit and more hits note
    let call_lim = recall_call(serde_json::json!({ "query": "alpha", "limit": 1 }));
    let res_lim = tool.execute(&call_lim, &control, None).await;
    let content_lim = match &res_lim.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_lim.contains("more hits exist; suggest narrowing query"));

    // 7. No hit message
    let call_none = recall_call(serde_json::json!({ "query": "definitely_not_in_any_record" }));
    let res_none = tool.execute(&call_none, &control, None).await;
    let content_none = match &res_none.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert_eq!(content_none, "no hits found for query");

    // 8. Unreadable record listed at the end
    let r3_id = super::history::new_record_id();
    let r3_rewrite = super::history::Rewrite {
        record_id: r3_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..1,
            with: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "b3",
            )],
        }],
    };
    context
        .edit()
        .append(vec![ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::User,
            "to_delete",
        )])
        .commit()
        .unwrap();
    store.commit_rewrite(&mut context, r3_rewrite).unwrap();
    // Delete r3's file
    store.remove(&r3_id);

    let call_unreadable = recall_call(serde_json::json!({ "query": "alpha" }));
    let res_unreadable = tool.execute(&call_unreadable, &control, None).await;
    let content_unreadable = match &res_unreadable.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_unreadable.contains(&format!("unreadable: {r3_id}")));
    assert!(content_unreadable.contains(&format!("{r2_id}/{}", b_call.id().0)));
}

#[tokio::test]
async fn test_be07_recall_regex_search() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());
    let error_line = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "build output\nerror[E0308]: mismatched types\nwarning: unused import",
    );
    let other = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "Error codes like E0308 come from rustc",
    );
    let mut context = Context::new();
    context
        .edit()
        .append(vec![error_line.clone(), other.clone()])
        .commit()
        .unwrap();
    let record_id = super::history::new_record_id();
    store
        .commit_rewrite(
            &mut context,
            super::history::Rewrite {
                record_id: record_id.clone(),
                trigger: super::HistoryTrigger::Manual,
                kind: super::history::RewriteKind::Summarize,
                estimate_before: 500,
                estimate_after: 100,
                replacements: vec![super::history::PlannedReplacement {
                    range: 0..2,
                    with: vec![ContextBlock::text(
                        BlockId::new(uuid::Uuid::now_v7()),
                        Role::User,
                        "brief",
                    )],
                }],
            },
        )
        .unwrap();
    let control = recall_control();
    let text = |result: &causa::kernel::ToolResultPayload| match &result.output.content {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };

    // A pattern anchored at the line start only matches the compiler line.
    let anchored = tool
        .execute(
            &recall_call(serde_json::json!({ "query": r"^error\[E\d{4}\]", "regex": true })),
            &control,
            None,
        )
        .await;
    assert_eq!(anchored.status, ToolResultStatus::Succeeded);
    let anchored = text(&anchored);
    assert!(anchored.contains(&format!("{record_id}/{}", error_line.id().0)));
    assert!(anchored.contains("L2: error[E0308]: mismatched types"));
    assert!(!anchored.contains(&other.id().0.to_string()));

    // Case-insensitive by default; case_sensitive narrows it.
    let insensitive = text(
        &tool
            .execute(
                &recall_call(serde_json::json!({ "query": r"^error", "regex": true })),
                &control,
                None,
            )
            .await,
    );
    assert!(insensitive.contains(&other.id().0.to_string()));
    let sensitive = text(
        &tool
            .execute(
                &recall_call(
                    serde_json::json!({ "query": r"^error", "regex": true, "case_sensitive": true }),
                ),
                &control,
                None,
            )
            .await,
    );
    assert!(!sensitive.contains(&other.id().0.to_string()));
    assert!(sensitive.contains(&error_line.id().0.to_string()));

    // Without regex the same text is a literal and finds nothing.
    let literal = text(
        &tool
            .execute(
                &recall_call(serde_json::json!({ "query": r"^error\[E\d{4}\]" })),
                &control,
                None,
            )
            .await,
    );
    assert_eq!(literal, "no hits found for query");

    // Invalid, overlong and misplaced regex options are failed calls.
    for (arguments, message) in [
        (
            serde_json::json!({ "query": "(unclosed", "regex": true }),
            "invalid regular expression",
        ),
        (
            serde_json::json!({ "query": "a".repeat(1_001), "regex": true }),
            "longer than 1000 characters",
        ),
        (
            serde_json::json!({ "id": record_id, "regex": true }),
            "apply only to a query",
        ),
    ] {
        let result = tool.execute(&recall_call(arguments), &control, None).await;
        assert_eq!(result.status, ToolResultStatus::Failed);
        assert!(
            result.output.content["error"]
                .as_str()
                .unwrap()
                .contains(message)
        );
    }
}

#[tokio::test]
async fn test_be07_recall_next_layer_pointers() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());
    let control = recall_control();

    // R1: Summarize record, inserts brief B1
    let orig1 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "initial message",
    );
    let orig2 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::Assistant,
        "initial reply",
    );
    let mut context = Context::new();
    context
        .edit()
        .append(vec![orig1.clone(), orig2.clone()])
        .commit()
        .unwrap();

    let b1 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "brief B1");
    let r1_id = super::history::new_record_id();
    let r1_rewrite = super::history::Rewrite {
        record_id: r1_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..2,
            with: vec![b1.clone()],
        }],
    };
    store.commit_rewrite(&mut context, r1_rewrite).unwrap();

    // E: Elide record, replaces tool result T with shortened T2
    let t = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            BlockId::new(uuid::Uuid::now_v7()),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("long tool result to be shortened")),
        )),
        BlockMeta::default(),
    );
    context.edit().append(vec![t.clone()]).commit().unwrap();

    let t2 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            t.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!("shortened T2")),
        )),
        BlockMeta::default(),
    );
    let e_id = super::history::new_record_id();
    let e_rewrite = super::history::Rewrite {
        record_id: e_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Elide,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 1..2,
            with: vec![t2.clone()],
        }],
    };
    store.commit_rewrite(&mut context, e_rewrite).unwrap();

    // R2: Summarize record that removes T2 among other blocks
    let r2_extra = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "user extra");
    context
        .edit()
        .append(vec![r2_extra.clone()])
        .commit()
        .unwrap();

    let r2_id = super::history::new_record_id();
    let r2_brief = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "r2 brief");
    let r2_rewrite = super::history::Rewrite {
        record_id: r2_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 1..3, // removes T2 and r2_extra
            with: vec![r2_brief.clone()],
        }],
    };
    store.commit_rewrite(&mut context, r2_rewrite).unwrap();

    // M: Merge record that removes B1 (and another brief) and inserts B3
    let another_brief = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        "another brief",
    );
    context
        .edit()
        .append(vec![another_brief.clone()])
        .commit()
        .unwrap();

    let b3 = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "merged B3");
    let m_id = super::history::new_record_id();
    let m_rewrite = super::history::Rewrite {
        record_id: m_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Merge,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..1, // removes B1
            with: vec![b3.clone()],
        }],
    };
    store.commit_rewrite(&mut context, m_rewrite).unwrap();

    // 1. Listing M marks B1 with original: recall id=R1
    let call_m = recall_call(serde_json::json!({ "id": m_id }));
    let res_m = tool.execute(&call_m, &control, None).await;
    let content_m = match &res_m.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_m.contains(&format!("{m_id}/{}", b1.id().0)));
    assert!(content_m.contains(&format!("original: recall id={r1_id}")));

    // 2. Listing R2 marks T2 with original: recall id=E
    let call_r2 = recall_call(serde_json::json!({ "id": r2_id }));
    let res_r2 = tool.execute(&call_r2, &control, None).await;
    let content_r2 = match &res_r2.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_r2.contains(&format!("{r2_id}/{}", t2.id().0)));
    assert!(content_r2.contains(&format!("original: recall id={e_id}")));

    // 3. Listing R1 shows plain originals carrying NO original line (negative case)
    let call_r1 = recall_call(serde_json::json!({ "id": r1_id }));
    let res_r1 = tool.execute(&call_r1, &control, None).await;
    let content_r1 = match &res_r1.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_r1.contains(&format!("{r1_id}/{}", orig1.id().0)));
    assert!(content_r1.contains(&format!("{r1_id}/{}", orig2.id().0)));
    assert!(!content_r1.contains("original: recall id="));

    // 4. Reading B1 directly also carries original: recall id=R1
    let call_read_b1 = recall_call(serde_json::json!({ "id": format!("{m_id}/{}", b1.id().0) }));
    let res_read_b1 = tool.execute(&call_read_b1, &control, None).await;
    let content_read_b1 = match &res_read_b1.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content_read_b1.contains(&format!("original: recall id={r1_id}")));
}

#[tokio::test]
async fn test_be07_recall_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());
    let control = recall_control();

    let orig = ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, "test");
    let mut context = Context::new();
    context.edit().append(vec![orig.clone()]).commit().unwrap();

    let rec_id = super::history::new_record_id();
    let rewrite = super::history::Rewrite {
        record_id: rec_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 100,
        estimate_after: 20,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..1,
            with: vec![ContextBlock::text(
                BlockId::new(uuid::Uuid::now_v7()),
                Role::User,
                "b",
            )],
        }],
    };
    store.commit_rewrite(&mut context, rewrite).unwrap();

    // 1. Neither id nor query
    let res = tool
        .execute(&recall_call(serde_json::json!({})), &control, None)
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("must provide exactly one of 'id' or 'query'")
    );

    // 2. Both id and query
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": rec_id, "query": "q" })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("must provide exactly one of 'id' or 'query'")
    );

    // 3. Empty query
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "query": "" })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("search query cannot be empty")
    );

    // 4. Malformed id
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": "not-a-uuid" })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("not a canonical UUID")
    );

    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": "not-a-uuid/also-bad" })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("not a canonical UUID")
    );

    // 5. Unknown record
    let unknown_uuid = uuid::Uuid::now_v7().to_string();
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": unknown_uuid })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("not in this session's history index")
    );

    // 6. Block not in record
    let other_block_id = uuid::Uuid::now_v7().to_string();
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": format!("{rec_id}/{other_block_id}") })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    assert!(
        res.output
            .content
            .to_string()
            .contains("not among the originals of history record")
    );

    // 7. Out-of-range limit values
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": rec_id, "limit": 0 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": rec_id, "limit": 201 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);

    let res = tool
        .execute(
            &recall_call(
                serde_json::json!({ "id": format!("{rec_id}/{}", orig.id().0), "limit": 0 }),
            ),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    let res = tool
        .execute(
            &recall_call(
                serde_json::json!({ "id": format!("{rec_id}/{}", orig.id().0), "limit": 12001 }),
            ),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);

    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "query": "q", "limit": 0 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);
    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "query": "q", "limit": 21 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);

    let res = tool
        .execute(
            &recall_call(serde_json::json!({ "query": "q", "context": 4 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res.status, ToolResultStatus::Failed);

    // Verify no file paths in any error output
    assert!(!res.output.content.to_string().contains(".history"));
    assert!(!res.output.content.to_string().contains('/'));
}

#[tokio::test]
async fn test_be07_recall_output_cap() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());
    let control = recall_control();

    // Append 1000 index entries without files so search produces > 70 KiB of unreadable notes
    for _ in 0..1000 {
        store.append(HistoryRecordEntry {
            record_id: super::history::new_record_id(),
            created_at_ms: 1_700_000_000_000,
            trigger: super::HistoryTrigger::Auto,
            kind: super::history::RewriteKind::Summarize,
            removed_block_count: 1,
            inserted_block_ids: vec![],
        });
    }

    let call = recall_call(serde_json::json!({ "query": "anything" }));
    let res = tool.execute(&call, &control, None).await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(res.output.truncation, causa::kernel::Truncation::Middle);
    let content = match &res.output.content {
        serde_json::Value::String(s) => s.as_str(),
        _ => unreachable!(),
    };
    assert!(content.len() <= super::recall::MAX_OUTPUT_BYTES);
    assert!(content.contains("bytes omitted; page with offset/limit"));

    // Direct unit verification of cap_output
    let (capped, trunc) = super::recall::cap_output("x".repeat(80 * 1024));
    assert_eq!(trunc, causa::kernel::Truncation::Middle);
    assert!(capped.len() <= super::recall::MAX_OUTPUT_BYTES);
    assert!(capped.contains("bytes omitted; page with offset/limit"));
}

#[tokio::test]
async fn test_be07_recall_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let (_store, _runtime, tool) = setup_recall_test(dir.path());

    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let cancelled_control = CallControl::new(token, None);

    let call = recall_call(serde_json::json!({ "query": "anything" }));
    let res = tool.execute(&call, &cancelled_control, None).await;
    assert_eq!(res.status, ToolResultStatus::Cancelled);
}

/// Releases a test gate when dropped, so a failed assertion cannot leave a
/// worker paused and hang runtime shutdown.
struct ReleaseGate(Arc<crate::tools::TestGate>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Starts a search whose worker pauses at `gate`, and waits until it is paused.
async fn paused_recall(
    tool: &Arc<super::RecallTool>,
    gate: &Arc<crate::tools::TestGate>,
    control: CallControl,
) -> tokio::task::JoinHandle<causa::kernel::ToolResultPayload> {
    *tool.gate.lock().unwrap() = Some(gate.clone());
    let task = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.execute(
                &recall_call(serde_json::json!({ "query": "anything" })),
                &control,
                None,
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.reached.notified())
        .await
        .unwrap();
    task
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_be07_recall_stops_in_flight_on_cancel_deadline_and_drop() {
    let wait = std::time::Duration::from_secs(3);
    // Each case settles its reporter, so each gets its own tool.
    let fresh = || {
        let dir = tempfile::tempdir().unwrap();
        let (_store, _runtime, tool) = setup_recall_test(dir.path());
        let gate = crate::tools::TestGate::new(false);
        (dir, Arc::new(tool), ReleaseGate(gate.clone()), gate)
    };

    // Cancelled while the worker runs: returns Cancelled without waiting for it.
    let (_dir, tool, _release, gate) = fresh();
    let token = CancellationToken::new();
    let task = paused_recall(&tool, &gate, CallControl::new(token.clone(), None)).await;
    token.cancel();
    let result = tokio::time::timeout(wait, task).await.unwrap().unwrap();
    assert_eq!(result.status, ToolResultStatus::Cancelled);
    gate.release();
    tokio::time::timeout(wait, tool.reporter().drain())
        .await
        .unwrap();

    // Past its deadline while the worker runs: returns TimedOut.
    let (_dir, tool, _release, gate) = fresh();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let task = paused_recall(
        &tool,
        &gate,
        CallControl::new(CancellationToken::new(), Some(deadline)),
    )
    .await;
    let result = tokio::time::timeout(wait, task).await.unwrap().unwrap();
    assert_eq!(result.status, ToolResultStatus::TimedOut);
    gate.release();
    tokio::time::timeout(wait, tool.reporter().drain())
        .await
        .unwrap();

    // Dropped while the worker runs: the worker ends once released.
    let (_dir, tool, _release, gate) = fresh();
    let task = paused_recall(&tool, &gate, recall_control()).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    gate.release();
    tokio::time::timeout(wait, tool.reporter().drain())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_be08_shortened_result_note_contains_recall_id_and_recall_reads_original() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());

    let original_text = "original full output of a lengthy command or file read";
    let b0 = ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        Role::User,
        original_text,
    );
    let mut context = Context::new();
    context.edit().append(vec![b0.clone()]).commit().unwrap();

    let record_id = super::history::new_record_id();
    let block_id = b0.id();
    let rewrite = super::history::Rewrite {
        record_id: record_id.clone(),
        trigger: super::HistoryTrigger::Auto,
        kind: super::history::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![super::history::PlannedReplacement {
            range: 0..1,
            with: vec![summary::summary_block(
                summary::new_block_id(),
                "brief summary",
            )],
        }],
    };
    store.commit_rewrite(&mut context, rewrite).unwrap();

    let note = super::shortened_result_note(&record_id, block_id.0);
    let recall_id = format!("{record_id}/{}", block_id.0);
    assert!(
        note.0.contains(&recall_id),
        "note must contain recall id: {}",
        note.0
    );

    let mut output = ToolOutput::new(serde_json::json!("shortened preview"));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        recall_id.clone(),
        original_text.len(),
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let mut result_payload = ToolResultPayload::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolResultStatus::Succeeded,
        output,
    );
    result_payload.notes.push(note);

    // Follow the id exactly as the model sees it in the note.
    let noted_id = result_payload.notes[0]
        .0
        .rsplit_once("recall id=")
        .map(|(_, id)| id.trim())
        .expect("note names a recall id");
    assert_eq!(noted_id, recall_id);
    let call = recall_call(serde_json::json!({ "id": noted_id }));
    let res = tool.execute(&call, &recall_control(), None).await;
    assert_eq!(res.status, ToolResultStatus::Succeeded);
    let output_text = match &res.output.content {
        serde_json::Value::String(s) => s.as_str(),
        other => panic!("expected string output, got: {other:?}"),
    };
    assert!(output_text.contains(original_text));
}
