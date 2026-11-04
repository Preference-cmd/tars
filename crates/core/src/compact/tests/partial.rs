//! Partial summaries: cut points, budgets, tool declarations, the fixed
//! prefix, notes and expansion of put-back blocks.

use super::super::CompactionAbandon;
use super::super::expansion::{BlockStatus, expand_conversation};
use super::super::history::{HistoryStore, PlannedReplacement, Rewrite};
use super::super::segment::{self, PrefixTooLarge};
use super::super::summary;
use super::{CompactionRuntime, HistoryTrigger, UsageTracker};
use super::{ToolThenDone, big_block, definition, summarize_rewrite, tool_change};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, CallControl, CancellationToken, Context, ContextBlock,
    ModelGateway, Role, ToolCallPayload, ToolChange, ToolOutput, ToolResultPayload,
    ToolResultStatus, current_tools,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn text(role: Role, body: &str) -> ContextBlock {
    ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), role, body)
}

fn user(body: &str) -> ContextBlock {
    text(Role::User, body)
}

fn assistant(body: &str) -> ContextBlock {
    text(Role::Assistant, body)
}

fn system(body: &str) -> ContextBlock {
    text(Role::System, body)
}

fn call(name: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(name, serde_json::json!({}))),
        BlockMeta::default(),
    )
}

fn result(call: &ContextBlock, output: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call.id(),
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!(output)),
        )),
        BlockMeta::default(),
    )
}

fn ids(blocks: &[ContextBlock]) -> Vec<BlockId> {
    blocks.iter().map(ContextBlock::id).collect()
}

fn brief(body: &str) -> ContextBlock {
    summary::summary_block(summary::new_block_id(), body)
}

/// A runtime over a fresh history directory whose summaries answer text.
fn runtime(
    directory: &std::path::Path,
    window: Option<u64>,
) -> (Arc<CompactionRuntime>, Arc<AtomicUsize>) {
    let summary_calls = Arc::new(AtomicUsize::new(0));
    let gateway = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: summary_calls.clone(),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
        cache_read_tokens: None,
    }) as Arc<dyn ModelGateway>;
    let runtime = CompactionRuntime::new(
        &definition(window, None),
        &"conversation-partial".into(),
        Some(directory),
        Vec::new(),
        None,
        None,
        gateway,
        Arc::new(UsageTracker::default()),
    )
    .unwrap();
    (runtime, summary_calls)
}

async fn plan(
    runtime: &CompactionRuntime,
    context: &Context,
) -> Result<Rewrite, CompactionAbandon> {
    runtime
        .plan(
            context,
            &causa::kernel::ModelRef::new("fixture"),
            &causa::kernel::GenerationOptions::default(),
            HistoryTrigger::Manual,
            &CallControl::new(CancellationToken::new(), None),
        )
        .await
}

#[test]
fn test_be10_cut_points_are_user_messages_and_the_ends_of_completed_tool_rounds() {
    let single = call("alpha");
    let single_result = result(&single, "one");
    let first = call("alpha");
    let second = call("beta");
    let first_result = result(&first, "first");
    let second_result = result(&second, "second");
    let blocks = vec![
        user("u0"),
        assistant("a0"),
        user("u1"),
        single,
        single_result,
        assistant("a1"),
        user("u2"),
        first,
        second,
        first_result,
        second_result,
        assistant("a2"),
    ];
    let cuts: Vec<usize> = (0..=blocks.len())
        .filter(|&index| segment::is_cut_point(&blocks, index))
        .collect();
    // User messages, the block after each completed round, and the end.
    assert_eq!(cuts, vec![0, 2, 5, 6, 11, 12]);

    // Whatever the budget, the tail starts at the earliest cut point after
    // the head that fits, so no call loses its result and the parallel round
    // stays together.
    let total = segment::blocks_estimate(&blocks);
    for budget in 0..=total + 1 {
        let k = segment::tail_start(&blocks, 0, budget);
        assert!(cuts.contains(&k) && k > 0, "budget {budget}: k = {k}");
        assert!(segment::blocks_estimate(&blocks[k..]) <= budget);
        for &earlier in cuts.iter().filter(|&&cut| cut > 0 && cut < k) {
            assert!(
                segment::blocks_estimate(&blocks[earlier..]) > budget,
                "budget {budget}: {earlier} fits before {k}"
            );
        }
    }
}

#[test]
fn test_be10_a_tail_that_fits_only_mid_round_starts_at_the_next_cut_point() {
    let lookup = call("alpha");
    let large_result = result(&lookup, &"z".repeat(3_000));
    let blocks = vec![
        user("u0"),
        user("u1"),
        lookup,
        large_result,
        assistant("a1"),
    ];
    // The result and the answer fit, but starting there would cut the
    // result from its call: k moves on to the answer, a smaller tail.
    let budget = segment::blocks_estimate(&blocks[3..]);
    assert!(segment::blocks_estimate(&blocks[1..]) > budget);
    assert!(!segment::is_cut_point(&blocks, 2));
    assert!(!segment::is_cut_point(&blocks, 3));
    assert_eq!(segment::tail_start(&blocks, 0, budget), 4);
}

#[test]
fn test_be10_a_last_result_over_the_budget_leaves_the_tail_empty() {
    let lookup = call("alpha");
    let large_result = result(&lookup, &"z".repeat(3_000));
    let blocks = vec![user("u0"), user("u1"), lookup, large_result];
    let budget = segment::blocks_estimate(&blocks[3..]) - 1;
    assert_eq!(segment::tail_start(&blocks, 0, budget), blocks.len());
    // A head that is the whole Context leaves nothing to summarize.
    assert_eq!(segment::tail_start(&blocks, 4, budget), blocks.len());
}

#[test]
fn test_be28_the_tail_budget_comes_from_the_line_minus_the_fixed_prefix() {
    // Line 7,000: a quarter of what the prefix leaves.
    assert_eq!(
        segment::retained_tail_budget(Some(10_000), 70, 0),
        Ok(1_750)
    );
    assert_eq!(
        segment::retained_tail_budget(Some(10_000), 70, 1_000),
        Ok(1_500)
    );
    // Up to half the line the prefix is allowed; above it nothing can help.
    assert_eq!(
        segment::retained_tail_budget(Some(10_000), 70, 3_500),
        Ok(875)
    );
    assert_eq!(
        segment::retained_tail_budget(Some(10_000), 70, 3_501),
        Err(PrefixTooLarge)
    );
    // Large windows and unknown windows use the cap.
    assert_eq!(
        segment::retained_tail_budget(Some(1_000_000), 70, 0),
        Ok(segment::RETAINED_TAIL_CAP)
    );
    assert_eq!(
        segment::retained_tail_budget(Some(1_000_000), 70, 100_000),
        Ok(segment::RETAINED_TAIL_CAP)
    );
    assert_eq!(
        segment::retained_tail_budget(None, 70, 1_000_000),
        Ok(segment::RETAINED_TAIL_CAP)
    );
}

#[test]
fn test_be28_the_fixed_prefix_counts_leading_system_text_and_the_whole_tool_state() {
    let blocks = vec![
        system("first system block"),
        tool_change("alpha"),
        system("second system block"),
        user("question"),
        tool_change("beta"),
        assistant("answer"),
    ];
    assert_eq!(segment::fixed_prefix_end(&blocks), 3);
    let system_only = segment::blocks_estimate(&[blocks[0].clone(), blocks[2].clone()]);
    // The declaration of every current tool counts, wherever it sits.
    let estimate = segment::fixed_prefix_estimate(&blocks);
    let with_alpha = segment::fixed_prefix_estimate(&blocks[..4]);
    assert!(estimate > with_alpha && with_alpha > system_only);
    // Without system blocks only the tools count; without either, nothing.
    assert_eq!(segment::fixed_prefix_end(&blocks[3..]), 0);
    assert_eq!(segment::fixed_prefix_estimate(&[user("q")]), 0);
}

#[tokio::test]
async fn test_be14_tool_changes_in_the_interval_fold_into_one_declaration() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), Some(10_000));
    let blocks = vec![
        big_block(9_000),
        tool_change("alpha"),
        assistant("a1"),
        big_block(9_000),
        tool_change("beta"),
        assistant("a2"),
        user("u3"),
        assistant("a3"),
    ];
    let tools = current_tools(&blocks);
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    assert_eq!(rewrite.replacements.len(), 1);
    assert_eq!(rewrite.replacements[0].range, 0..6);
    let without_declaration = {
        let mut broken = rewrite.clone();
        broken.record_id = super::super::history::new_record_id();
        broken.replacements[0]
            .with
            .retain(|block| !matches!(block.content(), BlockContent::ToolChange(_)));
        broken
    };
    runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();
    assert_eq!(current_tools(after), tools, "the tool state is unchanged");
    let declarations = after
        .iter()
        .filter(|block| matches!(block.content(), BlockContent::ToolChange(_)))
        .count();
    assert_eq!(declarations, 1, "both declarations fold into one");
    assert!(matches!(after[0].content(), BlockContent::ToolChange(_)));
    assert!(segment::is_brief(&after[1]));
    assert_eq!(ids(&after[2..]), ids(&blocks[6..]));

    // Counterexample: a rewrite without the declaration loses the tools.
    // Causa's runner compares the prepared frame's folded tools with the
    // round's bound tools in exactly this way and refuses the frame.
    let mut broken = Context::from_blocks(blocks).unwrap();
    let store = HistoryStore::new(directory.path(), "conversation-broken", Vec::new());
    store
        .commit_rewrite(&mut broken, without_declaration)
        .unwrap();
    assert!(!ToolChange::between(&current_tools(broken.blocks()), &tools).is_empty());
    assert!(ToolChange::between(&current_tools(context.blocks()), &tools).is_empty());
}

#[tokio::test]
async fn test_be14_no_declaration_is_added_when_the_interval_changes_no_tool() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), Some(10_000));
    let blocks = vec![
        tool_change("alpha"),
        big_block(9_000),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
    ];
    assert_eq!(segment::head_end(&blocks), 1);
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    assert_eq!(rewrite.replacements.len(), 1);
    assert_eq!(rewrite.replacements[0].range, 1..3);
    assert_eq!(rewrite.replacements[0].with.len(), 1, "only the brief");
    runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();
    assert_eq!(after[0], blocks[0]);
    assert!(segment::is_brief(&after[1]));
    assert_eq!(ids(&after[2..]), ids(&blocks[3..]));
    assert_eq!(current_tools(after), current_tools(&blocks));
}

#[tokio::test]
async fn test_be25_leading_system_blocks_and_their_declaration_are_never_rewritten() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), Some(10_000));
    let blocks = vec![
        system("You are the fixture assistant."),
        tool_change("alpha"),
        system("Project rules: keep answers short."),
        big_block(9_000),
        assistant("a1"),
        user("u2"),
        assistant("a2"),
    ];
    assert_eq!(segment::head_end(&blocks), 3);
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();
    assert_eq!(&after[..3], &blocks[..3], "same ids and content");
    assert!(segment::is_brief(&after[3]));
    assert_eq!(ids(&after[4..]), ids(&blocks[5..]));
    let record = runtime.history().unwrap().read(&entry.record_id).unwrap();
    let originals: Vec<BlockId> = record
        .replacements
        .iter()
        .flat_map(|replacement| ids(&replacement.originals))
        .collect();
    assert_eq!(originals, ids(&blocks[3..5]));
    for prefix in &blocks[..3] {
        assert!(!originals.contains(&prefix.id()));
        assert!(!entry.inserted_block_ids.contains(&prefix.id()));
    }
}

#[tokio::test]
async fn test_be27_a_note_in_the_interval_goes_to_the_record_and_one_in_the_tail_stays() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), Some(10_000));
    let interval_note = system("Repair note: an earlier call was interrupted.");
    let tail_note = system("Repair note: the last call was interrupted.");
    let blocks = vec![
        system("You are the fixture assistant."),
        big_block(9_000),
        interval_note.clone(),
        assistant("a1"),
        big_block(9_000),
        assistant("a2"),
        user("u3"),
        tail_note.clone(),
        assistant("a3"),
    ];
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let record = runtime.history().unwrap().read(&entry.record_id).unwrap();
    let originals: Vec<BlockId> = record
        .replacements
        .iter()
        .flat_map(|replacement| ids(&replacement.originals))
        .collect();
    assert!(originals.contains(&interval_note.id()));
    assert!(!originals.contains(&tail_note.id()));
    let after = context.blocks();
    assert_eq!(
        ids(&after[2..]),
        ids(&blocks[6..]),
        "the tail note stays in place"
    );
    // The brief stands between the leading system block and the tail, so the
    // tail note never joins the system prompt.
    assert_eq!(segment::fixed_prefix_end(after), 1);
}

#[tokio::test]
async fn test_be28_a_fixed_prefix_over_half_the_line_sends_no_brief_request() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, summary_calls) = runtime(directory.path(), Some(1_000));
    // Line 700: a 1,200-byte system block (400 tokens) is over half of it.
    let context = Context::from_blocks(vec![
        system(&"s".repeat(1_200)),
        big_block(3_000),
        assistant("a1"),
        user("u2"),
    ])
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            plan(&runtime, &context).await.unwrap_err(),
            CompactionAbandon::PrefixTooLarge
        );
    }
    assert_eq!(summary_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn test_be12_the_request_in_progress_is_put_back_after_the_brief_in_the_same_record() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), Some(10_000));
    let request = user(&"request in progress ".repeat(100));
    let lookup = call("alpha");
    let lookup_result = result(&lookup, &"r".repeat(9_000));
    let blocks = vec![
        big_block(6_000),
        assistant("a0"),
        request.clone(),
        tool_change("alpha"),
        lookup,
        lookup_result,
    ];
    runtime.set_current_work_inputs(std::slice::from_ref(&request));
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    // Two ranges around the request, in one rewrite.
    assert_eq!(
        rewrite
            .replacements
            .iter()
            .map(|replacement| replacement.range.clone())
            .collect::<Vec<_>>(),
        vec![0..2, 2..6]
    );
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();
    assert_eq!(after.len(), 3);
    assert!(matches!(after[0].content(), BlockContent::ToolChange(_)));
    assert!(segment::is_brief(&after[1]));
    assert_eq!(after[2], request, "same id and content");
    let brief_text = serde_json::to_string(&after[1]).unwrap();
    assert!(!brief_text.contains("request in progress"));
    let record = runtime.history().unwrap().read(&entry.record_id).unwrap();
    let originals: Vec<BlockId> = record
        .replacements
        .iter()
        .flat_map(|replacement| ids(&replacement.originals))
        .collect();
    assert_eq!(originals, ids(&blocks), "both segments in one record");
    assert_eq!(entry.removed_block_count, 5, "the request is not removed");
}

#[test]
fn test_be12_expansion_follows_a_request_put_back_twice_and_summarized_later() {
    let directory = tempfile::tempdir().unwrap();
    let store = HistoryStore::new(directory.path(), "conversation-put-back", Vec::new());
    let request = user("request in progress");
    let earlier = vec![user("e1"), assistant("e2")];
    let mut conversation = earlier.clone();
    conversation.push(request.clone());
    let (x1, x2, x3, x4, y1) = (
        assistant("x1"),
        assistant("x2"),
        assistant("x3"),
        assistant("x4"),
        user("y1"),
    );
    conversation.extend([x1.clone(), x2.clone()]);
    let mut context = Context::from_blocks(conversation.clone()).unwrap();

    // Round 1: the earlier work is summarized, the request put back, x2 kept.
    let first = brief("first brief");
    let blocks = context.blocks().to_vec();
    let one = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(segment::replacements(
                &blocks,
                0,
                4,
                &[2],
                vec![first.clone()],
            )),
        )
        .unwrap();
    assert_eq!(
        ids(context.blocks()),
        ids(&[first.clone(), request.clone(), x2.clone()])
    );

    // Round 2 of the same work: the request now opens the interval.
    context
        .edit()
        .append([x3.clone(), x4.clone()])
        .commit()
        .unwrap();
    let second = brief("second brief");
    let blocks = context.blocks().to_vec();
    assert_eq!(segment::head_end(&blocks), 1);
    let two = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(segment::replacements(
                &blocks,
                1,
                4,
                &[1],
                vec![second.clone()],
            )),
        )
        .unwrap();
    assert_eq!(
        ids(context.blocks()),
        ids(&[first.clone(), second.clone(), request.clone(), x4.clone()])
    );
    let summarized =
        |entry: &super::HistoryRecordEntry| BlockStatus::Summarized(entry.record_id.clone());
    let expanded = expand_conversation(context.blocks(), &store.entries(), None, |id| {
        store.read(id)
    });
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<(BlockId, BlockStatus)> = expanded
        .blocks
        .iter()
        .map(|block| (block.block.id(), block.status.clone()))
        .collect();
    assert_eq!(
        shown,
        vec![
            (earlier[0].id(), summarized(&one)),
            (earlier[1].id(), summarized(&one)),
            (request.id(), BlockStatus::Original),
            (x1.id(), summarized(&one)),
            (x2.id(), summarized(&two)),
            (x3.id(), summarized(&two)),
            (x4.id(), BlockStatus::Original),
        ]
    );

    // A later work summarizes the request too.
    context.edit().append([y1.clone()]).commit().unwrap();
    let blocks = context.blocks().to_vec();
    assert_eq!(segment::head_end(&blocks), 2);
    let three = store
        .commit_rewrite(
            &mut context,
            summarize_rewrite(segment::replacements(
                &blocks,
                2,
                5,
                &[],
                vec![brief("third brief")],
            )),
        )
        .unwrap();
    assert_eq!(three.removed_block_count, 3);
    let expanded = expand_conversation(context.blocks(), &store.entries(), None, |id| {
        store.read(id)
    });
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<(BlockId, BlockStatus)> = expanded
        .blocks
        .iter()
        .map(|block| (block.block.id(), block.status.clone()))
        .collect();
    assert_eq!(
        shown,
        vec![
            (earlier[0].id(), summarized(&one)),
            (earlier[1].id(), summarized(&one)),
            (request.id(), summarized(&three)),
            (x1.id(), summarized(&one)),
            (x2.id(), summarized(&two)),
            (x3.id(), summarized(&two)),
            (x4.id(), summarized(&three)),
            (y1.id(), summarized(&three)),
        ]
    );
    let blocks: Vec<ContextBlock> = expanded
        .blocks
        .into_iter()
        .map(|block| block.block)
        .collect();
    let mut full = conversation;
    full.extend([x3, x4, y1]);
    assert_eq!(blocks, full, "every original once, in order");
}

#[test]
fn replacements_put_each_protected_block_back_with_its_own_id() {
    let request = user("request");
    let blocks = vec![
        user("e1"),
        assistant("e2"),
        request.clone(),
        assistant("x1"),
        assistant("x2"),
    ];
    let new_brief = brief("brief");
    let planned = segment::replacements(&blocks, 0, 4, &[2], vec![new_brief.clone()]);
    assert_eq!(
        planned,
        vec![
            PlannedReplacement {
                range: 0..2,
                with: vec![new_brief.clone()],
            },
            PlannedReplacement {
                range: 2..4,
                with: vec![request.clone()],
            },
        ]
    );
    // A protected block that opens the interval comes after the new blocks.
    let planned = segment::replacements(&blocks, 2, 4, &[2], vec![new_brief.clone()]);
    assert_eq!(
        planned,
        vec![PlannedReplacement {
            range: 2..4,
            with: vec![new_brief, request],
        }]
    );
}
