//! Merging the brief chain: when it happens, what replaces the chain, the
//! record it leaves and the budget it is measured against.

use super::super::block_to_text as block_text;
use super::super::expansion::{BlockStatus, expand_conversation};
use super::super::history::RewriteKind;
use super::super::segment;
use super::partial::{assistant, brief, ids, system, user};
use super::tool_change;
use super::{CompactionRuntime, HistoryTrigger, ToolThenDone, UsageTracker, definition};
use causa::kernel::{
    CallControl, CancellationToken, Context, ContextBlock, GenerationOptions, ModelGateway,
    ModelRef, current_tools,
};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

/// The material of each brief request, in order.
type Requests = Arc<Mutex<Vec<Vec<ContextBlock>>>>;

/// A runtime whose brief requests are recorded.
fn runtime(directory: &std::path::Path, window: Option<u64>) -> (Arc<CompactionRuntime>, Requests) {
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: inputs.clone(),
        tool_call_first: false,
        cache_read_tokens: None,
    }) as Arc<dyn ModelGateway>;
    let runtime = CompactionRuntime::new(
        &definition(window, None),
        &"conversation-merge".into(),
        Some(directory),
        Vec::new(),
        None,
        None,
        gateway,
        Arc::new(UsageTracker::default()),
    )
    .unwrap();
    (runtime, inputs)
}

#[tokio::test]
async fn test_be13_a_chain_over_its_budget_merges_into_one_brief_listing_the_old_ones() {
    let directory = tempfile::tempdir().unwrap();
    // A 1,400-token line: the chain budget is about 160 tokens, under two
    // briefs.
    let (runtime, inputs) = runtime(directory.path(), Some(2_000));
    let rules = system("Project rules: answer in one line.");
    let (first, second) = (brief("first brief"), brief("second brief"));
    let blocks = vec![
        rules.clone(),
        tool_change("alpha"),
        first.clone(),
        tool_change("beta"),
        second.clone(),
        user("u"),
        assistant("a"),
    ];
    let tools = current_tools(&blocks);
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    assert!(runtime.needs_merge(&context));
    let rewrite = runtime
        .plan_merge(
            &context,
            &ModelRef::new("fixture"),
            &GenerationOptions::default(),
            HistoryTrigger::Auto,
            &CallControl::new(CancellationToken::new(), None),
        )
        .await
        .unwrap();
    assert_eq!(rewrite.kind, RewriteKind::Merge);
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();

    // The request is the head and the merge instruction.
    let request = inputs.lock().unwrap()[0].clone();
    assert_eq!(ids(&request[..5]), ids(&blocks[..5]));
    assert_eq!(request.len(), 6);
    assert!(block_text(&request[5]).contains("merging the working briefs"));

    // [rules, one declaration, the merged brief, u, a]
    let after = context.blocks();
    assert_eq!(after.len(), 5, "{after:?}");
    assert_eq!(after[0], rules, "the fixed prefix is never rewritten");
    assert_eq!(current_tools(after), tools, "the tool state is unchanged");
    assert!(segment::is_brief(&after[2]));
    assert!(!runtime.needs_merge(&context), "one brief is never merged");
    let merged = block_text(&after[2]);
    let pointers = format!(
        "Earlier briefs: recall id={record}/{first}, recall id={record}/{second}",
        record = entry.record_id,
        first = first.id().0,
        second = second.id().0,
    );
    assert!(merged.ends_with(&pointers), "{merged}");
    assert_eq!(ids(&after[3..]), ids(&blocks[5..]));

    // The record keeps the chain, declarations included, and not the prefix.
    let history = runtime.history().unwrap();
    let record = history.read(&entry.record_id).unwrap();
    assert_eq!(record.kind, RewriteKind::Merge);
    assert_eq!(record.replacements.len(), 1);
    assert_eq!(record.replacements[0].originals, blocks[1..5].to_vec());

    // Expanded, the old briefs come back in place.
    let expanded = expand_conversation(context.blocks(), &runtime.entries(), None, |id| {
        history.read(id)
    });
    let shown: Vec<ContextBlock> = expanded
        .blocks
        .iter()
        .map(|block| block.block.clone())
        .collect();
    assert_eq!(shown, blocks);
    let merged_status = BlockStatus::Summarized(entry.record_id.clone());
    assert_eq!(expanded.blocks[0].status, BlockStatus::Original);
    assert!(
        expanded.blocks[1..5]
            .iter()
            .all(|block| block.status == merged_status)
    );
}

#[test]
fn test_be13_a_chain_within_its_budget_or_of_one_brief_is_not_merged() {
    let chain = |count: usize| -> Vec<ContextBlock> {
        let mut blocks: Vec<ContextBlock> = (0..count)
            .map(|index| brief(&format!("brief {index}")))
            .collect();
        blocks.extend([user("u"), assistant("a")]);
        blocks
    };
    // One brief: nothing to merge, however small the budget.
    assert!(!segment::chain_needs_merge(&chain(1), Some(1_000), 70));
    // Within the budget: no merge.
    assert!(!segment::chain_needs_merge(&chain(2), Some(1_000_000), 70));
    assert!(segment::chain_needs_merge(&chain(2), Some(2_000), 70));
    // Without a window only the count decides: more than eight briefs.
    assert!(!segment::chain_needs_merge(&chain(8), None, 70));
    assert!(segment::chain_needs_merge(&chain(9), None, 70));
    assert!(segment::chain_needs_merge(&chain(9), Some(1_000_000), 70));
}

#[test]
fn test_be28_the_chain_budget_comes_from_the_line_minus_the_fixed_prefix() {
    assert_eq!(segment::chain_budget(Some(10_000), 80, 0), Some(1_000));
    assert_eq!(segment::chain_budget(Some(10_000), 80, 2_000), Some(750));
    assert_eq!(segment::chain_budget(Some(10_000), 80, 9_000), Some(0));
    assert_eq!(segment::chain_budget(None, 80, 2_000), None);

    // The same two briefs fit the budget with no fixed prefix and do not
    // once a system block takes part of the line.
    let briefs = vec![brief("first brief"), brief("second brief")];
    let chain_tokens = segment::blocks_estimate(&briefs);
    let window = (chain_tokens + 10) * 8 * 100 / 70 + 100;
    let mut blocks = briefs.clone();
    blocks.extend([user("u"), assistant("a")]);
    assert!(!segment::chain_needs_merge(&blocks, Some(window), 70));
    let line = window * 70 / 100;
    let prefix_bytes = usize::try_from((line - 8 * chain_tokens + 40) * 3).unwrap();
    let mut prefixed = vec![system(&"r".repeat(prefix_bytes))];
    prefixed.extend(blocks);
    assert!(segment::fixed_prefix_estimate(&prefixed) > line - 8 * chain_tokens);
    assert!(segment::chain_needs_merge(&prefixed, Some(window), 70));
}
