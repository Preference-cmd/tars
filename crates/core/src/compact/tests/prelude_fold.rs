//! Compaction folding of dynamic prelude updates, fixed-prefix boundary
//! semantics, window budget re-tightening, and history expansion.

use super::super::expansion::{BlockStatus, ExpansionCursor, ExpansionStep, expand_conversation};
use super::super::segment;
use super::partial::{assistant, plan, runtime, user};
use super::{big_block, tool_change};
use crate::prelude::{
    BASE_HOST_CONTRACT, DynamicSection, PRELUDE_BASE_SOURCE, PRELUDE_DYNAMIC_CUSTOM_SOURCE,
    PRELUDE_ENV_SOURCE, PRELUDE_SKILLS_SOURCE, PRELUDE_STATIC_CUSTOM_SOURCE,
    PRELUDE_USER_INSTRUCTIONS_SOURCE, PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE, PreludeBudgetConfig,
    PreludeDefinition, PreludeRuntimeState, effective_section_contents, evaluate_dynamic_updates,
};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContentPart, Context, ContextBlock, Role, current_tools,
};
use std::collections::BTreeSet;

fn prelude_block(role: Role, source: &str, text: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(role, text),
        BlockMeta::from_source(source),
    )
}

fn text_of(block: &ContextBlock) -> String {
    match block.content() {
        BlockContent::Message { parts, .. } => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text(t) => Some(t.0.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

#[test]
fn test_si04_leading_system_end_vs_fixed_prefix_end_and_estimate() {
    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let custom = prelude_block(Role::System, PRELUDE_STATIC_CUSTOM_SOURCE, "static custom");
    let tc = tool_change("alpha");
    let dyn_ws = prelude_block(
        Role::User,
        PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
        "<workspace_instructions>\nRule v1\n</workspace_instructions>",
    );
    let dyn_env = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\npermission_mode: ask\n</env>",
    );
    let q1 = user("first question");
    let upd_env = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\npermission_mode: accept-edits\n</env>",
    );

    let blocks = vec![
        base.clone(),
        custom.clone(),
        tc,
        dyn_ws.clone(),
        dyn_env.clone(),
        q1,
        upd_env,
    ];
    assert_eq!(segment::leading_system_end(&blocks), 2);
    assert_eq!(segment::fixed_prefix_end(&blocks), 5);
    assert_eq!(segment::head_end(&blocks), 5);

    let sys_and_tools_only = segment::fixed_prefix_estimate(&[base, custom, tool_change("alpha")]);
    let full_estimate = segment::fixed_prefix_estimate(&blocks);
    assert!(
        full_estimate > sys_and_tools_only,
        "fixed_prefix_estimate must include dynamic prelude blocks"
    );
}

#[tokio::test]
async fn test_si04_compact_without_updates_keeps_prefix_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(dir.path(), None);

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let env = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\npermission_mode: ask\n</env>",
    );
    let u0 = big_block(1200);
    let a0 = assistant("a0");
    let u1 = user("u1");

    let blocks = vec![base.clone(), env.clone(), u0, a0, u1.clone()];
    let mut context = Context::from_blocks(blocks).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    assert_eq!(rewrite.replacements.len(), 1);
    assert_eq!(rewrite.replacements[0].range, 2..4);
    runtime.commit_rewrite(&mut context, rewrite).unwrap();

    let after = context.blocks();
    assert_eq!(after[0].id(), base.id());
    assert_eq!(after[1].id(), env.id());
    assert_eq!(after[3].id(), u1.id());
}

#[tokio::test]
async fn test_si04_compact_folds_updates_inserts_new_removes_tombstones_and_expands_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    // 10K window -> trigger_line = 7,000 -> retained_tail_budget ~1,400 tokens.
    // Making a0/a1 5,000 bytes (~1,667 tokens) ensures the cut point k lands after them,
    // so all prelude updates in the turn fall inside `head_end..k`.
    let (runtime, _) = runtime(dir.path(), Some(10_000));

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let static_custom = prelude_block(Role::System, PRELUDE_STATIC_CUSTOM_SOURCE, "static custom");
    let ws_v1 = prelude_block(
        Role::User,
        PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
        "<workspace_instructions>\nWS v1\n</workspace_instructions>",
    );
    let env_v1 = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\nplatform: linux\nworkspace_root: (none)\npermission_mode: ask\ncapabilities: (none)\n</env>",
    );
    let dyn_v1 = prelude_block(
        Role::User,
        PRELUDE_DYNAMIC_CUSTOM_SOURCE,
        "<dynamic_context>\nDyn v1\n</dynamic_context>",
    );

    let tc = tool_change("alpha");
    let u0 = big_block(1200);
    // Add a new section (`UserInstructions`) that was absent at creation,
    // update `Env` twice (latest wins), and remove `DynamicCustom`.
    let upd_user_instr = prelude_block(
        Role::User,
        DynamicSection::UserInstructions.update_source(),
        "<user_instructions updated=\"true\">\nUser rule v1\n</user_instructions>",
    );
    let upd_env_mid = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\nplatform: linux\nworkspace_root: (none)\npermission_mode: read-only\ncapabilities: (none)\n</env>",
    );
    let upd_env_latest = prelude_block(
        Role::User,
        DynamicSection::Env.update_source(),
        "<env updated=\"true\">\nplatform: linux\nworkspace_root: (none)\npermission_mode: accept-edits\ncapabilities: (none)\n</env>",
    );
    let upd_dyn_removed = prelude_block(
        Role::User,
        DynamicSection::DynamicCustom.update_source(),
        "<dynamic_context removed=\"true\" />",
    );
    let a0 = big_block(5_000);
    let u_tail = user("tail question");

    let initial_blocks = vec![
        base.clone(),
        static_custom.clone(),
        ws_v1.clone(),
        env_v1.clone(),
        dyn_v1.clone(),
        tc,
        u0.clone(),
        upd_user_instr.clone(),
        upd_env_mid.clone(),
        upd_env_latest.clone(),
        upd_dyn_removed.clone(),
        a0.clone(),
        u_tail.clone(),
    ];
    let tools_before = current_tools(&initial_blocks);
    let mut context = Context::from_blocks(initial_blocks.clone()).unwrap();

    let rewrite = plan(&runtime, &context).await.unwrap();
    // Must contain two ranges: dynamic prefix fold (2..5) and summary (6..12, since tc at 5 is in head).
    assert_eq!(rewrite.replacements.len(), 2);
    assert_eq!(rewrite.replacements[0].range, 2..5);
    assert_eq!(rewrite.replacements[1].range, 6..12);

    let r1 = runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();

    // Static Role::System blocks keep exact IDs and content; tools stay identical.
    assert_eq!(after[0].id(), base.id());
    assert_eq!(after[1].id(), static_custom.id());
    assert_eq!(current_tools(after), tools_before);

    // Dynamic prefix order: UserInstructions (inserted in order!), WorkspaceInstructions (kept),
    // Env (updated, canonical tag without updated="true"); DynamicCustom was removed!
    assert_eq!(
        after[2].meta().source.as_deref(),
        Some(PRELUDE_USER_INSTRUCTIONS_SOURCE)
    );
    assert_eq!(
        text_of(&after[2]),
        "<user_instructions>\nUser rule v1\n</user_instructions>"
    );
    assert_eq!(
        after[3].meta().source.as_deref(),
        Some(PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE)
    );
    assert_eq!(
        text_of(&after[3]),
        "<workspace_instructions>\nWS v1\n</workspace_instructions>"
    );
    assert_eq!(after[4].meta().source.as_deref(), Some(PRELUDE_ENV_SOURCE));
    assert!(text_of(&after[4]).contains("<env>\n"));
    assert!(!text_of(&after[4]).contains("updated=\"true\""));
    assert!(text_of(&after[4]).contains("permission_mode: accept-edits"));

    assert_eq!(segment::fixed_prefix_end(after), 5);
    assert!(matches!(after[5].content(), BlockContent::ToolChange(_)));
    assert!(segment::is_brief(&after[6]));
    assert_eq!(after[7].id(), u_tail.id());

    // Effective sections match canonical state, and next turn emits 0 updates when nothing changed.
    let eff = effective_section_contents(after);
    assert_eq!(
        eff[&DynamicSection::UserInstructions].as_deref(),
        Some("<user_instructions>\nUser rule v1\n</user_instructions>")
    );
    assert_eq!(eff[&DynamicSection::DynamicCustom], None);

    let ws_temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws_temp.path().join("cfg")).unwrap();
    std::fs::write(ws_temp.path().join("cfg/AGENTS.md"), "User rule v1").unwrap();
    let p_def = PreludeDefinition {
        config_dir: Some(ws_temp.path().join("cfg")),
        ..Default::default()
    };
    let p_state = PreludeRuntimeState {
        workspace_root: None,
        permission_mode: crate::permission::PermissionMode::AcceptEdits,
        capabilities: BTreeSet::new(),
        platform: "linux".into(),
    };
    // WorkspaceInstructions came from original ws_v1 without ws_root file -> check Env + UserInstructions stay 0.
    let update = evaluate_dynamic_updates(after, &p_def, &p_state, &PreludeBudgetConfig::default());
    assert!(
        update.blocks.iter().all(|b| b.meta().source.as_deref()
            != Some(DynamicSection::Env.update_source())
            && b.meta().source.as_deref()
                != Some(DynamicSection::UserInstructions.update_source())),
        "folded Env and UserInstructions must emit zero duplicate updates: {:?}",
        update.blocks
    );

    // Verify ExpansionCursor: 1 divider only (not at prelude!), and v1 dynamic prefix blocks stay Original.
    let history = runtime.history().unwrap();
    let mut cursor = ExpansionCursor::new(after, runtime.entries());
    let mut marks = 0;
    loop {
        match cursor.next() {
            ExpansionStep::Rewrite(mark) => {
                marks += 1;
                assert_eq!(mark.record_id, r1.record_id);
            }
            ExpansionStep::NeedRecord(id) => {
                cursor.supply(&id, history.read(&id).map_err(|e| e.to_string()));
            }
            ExpansionStep::Unreadable(err) => panic!("{err:?}"),
            ExpansionStep::Block(_) => {}
            ExpansionStep::End => break,
        }
    }
    assert_eq!(marks, 1, "must emit exactly one compaction divider");

    let expanded = expand_conversation(after, &runtime.entries(), None, |id| history.read(id));
    assert!(expanded.unreadable_records.is_empty());
    let restored_blocks: Vec<ContextBlock> =
        expanded.blocks.iter().map(|b| b.block.clone()).collect();
    assert_eq!(restored_blocks, initial_blocks);
    // Blocks 0..6 (static + v1 dynamic prefix + head ToolChange) must all be BlockStatus::Original.
    assert!(
        expanded.blocks[..6]
            .iter()
            .all(|b| b.status == BlockStatus::Original),
        "v1 prelude blocks must keep Original status: {:?}",
        &expanded.blocks[..6]
    );
    // Conversation blocks 6..12 (u0, updates, a0) must be Summarized(r1).
    let expected_sum = BlockStatus::Summarized(r1.record_id.clone());
    assert!(
        expanded.blocks[6..12]
            .iter()
            .all(|b| b.status == expected_sum)
    );
    assert_eq!(expanded.blocks[12].status, BlockStatus::Original);

    // Second compaction WITH an update, keeping brief_1 in the chain (two separate runs in ExpansionCursor!).
    let u1_big = big_block(1200);
    let upd_skills = prelude_block(
        Role::User,
        DynamicSection::Skills.update_source(),
        "<available_skills updated=\"true\">\n  <skill><name>s1</name><description>d1</description></skill>\n</available_skills>",
    );
    let a1 = big_block(5_000);
    let u2 = user("latest");
    context
        .edit()
        .append(vec![
            u1_big.clone(),
            upd_skills.clone(),
            a1.clone(),
            u2.clone(),
        ])
        .commit()
        .unwrap();

    let rewrite2 = plan(&runtime, &context).await.unwrap();
    let r2 = runtime.commit_rewrite(&mut context, rewrite2).unwrap();
    let after2 = context.blocks();
    assert_eq!(after2[0].id(), base.id());
    assert_eq!(after2[1].id(), static_custom.id());
    assert_eq!(
        after2[4].meta().source.as_deref(),
        Some(PRELUDE_SKILLS_SOURCE)
    );

    // Now ExpansionCursor hits brief_2 (R2), then brief_1 (R1), then the folded prelude (R2 -> R1)!
    let mut cursor2 = ExpansionCursor::new(after2, runtime.entries());
    let mut mark_ids = Vec::new();
    loop {
        match cursor2.next() {
            ExpansionStep::Rewrite(mark) => mark_ids.push(mark.record_id),
            ExpansionStep::NeedRecord(id) => {
                cursor2.supply(&id, history.read(&id).map_err(|e| e.to_string()));
            }
            ExpansionStep::Unreadable(err) => panic!("{err:?}"),
            ExpansionStep::Block(_) => {}
            ExpansionStep::End => break,
        }
    }
    assert_eq!(
        mark_ids,
        vec![r2.record_id.clone(), r1.record_id.clone()],
        "separated prelude-fold run must not emit a duplicate Rewrite divider"
    );

    let expanded2 = expand_conversation(after2, &runtime.entries(), None, |id| history.read(id));
    let restored2: Vec<ContextBlock> = expanded2.blocks.iter().map(|b| b.block.clone()).collect();
    let mut expected_full = initial_blocks;
    expected_full.extend([u1_big, upd_skills, a1, u2]);
    assert_eq!(restored2, expected_full);
    assert!(
        expanded2.blocks[..5]
            .iter()
            .all(|b| b.status == BlockStatus::Original)
    );
}

#[tokio::test]
async fn test_si04_compact_fold_retightens_small_window_budget() {
    let dir = tempfile::tempdir().unwrap();
    // 14K window with 70% threshold -> trigger_line = 9,800, half = 4,900 tokens.
    let (runtime, _) = runtime(dir.path(), Some(14_000));

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let env = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\npermission_mode: ask\n</env>",
    );
    let huge_ws = "Rule line. ".repeat(1_600); // ~17.6 KiB
    let upd_ws = prelude_block(
        Role::User,
        DynamicSection::WorkspaceInstructions.update_source(),
        &format!("<workspace_instructions updated=\"true\">\n{huge_ws}\n</workspace_instructions>"),
    );
    let u0 = big_block(1800);
    let a0 = assistant("a0");
    let u1 = user("u1");

    let mut context = Context::from_blocks(vec![base, env, u0, upd_ws, a0, u1]).unwrap();
    let rewrite = plan(&runtime, &context).await.unwrap();
    runtime.commit_rewrite(&mut context, rewrite).unwrap();

    let after = context.blocks();
    let estimate = segment::fixed_prefix_estimate(after);
    let half_trigger = (14_000 * 70 / 100) / 2;
    assert!(
        estimate <= half_trigger,
        "folded fixed prefix estimate {estimate} must stay <= {half_trigger}"
    );
    let ws_block = after
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE))
        .unwrap();
    assert!(text_of(ws_block).contains("[truncated; original size:"));
}
