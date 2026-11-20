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
    // A 1,400-token line: after the fixed prefix (~190 tokens), the chain
    // budget is capped at available / 2 (~605 tokens). Two briefs over that
    // budget trigger a merge.
    let (runtime, inputs) = runtime(directory.path(), Some(2_000));
    let rules = system("Project rules: answer in one line.");
    let (first, second) = (
        brief(&format!("first brief {}", "a ".repeat(500))),
        brief(&format!("second brief {}", "b ".repeat(500))),
    );
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
    let chain = |count: usize, body: &str| -> Vec<ContextBlock> {
        let mut blocks: Vec<ContextBlock> = (0..count)
            .map(|index| brief(&format!("brief {index} {body}")))
            .collect();
        blocks.extend([user("u"), assistant("a")]);
        blocks
    };
    let large = "x".repeat(1_200);
    // One brief: nothing to merge, however small the budget or large the brief.
    assert!(!segment::chain_needs_merge(
        &chain(1, &large),
        Some(1_000),
        70
    ));
    // Within the budget: no merge.
    assert!(!segment::chain_needs_merge(
        &chain(2, ""),
        Some(1_000_000),
        70
    ));
    assert!(!segment::chain_needs_merge(&chain(2, ""), Some(2_000), 70));
    // Two briefs over the small-window cap (available / 2 = 700): merge.
    assert!(segment::chain_needs_merge(
        &chain(2, &large),
        Some(2_000),
        70
    ));
    // Without a window only the count decides: more than eight briefs.
    assert!(!segment::chain_needs_merge(&chain(8, ""), None, 70));
    assert!(segment::chain_needs_merge(&chain(9, ""), None, 70));
    assert!(segment::chain_needs_merge(
        &chain(9, ""),
        Some(1_000_000),
        70
    ));
}

#[test]
fn test_be28_the_chain_budget_comes_from_the_line_minus_the_fixed_prefix() {
    // Small windows are capped at (line - prefix) / 2 until the 4,000-token floor.
    assert_eq!(segment::chain_budget(Some(10_000), 80, 0), Some(4_000));
    assert_eq!(segment::chain_budget(Some(10_000), 80, 2_000), Some(3_000));
    assert_eq!(segment::chain_budget(Some(10_000), 80, 9_000), Some(0));
    assert_eq!(segment::chain_budget(None, 80, 2_000), None);

    // The same two briefs fit the budget with no fixed prefix and do not
    // once a system block takes part of the line (in both the available / 2
    // cap regime and the large-window available / 8 regime).
    let small_briefs = vec![brief("first brief"), brief("second brief")];
    let small_tokens = segment::blocks_estimate(&small_briefs);
    let small_window = (small_tokens + 10) * 2 * 100 / 70 + 100;
    let mut small_blocks = small_briefs.clone();
    small_blocks.extend([user("u"), assistant("a")]);
    assert!(!segment::chain_needs_merge(
        &small_blocks,
        Some(small_window),
        70
    ));
    let small_line = small_window * 70 / 100;
    let small_prefix_bytes = usize::try_from((small_line - 2 * small_tokens + 40) * 3).unwrap();
    let mut small_prefixed = vec![system(&"r".repeat(small_prefix_bytes))];
    small_prefixed.extend(small_blocks);
    assert!(segment::fixed_prefix_estimate(&small_prefixed) > small_line - 2 * small_tokens);
    assert!(segment::chain_needs_merge(
        &small_prefixed,
        Some(small_window),
        70
    ));

    let large_briefs = vec![brief(&"a".repeat(7_500)), brief(&"b".repeat(7_500))];
    let large_tokens = segment::blocks_estimate(&large_briefs);
    assert!(large_tokens > segment::CHAIN_BUDGET_FLOOR);
    let large_window = (large_tokens + 10) * 8 * 100 / 70 + 100;
    let mut large_blocks = large_briefs.clone();
    large_blocks.extend([user("u"), assistant("a")]);
    assert!(!segment::chain_needs_merge(
        &large_blocks,
        Some(large_window),
        70
    ));
    let large_line = large_window * 70 / 100;
    let large_prefix_bytes = usize::try_from((large_line - 8 * large_tokens + 40) * 3).unwrap();
    let mut large_prefixed = vec![system(&"r".repeat(large_prefix_bytes))];
    large_prefixed.extend(large_blocks);
    assert!(segment::fixed_prefix_estimate(&large_prefixed) > large_line - 8 * large_tokens);
    assert!(segment::chain_needs_merge(
        &large_prefixed,
        Some(large_window),
        70
    ));
}

#[test]
fn test_cb01_chain_budget_floor_cap_and_large_window_regimes() {
    // Tiny window (4K, 70% threshold, 1,500 prefix -> available = 1,300):
    // capped at available / 2 = 650 instead of 4,000.
    assert_eq!(segment::chain_budget(Some(4_000), 70, 1_500), Some(650));
    // Boundary where available / 2 == 4,000 (available = 8,000): 4,000.
    assert_eq!(segment::chain_budget(Some(13_572), 70, 1_500), Some(4_000));
    // 14K and 32K windows (70% threshold, 1,500 prefix) hit the 4,000-token floor.
    assert_eq!(segment::chain_budget(Some(14_000), 70, 1_500), Some(4_000));
    assert_eq!(segment::chain_budget(Some(32_000), 70, 1_500), Some(4_000));
    // Boundary where available / 8 == 4,000 (available = 32,000): 4,000.
    assert_eq!(segment::chain_budget(Some(47_858), 70, 1_500), Some(4_000));
    // 64K+ large windows match available / 8 identically.
    assert_eq!(segment::chain_budget(Some(64_000), 70, 1_500), Some(5_412));
    assert_eq!(
        segment::chain_budget(Some(128_000), 70, 1_500),
        Some(11_012)
    );
    assert_eq!(
        segment::chain_budget(Some(384_000), 70, 1_500),
        Some(33_412)
    );
    // Prefix at or above trigger line leaves 0 budget; unknown window returns None.
    assert_eq!(segment::chain_budget(Some(14_000), 70, 9_800), Some(0));
    assert_eq!(segment::chain_budget(Some(14_000), 70, 12_000), Some(0));
    assert_eq!(segment::chain_budget(None, 70, 1_500), None);
}

#[test]
fn test_cb02_two_typical_briefs_on_14k_window_do_not_merge_until_third() {
    // Fixed prefix ~1,500 tokens on a 14K window (70% threshold) -> 4,000-token chain budget.
    let prefix = system(&"p".repeat(4_400));
    let prefix_tokens = segment::fixed_prefix_estimate(std::slice::from_ref(&prefix));
    assert!((1_450..=1_550).contains(&prefix_tokens), "{prefix_tokens}");
    assert_eq!(
        segment::chain_budget(Some(14_000), 70, prefix_tokens),
        Some(segment::CHAIN_BUDGET_FLOOR)
    );

    // Two briefs totaling ~3,400 tokens stay under the 4,000-token floor and do not merge.
    let b1 = brief(&"a".repeat(4_750));
    let b2 = brief(&"b".repeat(4_750));
    let two_tokens = segment::blocks_estimate(&[b1.clone(), b2.clone()]);
    assert!(
        (3_300..=3_500).contains(&two_tokens) && two_tokens <= segment::CHAIN_BUDGET_FLOOR,
        "{two_tokens}"
    );
    let two_blocks = vec![
        prefix.clone(),
        b1.clone(),
        b2.clone(),
        user("u"),
        assistant("a"),
    ];
    assert!(!segment::chain_needs_merge(&two_blocks, Some(14_000), 70));

    // Adding a third brief pushes the chain past 4,000 tokens and triggers merge.
    let b3 = brief(&"c".repeat(4_750));
    let three_tokens = segment::blocks_estimate(&[b1, b2, b3.clone()]);
    assert!(three_tokens > segment::CHAIN_BUDGET_FLOOR, "{three_tokens}");
    let mut three_blocks = two_blocks;
    three_blocks.insert(3, b3);
    assert!(segment::chain_needs_merge(&three_blocks, Some(14_000), 70));

    // A single brief over 4,000 tokens still never triggers a merge.
    let huge = brief(&"h".repeat(12_500));
    assert!(segment::blocks_estimate(std::slice::from_ref(&huge)) > segment::CHAIN_BUDGET_FLOOR);
    let single_blocks = vec![prefix, huge, user("u"), assistant("a")];
    assert!(!segment::chain_needs_merge(
        &single_blocks,
        Some(14_000),
        70
    ));
}
