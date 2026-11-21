//! Temporary frame trimming: cut points, tool declarations, middle system
//! blocks (BE27), sealed items (BE24), protected inputs and minimal fallback.

use super::super::sealed;
use super::super::segment;
use super::super::trim::trim_frame;
use super::partial::{assistant, brief, call, ids, result, system, user};
use super::{big_block, tool_change};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContentPart, ContextBlock, Role, SealedPart, TextPayload,
    ToolChange, current_tools,
};
use std::collections::HashSet;

fn sealed_part(thought: &str) -> ContentPart {
    ContentPart::Sealed(SealedPart::new(
        "anthropic_messages",
        serde_json::json!({"type": "thinking", "thinking": thought, "signature": "sig"}),
    ))
}

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

#[test]
fn test_be18_fits_window_returns_none_and_never_trims() {
    let blocks = vec![
        system("rules"),
        user(&"u0 ".repeat(40)),
        assistant("a0"),
        user("u1"),
    ];
    let est = segment::blocks_estimate(&blocks);
    assert!(trim_frame(&blocks, est + 100, Some(100), est, &HashSet::new()).is_none());
    assert!(trim_frame(&blocks, est, None, est, &HashSet::new()).is_none());
}

#[test]
fn test_be18_trims_oldest_intervals_by_cut_points_and_keeps_head_and_protected() {
    let sys = system("system rules");
    let chain = brief("earlier brief");
    let c1 = call("alpha");
    let c2 = call("beta");
    let r1 = result(&c1, &"r1 ".repeat(100));
    let r2 = result(&c2, &"r2 ".repeat(100));
    let u_old = big_block(900);
    let u_cur = user("current request U");
    let c3 = call("gamma");
    let r3 = result(&c3, "r3 ok");
    let blocks = vec![
        sys.clone(),
        chain.clone(),
        u_old,
        c1,
        c2,
        r1,
        r2,
        u_cur.clone(),
        c3.clone(),
        r3.clone(),
    ];
    let protected: HashSet<BlockId> = [u_cur.id()].into_iter().collect();
    let est = segment::blocks_estimate(&blocks);
    let tail_only = segment::blocks_estimate(&[
        sys.clone(),
        chain.clone(),
        u_cur.clone(),
        c3.clone(),
        r3.clone(),
    ]);
    let window = tail_only + 200;
    let trimmed = trim_frame(&blocks, window, Some(100), est, &protected)
        .expect("exceeds window so must trim");
    assert!(trimmed.trimmed);
    assert!(trimmed.after_tokens + 100 <= window);
    assert_eq!(
        ids(&trimmed.frame.blocks),
        vec![sys.id(), chain.id(), u_cur.id(), c3.id(), r3.id()],
        "head and protected U and newest tool round stay; parallel old calls were dropped together"
    );

    // Even when cutting mid-round around U, U stays protected while older
    // tool rounds after U are dropped at a cut point.
    let c4 = call("delta");
    let r4 = result(&c4, "r4 small");
    let blocks2 = vec![
        sys.clone(),
        u_cur.clone(),
        c1_clone(&blocks[3]),
        blocks[5].clone(),
        c4.clone(),
        r4.clone(),
    ];
    let est2 = segment::blocks_estimate(&blocks2);
    let min_keep = segment::blocks_estimate(&[sys.clone(), u_cur.clone(), c4.clone(), r4.clone()]);
    let trimmed2 = trim_frame(&blocks2, min_keep + 50, None, est2, &protected).unwrap();
    assert_eq!(
        ids(&trimmed2.frame.blocks),
        vec![sys.id(), u_cur.id(), c4.id(), r4.id()],
        "protected U stays before kept newer tool round"
    );
}

fn c1_clone(b: &ContextBlock) -> ContextBlock {
    b.clone()
}

#[test]
fn test_be18_never_splits_call_and_result_or_parallel_calls() {
    let sys = system("sys");
    let u0 = user("u0");
    let c1 = call("alpha");
    let c2 = call("beta");
    let r1 = result(&c1, &"x".repeat(600));
    let r2 = result(&c2, &"y".repeat(600));
    let u1 = user("u1");
    let a1 = assistant("a1");
    let blocks = vec![sys.clone(), u0, c1, c2, r1, r2, u1.clone(), a1.clone()];
    let est = segment::blocks_estimate(&blocks);
    // Budget allows [sys, r2, u1, a1] by bytes, but r2 cannot be separated
    // from c1/c2/r1, so trimming must advance to u1.
    let budget_for_r2_and_tail =
        segment::blocks_estimate(&[sys.clone(), blocks[5].clone(), u1.clone(), a1.clone()]);
    let trimmed = trim_frame(
        &blocks,
        budget_for_r2_and_tail + 20,
        None,
        est,
        &HashSet::new(),
    )
    .expect("must trim");
    assert_eq!(ids(&trimmed.frame.blocks), vec![sys.id(), u1.id(), a1.id()]);
}

#[test]
fn test_be18_folds_dropped_tool_changes_and_passes_bound_tools_check() {
    let sys = system("sys");
    let u0 = big_block(900);
    let tc1 = tool_change("alpha");
    let a0 = assistant("a0");
    let tc2 = tool_change("beta");
    let u1 = user("u1");
    let a1 = assistant("a1");
    let blocks = vec![sys.clone(), u0, tc1, a0, tc2, u1.clone(), a1.clone()];
    let est = segment::blocks_estimate(&blocks);
    let trimmed = trim_frame(&blocks, est - 100, None, est, &HashSet::new()).unwrap();
    assert!(trimmed.trimmed);
    let bound = current_tools(&blocks);
    assert!(ToolChange::between(&current_tools(&trimmed.frame.blocks), &bound).is_empty());
    assert_eq!(trimmed.frame.blocks[0].id(), sys.id());
    assert!(matches!(
        trimmed.frame.blocks[1].content(),
        BlockContent::ToolChange(_)
    ));
    assert_eq!(trimmed.frame.blocks[2].id(), u1.id());
    assert_eq!(trimmed.frame.blocks[3].id(), a1.id());
}

#[test]
fn test_be27_middle_system_block_dropped_when_adjacent_to_fixed_prefix_and_kept_after_brief() {
    let sys = system("leading rules");
    let u0 = big_block(900);
    let c0 = call("alpha");
    let r0 = result(&c0, "done");
    let note = system("repair note: unpaired tool call removed");
    let u1 = user("u1");
    let a1 = assistant("a1");

    // No brief in head: dropping [u0, c0, r0] would put `note` directly after `sys`,
    // so `note` must also be dropped.
    let no_brief = vec![
        sys.clone(),
        u0.clone(),
        c0.clone(),
        r0.clone(),
        note.clone(),
        u1.clone(),
        a1.clone(),
    ];
    let est = segment::blocks_estimate(&no_brief);
    let window =
        segment::blocks_estimate(&[sys.clone(), note.clone(), u1.clone(), a1.clone()]) + 50;
    let trimmed = trim_frame(&no_brief, window, None, est, &HashSet::new()).unwrap();
    assert_eq!(
        ids(&trimmed.frame.blocks),
        vec![sys.id(), u1.id(), a1.id()],
        "middle system note must be dropped when its preceding conversation was dropped"
    );
    assert_eq!(segment::fixed_prefix_end(&trimmed.frame.blocks), 1);

    // Same without any leading system prompt at all: `note` must not become a leading system prompt.
    let no_leading_sys = vec![
        u0.clone(),
        c0.clone(),
        r0.clone(),
        note.clone(),
        u1.clone(),
        a1.clone(),
    ];
    let est_no_sys = segment::blocks_estimate(&no_leading_sys);
    let win_no_sys = segment::blocks_estimate(&[note.clone(), u1.clone(), a1.clone()]) + 50;
    let trimmed_no_sys = trim_frame(
        &no_leading_sys,
        win_no_sys,
        None,
        est_no_sys,
        &HashSet::new(),
    )
    .unwrap();
    assert_eq!(ids(&trimmed_no_sys.frame.blocks), vec![u1.id(), a1.id()]);
    assert_eq!(segment::fixed_prefix_end(&trimmed_no_sys.frame.blocks), 0);

    // With a brief in the head, `note` follows the brief (a User message), so it stays.
    let b = brief("brief");
    let with_brief = vec![
        sys.clone(),
        b.clone(),
        u0,
        c0,
        r0,
        note.clone(),
        u1.clone(),
        a1.clone(),
    ];
    let est_brief = segment::blocks_estimate(&with_brief);
    let win_brief =
        segment::blocks_estimate(&[sys.clone(), b.clone(), note.clone(), u1.clone(), a1.clone()])
            + 50;
    let trimmed_brief =
        trim_frame(&with_brief, win_brief, None, est_brief, &HashSet::new()).unwrap();
    assert_eq!(
        ids(&trimmed_brief.frame.blocks),
        vec![sys.id(), b.id(), note.id(), u1.id(), a1.id()]
    );
    assert_eq!(segment::fixed_prefix_end(&trimmed_brief.frame.blocks), 1);
}

#[test]
fn test_be18_be24_strips_sealed_items_after_first_change_and_omits_thinking_only_blocks() {
    let sys = system("sys");
    let u0 = big_block(900);
    let a0 = reply("t0", "a0");
    let u1 = user("u1");
    let t1 = thinking("t1");
    let c1 = call("alpha");
    let r1 = result(&c1, "r1");
    let t2 = thinking("t2");
    let a1 = reply("t3", "final answer");
    let blocks = vec![
        sys.clone(),
        u0,
        a0,
        u1.clone(),
        t1,
        c1.clone(),
        r1.clone(),
        t2,
        a1.clone(),
    ];
    let est = segment::blocks_estimate(&blocks);
    let trimmed = trim_frame(&blocks, est - 100, None, est, &HashSet::new()).unwrap();
    assert!(trimmed.trimmed);
    let kept = &trimmed.frame.blocks;
    // Expect: [sys, u1, c1, r1, a1_stripped] (t1 and t2 omitted; a1 stripped of sealed)
    assert_eq!(kept.len(), 5, "{kept:?}");
    assert_eq!(kept[0].id(), sys.id());
    assert_eq!(kept[1].id(), u1.id());
    assert_eq!(kept[2].id(), c1.id());
    assert_eq!(kept[3].id(), r1.id());
    assert_ne!(kept[4].id(), a1.id());
    assert!(!sealed::has_sealed(&kept[4]));
    assert_eq!(
        kept[4].content(),
        &BlockContent::Message {
            role: Role::Assistant,
            parts: vec![ContentPart::Text(TextPayload::new("final answer"))],
        }
    );
    assert!(kept.iter().all(|block| !sealed::has_sealed(block)));
}

#[test]
fn test_be18_minimal_frame_emitted_when_nothing_fits() {
    let sys = system(&"s".repeat(600));
    let u0 = big_block(600);
    let a0 = assistant("a0");
    let u1 = big_block(600);
    let blocks = vec![sys.clone(), u0.clone(), a0.clone(), u1.clone()];
    let protected: HashSet<BlockId> = [u1.id()].into_iter().collect();
    let est = segment::blocks_estimate(&blocks);
    // Window is smaller than [sys, u1] alone.
    let trimmed = trim_frame(&blocks, 100, Some(50), est, &protected).unwrap();
    assert!(trimmed.trimmed);
    assert_eq!(ids(&trimmed.frame.blocks), vec![sys.id(), u1.id()]);

    // With no protected blocks and no brief in head, minimal fallback must not
    // emit an empty frame or a system-only frame: it falls back to the
    // smallest valid candidate ([sys, u1]) or returns None when none is smaller.
    let no_sys = vec![u0.clone(), a0.clone(), u1.clone()];
    let est_no_sys = segment::blocks_estimate(&no_sys);
    let fallback_no_sys = trim_frame(&no_sys, 10, None, est_no_sys, &HashSet::new()).unwrap();
    assert!(fallback_no_sys.trimmed);
    assert_eq!(ids(&fallback_no_sys.frame.blocks), vec![u1.id()]);

    let with_sys = vec![sys.clone(), u0.clone(), a0.clone(), u1.clone()];
    let est_with_sys = segment::blocks_estimate(&with_sys);
    let fallback_with_sys = trim_frame(&with_sys, 10, None, est_with_sys, &HashSet::new()).unwrap();
    assert!(fallback_with_sys.trimmed);
    assert_eq!(
        ids(&fallback_with_sys.frame.blocks),
        vec![sys.id(), u1.id()]
    );

    let single_turn = vec![sys, u0, a0];
    let est_single = segment::blocks_estimate(&single_turn);
    assert!(trim_frame(&single_turn, 10, None, est_single, &HashSet::new()).is_none());
}

fn prelude_block(role: Role, source: &str, text: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(role, text),
        BlockMeta::from_source(source),
    )
}

#[test]
fn test_si04_trim_frame_protects_latest_prelude_updates_and_preserves_be27() {
    use crate::prelude::{
        BASE_HOST_CONTRACT, DynamicSection, PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE,
        PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
    };

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let ws_v1 = prelude_block(
        Role::User,
        PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
        "<workspace_instructions>\nRule v1\n</workspace_instructions>",
    );
    let env_v1 = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\npermission_mode: ask\n</env>",
    );

    let u0 = big_block(700);
    let env_v2 = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\npermission_mode: accept-edits\n</env>",
    );
    let ws_removed = prelude_block(
        Role::User,
        DynamicSection::WorkspaceInstructions.update_source(),
        "<workspace_instructions removed=\"true\" />",
    );
    let a0 = assistant("a0");
    let u1 = big_block(700);
    let env_v3 = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\npermission_mode: read-only\n</env>",
    );
    let a1 = assistant("a1");
    let middle_sys = system("middle system repair note");
    let u_cur = user("current question");

    // Case 1: `head_end..restart` drops [u0, env_v2, ws_removed, a0, u1, env_v3, a1, middle_sys]
    // and keeps `u_cur`. Both `env_v3` (latest for Env) and `ws_removed` (latest for WorkspaceInstructions)
    // must be protected, while `env_v2` is dropped. Also `middle_sys` is never promoted to leading system.
    let blocks = vec![
        base.clone(),
        ws_v1.clone(),
        env_v1.clone(),
        u0.clone(),
        env_v2.clone(),
        ws_removed.clone(),
        a0.clone(),
        u1.clone(),
        env_v3.clone(),
        a1.clone(),
        middle_sys.clone(),
        u_cur.clone(),
    ];
    assert_eq!(segment::leading_system_end(&blocks), 1);
    assert_eq!(segment::fixed_prefix_end(&blocks), 3);

    let est = segment::blocks_estimate(&blocks);
    let protected: HashSet<BlockId> = [u_cur.id()].into_iter().collect();
    let keep_budget = segment::blocks_estimate(&[
        base.clone(),
        ws_v1.clone(),
        env_v1.clone(),
        ws_removed.clone(),
        env_v3.clone(),
        u_cur.clone(),
    ]);
    let trimmed = trim_frame(&blocks, keep_budget + 50, None, est, &protected).unwrap();
    assert!(trimmed.trimmed);
    assert_eq!(
        ids(&trimmed.frame.blocks),
        vec![
            base.id(),
            ws_v1.id(),
            env_v1.id(),
            ws_removed.id(),
            env_v3.id(),
            u_cur.id()
        ],
        "only latest update per section in dropped interval is protected; middle system stays dropped"
    );
    assert!(
        !trimmed
            .frame
            .blocks
            .iter()
            .any(|b| b.id() == middle_sys.id())
    );
    assert!(!trimmed.frame.blocks.iter().any(|b| b.id() == env_v2.id()));

    // Case 2: When `restart..` already contains a newer update (`env_v4`) for Env,
    // the older `env_v2` / `env_v3` in `head_end..restart` is NOT protected.
    let env_v4 = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\npermission_mode: full-access\n</env>",
    );
    let blocks2 = vec![
        base.clone(),
        ws_v1.clone(),
        env_v1.clone(),
        u0,
        env_v2.clone(),
        ws_removed.clone(),
        a0,
        env_v4.clone(),
        u_cur.clone(),
    ];
    let est2 = segment::blocks_estimate(&blocks2);
    let keep_budget2 = segment::blocks_estimate(&[
        base.clone(),
        ws_v1.clone(),
        env_v1.clone(),
        ws_removed.clone(),
        env_v4.clone(),
        u_cur.clone(),
    ]);
    let trimmed2 = trim_frame(&blocks2, keep_budget2 + 50, None, est2, &protected).unwrap();
    assert_eq!(
        ids(&trimmed2.frame.blocks),
        vec![
            base.id(),
            ws_v1.id(),
            env_v1.id(),
            ws_removed.id(),
            env_v4.id(),
            u_cur.id()
        ],
        "env_v2 is not protected when env_v4 is already in kept tail"
    );
}

#[test]
fn test_si04_be27_rejects_middle_system_after_prelude_or_without_dynamic_prefix() {
    use crate::prelude::{BASE_HOST_CONTRACT, PRELUDE_BASE_SOURCE};

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let u0 = big_block(600);
    let a0 = assistant("a0");
    let middle_sys = system("middle repair note");
    let u_cur = user("current");

    let blocks = vec![base.clone(), u0, a0, middle_sys.clone(), u_cur.clone()];
    let est = segment::blocks_estimate(&blocks);
    let protected: HashSet<BlockId> = [u_cur.id()].into_iter().collect();
    let min_keep = segment::blocks_estimate(&[base.clone(), u_cur.clone()]);
    let trimmed = trim_frame(&blocks, min_keep + 10, None, est, &protected).unwrap();
    assert_eq!(ids(&trimmed.frame.blocks), vec![base.id(), u_cur.id()]);
    assert_eq!(segment::leading_system_end(&trimmed.frame.blocks), 1);
}
