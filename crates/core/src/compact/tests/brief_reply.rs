//! What a brief reply must be, and when a brief call is not worth making:
//! a reply that calls a tool is no brief, and an automatic summary rejected
//! as `no_shrink` is not repeated until the interval outgrows its brief.

use super::partial::{assistant, brief, user};
use super::{
    CompactionRuntime, HistoryTrigger, TinyAlphaTool, UsageTracker, big_block,
    build_session_with_tool, definition, history_files, is_summary, summary, summary_output,
};
use crate::compact::CompactionAbandon;
use crate::session::{SubmitRequest, WorkState};
use causa::kernel::{
    CallControl, CancellationToken, Context, ContextBlock, GenerationOptions, ModelGateway,
    ModelInvokeError, ModelItem, ModelOutput, ModelRef, ModelRequest, ModelResponse,
    ModelStopReason, ToolCallDraft,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

fn tool_call_output(text: Option<&str>) -> ModelOutput {
    let mut items = Vec::new();
    if let Some(text) = text {
        items.push(ModelItem::text(text));
    }
    items.push(ModelItem::tool_call(ToolCallDraft::new(
        "alpha",
        serde_json::json!({"input": "fixture"}),
    )));
    ModelOutput::new(ModelResponse { items }, ModelStopReason::ToolUse)
}

/// Brief requests answer with a tool call, after `text` when given.
struct ToolCallBriefGateway {
    text: Option<&'static str>,
}

#[async_trait::async_trait]
impl ModelGateway for ToolCallBriefGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        Ok(tool_call_output(self.text))
    }
}

/// Brief requests answer with a brief of `brief_len` characters.
struct SizedBriefGateway {
    brief_len: AtomicUsize,
    briefs: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelGateway for SizedBriefGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        assert!(
            is_summary(request),
            "only brief requests reach this fixture"
        );
        self.briefs.fetch_add(1, Ordering::SeqCst);
        Ok(summary_output(
            &"b".repeat(self.brief_len.load(Ordering::SeqCst)),
        ))
    }
}

fn runtime_with(
    directory: &std::path::Path,
    conversation: &str,
    gateway: Arc<dyn ModelGateway>,
) -> Arc<CompactionRuntime> {
    CompactionRuntime::new(
        &definition(Some(1_000), None),
        &conversation.into(),
        Some(directory),
        Vec::new(),
        None,
        None,
        gateway,
        Arc::new(UsageTracker::default()),
    )
    .unwrap()
}

async fn plan(
    runtime: &CompactionRuntime,
    context: &Context,
    trigger: HistoryTrigger,
) -> Result<super::super::history::Rewrite, CompactionAbandon> {
    runtime
        .plan(
            context,
            &ModelRef::new("fixture"),
            &GenerationOptions::default(),
            trigger,
            &control(),
        )
        .await
}

/// An earlier exchange and a newer request; with a 1,000-token window the
/// newer request is the retained tail and only the exchange is summarized.
fn small_interval() -> Context {
    Context::from_blocks(vec![
        user("earlier question"),
        assistant("earlier answer"),
        user("latest question"),
    ])
    .unwrap()
}

#[test]
fn brief_and_merge_instructions_say_not_to_call_a_tool() {
    let rule = "Output only the brief, as text; do not call any tool.";
    assert!(summary::instruction(false).contains(rule));
    assert!(summary::instruction(true).contains(rule));
    assert!(summary::merge_instruction().contains(rule));
}

#[tokio::test]
async fn a_brief_reply_that_calls_a_tool_is_abandoned_even_with_text() {
    for text in [Some("I'll read the file first."), None] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime_with(
            directory.path(),
            "conversation-tool-call",
            Arc::new(ToolCallBriefGateway { text }),
        );
        let context = Context::from_blocks(vec![big_block(2_000), user("latest")]).unwrap();
        let before = context.blocks().to_vec();
        for trigger in [HistoryTrigger::Manual, HistoryTrigger::Auto] {
            assert_eq!(
                plan(&runtime, &context, trigger).await.unwrap_err(),
                CompactionAbandon::ToolCall,
                "{text:?} {trigger:?}"
            );
        }
        assert_eq!(context.blocks(), before.as_slice());
        assert!(history_files(directory.path(), "conversation-tool-call").is_empty());
        assert!(runtime.entries().is_empty());
    }
    assert_eq!(CompactionAbandon::ToolCall.name(), "tool_call");
}

#[tokio::test]
async fn a_merge_reply_that_calls_a_tool_is_abandoned() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = runtime_with(
        directory.path(),
        "conversation-merge-tool-call",
        Arc::new(ToolCallBriefGateway { text: None }),
    );
    let context = Context::from_blocks(vec![
        brief("first brief"),
        brief("second brief"),
        user("u"),
        assistant("a"),
    ])
    .unwrap();
    let merge = runtime
        .plan_merge(
            &context,
            &ModelRef::new("fixture"),
            &GenerationOptions::default(),
            HistoryTrigger::Auto,
            &control(),
        )
        .await;
    assert_eq!(merge.unwrap_err(), CompactionAbandon::ToolCall);
    assert!(history_files(directory.path(), "conversation-merge-tool-call").is_empty());
}

#[tokio::test]
async fn an_automatic_no_shrink_is_remembered_until_the_interval_outgrows_its_brief() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Arc::new(SizedBriefGateway {
        brief_len: AtomicUsize::new(3_000),
        briefs: AtomicUsize::new(0),
    });
    let runtime = runtime_with(directory.path(), "conversation-no-shrink", gateway.clone());
    let small = small_interval();
    assert!(!runtime.summary_cannot_shrink(&small));

    // The brief is larger than the exchange it would replace.
    assert_eq!(
        plan(&runtime, &small, HistoryTrigger::Auto)
            .await
            .unwrap_err(),
        CompactionAbandon::NoShrink
    );
    assert_eq!(gateway.briefs.load(Ordering::SeqCst), 1);
    assert!(runtime.summary_cannot_shrink(&small));

    // A larger interval could take a brief of that size again.
    let mut grown = Context::from_blocks(vec![
        user(&"q".repeat(6_000)),
        assistant("earlier answer"),
        user("latest question"),
    ])
    .unwrap();
    assert!(!runtime.summary_cannot_shrink(&grown));
    let rewrite = plan(&runtime, &grown, HistoryTrigger::Auto).await.unwrap();
    assert_eq!(gateway.briefs.load(Ordering::SeqCst), 2);

    // A commit clears what was remembered.
    runtime.commit_rewrite(&mut grown, rewrite).unwrap();
    assert!(!runtime.summary_cannot_shrink(&small));
}

#[tokio::test]
async fn manual_compaction_neither_sets_nor_obeys_the_remembered_no_shrink() {
    let directory = tempfile::tempdir().unwrap();
    let gateway = Arc::new(SizedBriefGateway {
        brief_len: AtomicUsize::new(3_000),
        briefs: AtomicUsize::new(0),
    });
    let runtime = runtime_with(directory.path(), "conversation-manual", gateway.clone());
    let small = small_interval();

    assert_eq!(
        plan(&runtime, &small, HistoryTrigger::Manual)
            .await
            .unwrap_err(),
        CompactionAbandon::NoShrink
    );
    assert!(!runtime.summary_cannot_shrink(&small));

    // After an automatic rejection, a manual summary still calls the model.
    plan(&runtime, &small, HistoryTrigger::Auto)
        .await
        .unwrap_err();
    assert!(runtime.summary_cannot_shrink(&small));
    plan(&runtime, &small, HistoryTrigger::Manual)
        .await
        .unwrap_err();
    assert_eq!(gateway.briefs.load(Ordering::SeqCst), 3);
}

/// Ordinary rounds 0 and 1 call the fixture tool, later rounds finish; no
/// usage is reported, so the estimate stays on bytes. Brief requests answer
/// with a brief far larger than the unprotected material.
struct LargeBriefToolRounds {
    main_calls: AtomicUsize,
    briefs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelGateway for LargeBriefToolRounds {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        if is_summary(request) {
            self.briefs.fetch_add(1, Ordering::SeqCst);
            let mut output = summary_output(&"b".repeat(6_000));
            output.usage = None;
            return Ok(output);
        }
        if self.main_calls.fetch_add(1, Ordering::SeqCst) < 2 {
            return Ok(tool_call_output(None));
        }
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("done")],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

#[tokio::test]
async fn later_rounds_of_a_work_skip_a_brief_that_could_not_shrink() {
    let directory = tempfile::tempdir().unwrap();
    let briefs = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(LargeBriefToolRounds {
        main_calls: AtomicUsize::new(0),
        briefs: briefs.clone(),
    });
    let (mut session, _runtime) = build_session_with_tool(
        directory.path(),
        definition(Some(1_000), None),
        Context::from_blocks(vec![user("earlier question"), assistant("earlier answer")]).unwrap(),
        gateway.clone(),
        Arc::new(UsageTracker::default()),
        Arc::new(TinyAlphaTool),
    );
    let handle = session.handle();
    // The protected request alone is over the 700-token line; only the
    // earlier exchange and the small tool rounds can be summarized.
    let request: ContextBlock = big_block(2_400);
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "large-request".into(),
            blocks: vec![request],
        })
        .unwrap();
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(matches!(observation.state, WorkState::Finished(_)));
    assert_eq!(gateway.main_calls.load(Ordering::SeqCst), 3);
    // Round 0 asked for a brief and rejected it; rounds 1 and 2 did not ask.
    assert_eq!(briefs.load(Ordering::SeqCst), 1);
    assert!(history_files(directory.path(), "conversation").is_empty());
    session.shutdown().await;
}
