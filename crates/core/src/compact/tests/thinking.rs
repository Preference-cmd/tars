//! Sealed items such as thinking after a rewrite: replies after the change
//! lose them, originals stay in the record, an open tool round is summarized.

use super::super::expansion::{BlockStatus, ExpansionCursor, ExpansionStep, expand_conversation};
use super::super::history::PlannedReplacement;
use super::super::{sealed, segment};
use super::partial::{assistant, brief, call, ids, plan, result, runtime, user};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContentPart, Context, ContextBlock, Role, SealedPart,
    TextPayload,
};

fn sealed_part(thought: &str) -> ContentPart {
    ContentPart::Sealed(SealedPart::new(
        "anthropic_messages",
        serde_json::json!({"type": "thinking", "thinking": thought, "signature": "sig"}),
    ))
}

/// An assistant block that holds only a thinking item.
fn thinking(thought: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![sealed_part(thought)],
        },
        BlockMeta::default(),
    )
}

/// An assistant block with a thinking item followed by text.
fn reply(thought: &str, body: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![
                sealed_part(thought),
                ContentPart::Text(TextPayload::new(body)),
            ],
        },
        BlockMeta::default(),
    )
}

#[tokio::test]
async fn test_be24_a_summary_strips_sealed_items_from_the_replies_it_keeps() {
    let directory = tempfile::tempdir().unwrap();
    // No window: the tail gets the 20k cap, so it starts at the first cut
    // point after the head, `u1`.
    let (runtime, _) = runtime(directory.path(), None);
    let alpha = call("alpha");
    let earlier = "earlier question ".repeat(60);
    let blocks = vec![
        user(&earlier),
        thinking("t0"),
        assistant("a0"),
        user(&earlier),
        assistant("a1 plan"),
        thinking("t1"),
        reply("t1b", "a1"),
        alpha.clone(),
        result(&alpha, "r1"),
        assistant("a2"),
    ];
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    let ranges: Vec<_> = rewrite
        .replacements
        .iter()
        .map(|replacement| replacement.range.clone())
        .collect();
    // The summary, then the plan put back to stand for the thinking-only
    // block after it, then the reply that keeps its text.
    assert_eq!(ranges, vec![0..3, 4..6, 6..7]);
    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();

    let after = context.blocks();
    assert_eq!(
        after.len(),
        7,
        "[brief, u1, plan, a1, call, result, a2]: {after:?}"
    );
    assert!(segment::is_brief(&after[0]));
    assert!(after.iter().all(|block| !sealed::has_sealed(block)));
    assert_eq!(
        after[1..3],
        blocks[3..5],
        "blocks without sealed items are unchanged"
    );
    assert_ne!(
        after[3].id(),
        blocks[6].id(),
        "a stripped reply gets a new id"
    );
    assert_eq!(
        after[3].content(),
        &BlockContent::Message {
            role: Role::Assistant,
            parts: vec![ContentPart::Text(TextPayload::new("a1"))],
        }
    );
    assert_eq!(after[3].meta(), blocks[6].meta());
    assert_eq!(ids(&after[4..]), ids(&blocks[7..]));

    // The originals are in the same record, and the stripped ones count as
    // removed: three summarized, the thinking-only block and the old reply.
    let history = runtime.history().unwrap();
    let record = history.read(&entry.record_id).unwrap();
    assert_eq!(record.replacements[1].originals, blocks[4..6].to_vec());
    assert_eq!(record.replacements[1].inserted_ids, vec![blocks[4].id()]);
    assert_eq!(record.replacements[2].originals, vec![blocks[6].clone()]);
    assert_eq!(entry.removed_block_count, 5);

    // Expanded, the conversation is the original one. The kept replies stay
    // original, since the model still sees them, and only the summary
    // brings a divider, though `u1` separates it from the stripped ones.
    let expanded = expand_conversation(context.blocks(), &runtime.entries(), None, |id| {
        history.read(id)
    });
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<ContextBlock> = expanded
        .blocks
        .iter()
        .map(|block| block.block.clone())
        .collect();
    assert_eq!(shown, blocks);
    let summarized = BlockStatus::Summarized(entry.record_id.clone());
    let statuses: Vec<BlockStatus> = expanded
        .blocks
        .into_iter()
        .map(|block| block.status)
        .collect();
    let mut expected = vec![summarized; 3];
    expected.extend(std::iter::repeat_n(BlockStatus::Original, 7));
    assert_eq!(statuses, expected);

    let mut cursor = ExpansionCursor::new(context.blocks(), runtime.entries());
    let mut marks = 0;
    loop {
        match cursor.next() {
            ExpansionStep::Rewrite(_) => marks += 1,
            ExpansionStep::NeedRecord(id) => {
                let record = history.read(&id).map_err(|error| error.to_string());
                cursor.supply(&id, record);
            }
            ExpansionStep::Unreadable(unreadable) => panic!("{unreadable:?}"),
            ExpansionStep::Block(_) => {}
            ExpansionStep::End => break,
        }
    }
    assert_eq!(marks, 1, "one divider for one record");

    // A later summary covers the put-back plan and the stripped reply: they
    // expand to their originals once, with the later record's status.
    let rewrite = plan(&runtime, &context).await.unwrap();
    let later = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    assert_eq!(context.blocks().len(), 3, "[S1, S2, a2]");
    let expanded = expand_conversation(context.blocks(), &runtime.entries(), None, |id| {
        history.read(id)
    });
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<ContextBlock> = expanded
        .blocks
        .iter()
        .map(|block| block.block.clone())
        .collect();
    assert_eq!(shown, blocks);
    let statuses: Vec<BlockStatus> = expanded
        .blocks
        .into_iter()
        .map(|block| block.status)
        .collect();
    let mut expected = vec![BlockStatus::Summarized(entry.record_id.clone()); 3];
    expected.extend(std::iter::repeat_n(
        BlockStatus::Summarized(later.record_id.clone()),
        6,
    ));
    expected.push(BlockStatus::Original);
    assert_eq!(statuses, expected);
}

#[test]
fn test_be24_replies_before_the_first_change_keep_their_sealed_items() {
    let before = reply("t0", "a0");
    let a1 = assistant("a1");
    let blocks = vec![
        user("u0"),
        before.clone(),
        user("u1"),
        thinking("t1"),
        a1.clone(),
        thinking("t2"),
    ];
    let new_brief = brief("b");
    let planned = sealed::strip_after(
        &blocks,
        2,
        vec![PlannedReplacement {
            range: 2..3,
            with: vec![new_brief.clone()],
        }],
    );
    // The reply before the change is untouched. A thinking-only block right
    // after a range joins it; one after an unchanged block puts that block
    // back.
    assert_eq!(
        planned,
        vec![
            PlannedReplacement {
                range: 2..4,
                with: vec![new_brief],
            },
            PlannedReplacement {
                range: 4..6,
                with: vec![a1.clone()],
            },
        ]
    );
    assert!(
        planned
            .iter()
            .all(|replacement| replacement.range.start >= 2)
    );

    // Two consecutive thinking-only blocks right after a range join it.
    let b = brief("b");
    let consecutive_after_range = sealed::strip_after(
        &[user("u0"), thinking("t0"), thinking("t1"), assistant("a")],
        0,
        vec![PlannedReplacement {
            range: 0..1,
            with: vec![b.clone()],
        }],
    );
    assert_eq!(
        consecutive_after_range,
        vec![PlannedReplacement {
            range: 0..3,
            with: vec![b.clone()],
        }]
    );

    // Two consecutive thinking-only blocks after an unchanged block form a
    // put-back range and join it.
    let consecutive_after_unchanged = sealed::strip_after(
        &[user("u0"), a1.clone(), thinking("t1"), thinking("t2")],
        0,
        vec![PlannedReplacement {
            range: 0..1,
            with: vec![b.clone()],
        }],
    );
    assert_eq!(
        consecutive_after_unchanged,
        vec![
            PlannedReplacement {
                range: 0..1,
                with: vec![b],
            },
            PlannedReplacement {
                range: 1..4,
                with: vec![a1],
            },
        ]
    );
}

#[test]
fn test_be24_an_open_tool_round_with_sealed_items_stays_out_of_the_tail() {
    let make = |think: bool, closed: bool| {
        let alpha = call("alpha");
        let mut blocks = vec![
            user("u0"),
            assistant("a0"),
            user("request"),
            if think {
                thinking("t")
            } else {
                assistant("plan")
            },
            alpha.clone(),
            result(&alpha, "r"),
        ];
        if closed {
            blocks.push(assistant("answer"));
        }
        blocks
    };
    // The round in progress has thinking: the tail starts after it, so it
    // is empty here.
    let open = make(true, false);
    assert_eq!(sealed::tail_start_outside_open_loop(&open, 2), open.len());
    // No sealed items in the open round, or the round is closed: k stands.
    assert_eq!(
        sealed::tail_start_outside_open_loop(&make(false, false), 2),
        2
    );
    assert_eq!(
        sealed::tail_start_outside_open_loop(&make(true, true), 2),
        2
    );

    // Thinking only in an earlier round of the open loop: the tail starts at
    // the first cut point after it and keeps the later round.
    let first = call("alpha");
    let second = call("beta");
    let blocks = vec![
        user("request"),
        thinking("t"),
        first.clone(),
        result(&first, "r1"),
        second.clone(),
        result(&second, "r2"),
    ];
    assert_eq!(sealed::tail_start_outside_open_loop(&blocks, 1), 4);
    assert_eq!(sealed::tail_start_outside_open_loop(&blocks, 4), 4);
}

#[tokio::test]
async fn test_be24_a_summary_in_an_open_round_with_thinking_keeps_only_the_request() {
    let directory = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(directory.path(), None);
    let alpha = call("alpha");
    let request = user("request");
    let earlier = "earlier question ".repeat(60);
    let blocks = vec![
        user(&earlier),
        assistant("a0"),
        request.clone(),
        thinking("t"),
        alpha.clone(),
        result(&alpha, "r"),
    ];
    runtime.set_current_work_inputs(std::slice::from_ref(&request));
    let mut context = Context::from_blocks(blocks).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();
    assert_eq!(after.len(), 2, "[brief, request]: {after:?}");
    assert!(segment::is_brief(&after[0]));
    assert_eq!(after[1], request);
}

#[tokio::test]
async fn test_be24_a_thinking_only_reply_after_a_tool_result_puts_the_result_back() {
    let directory = tempfile::tempdir().unwrap();
    // No window: the tail gets the 20k cap, so it starts at the first cut
    // point after the head, `request`. Earlier text fills the summary interval.
    let (runtime, _) = runtime(directory.path(), None);
    let alpha = call("alpha");
    let alpha_res = result(&alpha, "r1");
    let beta = call("beta");
    let beta_res = result(&beta, "r2");
    let t1 = thinking("t1");
    let t2 = thinking("t2");
    let earlier = "earlier question ".repeat(60);
    let blocks = vec![
        user(&earlier),
        assistant("a0"),
        user("request"),
        t1.clone(),
        alpha.clone(),
        alpha_res.clone(),
        t2.clone(),
        beta.clone(),
        beta_res.clone(),
        assistant("answer"),
    ];
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    let ranges: Vec<_> = rewrite
        .replacements
        .iter()
        .map(|replacement| replacement.range.clone())
        .collect();
    assert_eq!(ranges, vec![0..2, 2..4, 5..7]);

    let entry = runtime.commit_rewrite(&mut context, rewrite).unwrap();

    let after = context.blocks();
    assert_eq!(
        after.len(),
        7,
        "[brief, request, call alpha, result alpha, call beta, result beta, answer]: {after:?}"
    );
    assert!(segment::is_brief(&after[0]));

    // After commit, no block in the Context has sealed parts.
    assert!(after.iter().all(|block| !sealed::has_sealed(block)));

    // Every ToolCall is directly followed by its ToolResult, and the put-back
    // tool result keeps its id.
    for (i, block) in after.iter().enumerate() {
        if let BlockContent::ToolCall(_) = block.content() {
            let next = &after[i + 1];
            match next.content() {
                BlockContent::ToolResult(res) => {
                    assert_eq!(res.call_block_id, block.id());
                }
                _ => panic!("expected ToolResult after ToolCall at index {i}"),
            }
        }
    }
    assert_eq!(
        after[3].id(),
        alpha_res.id(),
        "put-back result keeps its id"
    );

    // The record's replacements contain one whose inserted ids are exactly the
    // alpha result's id and whose originals are [result alpha, thinking t2].
    let history = runtime.history().unwrap();
    let record = history.read(&entry.record_id).unwrap();
    let put_back = record
        .replacements
        .iter()
        .find(|replacement| replacement.inserted_ids == vec![alpha_res.id()])
        .expect("replacement putting back alpha result");
    assert_eq!(put_back.originals, vec![alpha_res.clone(), t2.clone()]);

    // Expanding the conversation returns exactly the original blocks in order,
    // with no unreadable records, and the blocks outside the summarized interval
    // are BlockStatus::Original.
    let expanded = expand_conversation(context.blocks(), &runtime.entries(), None, |id| {
        history.read(id)
    });
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<ContextBlock> = expanded
        .blocks
        .iter()
        .map(|block| block.block.clone())
        .collect();
    assert_eq!(shown, blocks);

    let summarized = BlockStatus::Summarized(entry.record_id.clone());
    let statuses: Vec<BlockStatus> = expanded
        .blocks
        .into_iter()
        .map(|block| block.status)
        .collect();
    let mut expected = vec![summarized; 2];
    expected.extend(std::iter::repeat_n(BlockStatus::Original, 8));
    assert_eq!(statuses, expected);
}
