//! Compaction protection, budget cap, oldest-first eviction, deduplication,
//! and lossless history expansion tests for `tars:skill` blocks (SI07).

use super::super::expansion::{BlockStatus, ExpansionCursor, ExpansionStep, expand_conversation};
use super::super::segment;
use super::partial::{assistant, plan, runtime, user};
use super::{big_block, tool_change};
use crate::prelude::{BASE_HOST_CONTRACT, PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE};
use crate::skills::{SKILL_SOURCE, parse_standalone_skill_block};
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContentPart, Context, ContextBlock, Role, ToolCallPayload,
    ToolOutput, ToolOutputMeta, ToolResultPayload, ToolResultStatus,
};

fn prelude_block(role: Role, source: &str, text: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(role, text),
        BlockMeta::from_source(source),
    )
}

fn load_skill_call(name: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(
            "load_skill",
            serde_json::json!({ "name": name }),
        )),
        BlockMeta::default(),
    )
}

fn load_skill_result(call: &ContextBlock, name: &str, sha256: &str, body: &str) -> ContextBlock {
    let mut output = ToolOutput::new(serde_json::Value::String(format!(
        "<skill_content name=\"{name}\">\n{body}\n</skill_content>"
    )));
    let mut meta = ToolOutputMeta::default();
    meta.extra = Some(serde_json::json!({
        "source": SKILL_SOURCE,
        "skill": name,
        "sha256": sha256,
    }));
    output.meta = Some(meta);
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call.id(),
            ToolResultStatus::Succeeded,
            output,
        )),
        BlockMeta::default(),
    )
}

fn load_skill_dedup_result(call: &ContextBlock, name: &str, existing_id: BlockId) -> ContextBlock {
    let output = ToolOutput::new(serde_json::Value::String(format!(
        "Skill '{name}' is already loaded in the current context (block_id: {}).",
        existing_id.0
    )));
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call.id(),
            ToolResultStatus::Succeeded,
            output,
        )),
        BlockMeta::default(),
    )
}

fn text_of(block: &ContextBlock) -> String {
    match block.content() {
        BlockContent::Message { parts, .. } => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.0.as_str()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

fn has_orphaned_tool_results(blocks: &[ContextBlock]) -> bool {
    let call_ids: std::collections::HashSet<BlockId> = blocks
        .iter()
        .filter_map(|b| match b.content() {
            BlockContent::ToolCall(_) => Some(b.id()),
            _ => None,
        })
        .collect();
    blocks.iter().any(|b| match b.content() {
        BlockContent::ToolResult(res) => !call_ids.contains(&res.call_block_id),
        _ => false,
    })
}

#[test]
fn test_si07_skill_protection_budget_formula() {
    // Unknown window -> 12,000 token cap
    assert_eq!(segment::skill_protection_budget(None, 70, 500), 12_000);
    // 100K window, 70% threshold -> trigger_line = 70,000; (70,000 - 2,000) / 4 = 17,000 -> capped at 12,000
    assert_eq!(
        segment::skill_protection_budget(Some(100_000), 70, 2_000),
        12_000
    );
    // 14K window, 70% threshold -> trigger_line = 9,800; (9,800 - 1,800) / 4 = 2,000
    assert_eq!(
        segment::skill_protection_budget(Some(14_000), 70, 1_800),
        2_000
    );
    // Fixed prefix above trigger_line -> saturates to 0 without underflow
    assert_eq!(segment::skill_protection_budget(Some(10_000), 70, 9_000), 0);
}

#[tokio::test]
async fn test_si07_first_and_second_compaction_promote_keep_exact_id_and_expand_losslessly() {
    let dir = tempfile::tempdir().unwrap();
    // 20K window -> trigger_line = 14,000; tail budget ~ 3,000 tokens; skill budget ~ 3,000 tokens.
    let (runtime, _) = runtime(dir.path(), Some(20_000));

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, BASE_HOST_CONTRACT);
    let env = prelude_block(
        Role::User,
        PRELUDE_ENV_SOURCE,
        "<env>\npermission_mode: ask\n</env>",
    );
    let tc = tool_change("load_skill");

    let u0 = big_block(1_500);
    let c_alpha = load_skill_call("alpha");
    let r_alpha = load_skill_result(&c_alpha, "alpha", "hash-alpha-1", "Alpha skill guide");
    let a0 = big_block(10_500); // > 3,000 tokens so tail starts at u_tail
    let u_tail = user("tail prompt 1");

    let initial_blocks = vec![
        base.clone(),
        env.clone(),
        tc.clone(),
        u0.clone(),
        c_alpha.clone(),
        r_alpha.clone(),
        a0.clone(),
        u_tail.clone(),
    ];
    let mut context = Context::from_blocks(initial_blocks.clone()).unwrap();

    // Compaction #1: ToolCall + ToolResult in head_end..k are removed together and
    // promoted into a standalone Role::User `tars:skill` block with sha256 attribute.
    let rewrite1 = plan(&runtime, &context).await.unwrap();
    let r1 = runtime.commit_rewrite(&mut context, rewrite1).unwrap();
    let after1 = context.blocks().to_vec();

    assert!(
        !has_orphaned_tool_results(&after1),
        "first compaction must never leave an orphaned ToolResult"
    );
    assert!(
        !after1.iter().any(|b| matches!(
            b.content(),
            BlockContent::ToolCall(_) | BlockContent::ToolResult(_)
        )),
        "original load_skill call/result pair was cleanly replaced"
    );
    let alpha_pos = after1
        .iter()
        .position(|b| parse_standalone_skill_block(b).is_some())
        .expect("promoted standalone tars:skill block");
    let standalone_alpha = after1[alpha_pos].clone();
    assert_eq!(
        parse_standalone_skill_block(&standalone_alpha),
        Some(("alpha".to_owned(), "hash-alpha-1".to_owned()))
    );
    assert!(text_of(&standalone_alpha).contains("Alpha skill guide"));

    // Expansion #1 restores the original ToolCall + ToolResult losslessly with 1 divider.
    let history = runtime.history().unwrap();
    let mut cursor1 = ExpansionCursor::new(&after1, runtime.entries());
    let mut marks1 = Vec::new();
    loop {
        match cursor1.next() {
            ExpansionStep::Rewrite(mark) => marks1.push(mark.record_id),
            ExpansionStep::NeedRecord(id) => {
                cursor1.supply(&id, history.read(&id).map_err(|e| e.to_string()));
            }
            ExpansionStep::Unreadable(err) => panic!("{err:?}"),
            ExpansionStep::Block(_) => {}
            ExpansionStep::End => break,
        }
    }
    assert_eq!(marks1, vec![r1.record_id.clone()]);
    let expanded1 = expand_conversation(&after1, &runtime.entries(), None, |id| history.read(id));
    let restored1: Vec<ContextBlock> = expanded1.blocks.iter().map(|b| b.block.clone()).collect();
    assert_eq!(restored1, initial_blocks);

    // Compaction #2: keep standalone_alpha by exact BlockId AND promote a newly loaded `beta`
    // in chronological/activation order (`alpha` before `beta`).
    let u1 = big_block(1_500);
    let c_beta = load_skill_call("beta");
    let r_beta = load_skill_result(&c_beta, "beta", "hash-beta-1", "Beta skill guide");
    let a1 = big_block(10_500);
    let u_tail2 = user("tail prompt 2");

    context
        .edit()
        .append(vec![
            u1.clone(),
            c_beta.clone(),
            r_beta.clone(),
            a1.clone(),
            u_tail2.clone(),
        ])
        .commit()
        .unwrap();

    let rewrite2 = plan(&runtime, &context).await.unwrap();
    let r2 = runtime.commit_rewrite(&mut context, rewrite2).unwrap();
    let after2 = context.blocks().to_vec();

    assert!(!has_orphaned_tool_results(&after2));
    let skill_blocks2: Vec<(usize, &ContextBlock, (String, String))> = after2
        .iter()
        .enumerate()
        .filter_map(|(i, b)| parse_standalone_skill_block(b).map(|parsed| (i, b, parsed)))
        .collect();
    assert_eq!(skill_blocks2.len(), 2, "{after2:?}");
    assert_eq!(
        skill_blocks2[0].1.id(),
        standalone_alpha.id(),
        "second compaction must keep standalone tars:skill block with exact original BlockId"
    );
    assert_eq!(skill_blocks2[0].2.0, "alpha");
    assert_eq!(skill_blocks2[1].2.0, "beta");
    assert!(
        skill_blocks2[0].0 < skill_blocks2[1].0,
        "activation order (alpha before beta) must be preserved"
    );

    // Expansion #2 restores both rounds cleanly back to `initial_blocks + turn2`.
    let mut cursor2 = ExpansionCursor::new(&after2, runtime.entries());
    let mut marks2 = Vec::new();
    loop {
        match cursor2.next() {
            ExpansionStep::Rewrite(mark) => marks2.push(mark.record_id),
            ExpansionStep::NeedRecord(id) => {
                cursor2.supply(&id, history.read(&id).map_err(|e| e.to_string()));
            }
            ExpansionStep::Unreadable(err) => panic!("{err:?}"),
            ExpansionStep::Block(_) => {}
            ExpansionStep::End => break,
        }
    }
    assert_eq!(marks2, vec![r2.record_id, r1.record_id]);
    let expanded2 = expand_conversation(&after2, &runtime.entries(), None, |id| history.read(id));
    let restored2: Vec<ContextBlock> = expanded2.blocks.iter().map(|b| b.block.clone()).collect();
    let mut expected_full = initial_blocks;
    expected_full.extend([u1, c_beta, r_beta, a1, u_tail2]);
    assert_eq!(restored2, expected_full);
    assert!(
        expanded2.blocks[..3]
            .iter()
            .all(|b| b.status == BlockStatus::Original)
    );
}

#[tokio::test]
async fn test_si07_budget_evicts_oldest_skills_and_deducts_retained_tail() {
    let dir = tempfile::tempdir().unwrap();
    // 16K window, 70% threshold -> trigger_line = 11,200.
    // fixed_prefix ~ 200 tokens -> skill_protection_budget ~ (11,200 - 200) / 4 = 2,750 tokens (~8,250 bytes).
    let (runtime, _) = runtime(dir.path(), Some(16_000));

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, "host");
    let env = prelude_block(Role::User, PRELUDE_ENV_SOURCE, "<env>\n</env>");

    // Three skills of ~3,000 bytes (~1,000 tokens) each in head_end..k, plus one ~2,500 bytes (~833 tokens) in k..
    // Total ~3,833 tokens > 2,750 -> `tail-skill` (833) + `newest` (1,000) fit, while `oldest` (1,000) and `older`?
    // Let's size each skill in head_end..k to 3,200 bytes (~1,067 tokens) and tail skill to 2,400 bytes (~800 tokens):
    // remaining after tail = 2,750 - 800 = 1,950 tokens -> only `newest` (1,067 tokens) fits, `oldest` (1,067) is evicted!
    let c_oldest = load_skill_call("oldest");
    let r_oldest = load_skill_result(&c_oldest, "oldest", "h-oldest", &"o".repeat(3_200));
    let c_newest = load_skill_call("newest");
    let r_newest = load_skill_result(&c_newest, "newest", "h-newest", &"n".repeat(3_200));
    let sep = big_block(5_000);
    // Tail starts at `u_tail` (total tail ~ 2,400 + 600 < 2,750 tail budget).
    let u_tail = user("tail question");
    let c_tail = load_skill_call("tail-skill");
    let r_tail = load_skill_result(&c_tail, "tail-skill", "h-tail", &"t".repeat(2_200));
    let a_tail = assistant("done");

    let mut context = Context::from_blocks(vec![
        base, env, c_oldest, r_oldest, c_newest, r_newest, sep, u_tail, c_tail, r_tail, a_tail,
    ])
    .unwrap();

    let rewrite = plan(&runtime, &context).await.unwrap();
    runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();

    let active_names: Vec<String> = after
        .iter()
        .filter_map(|b| {
            parse_standalone_skill_block(b)
                .map(|(n, _)| n)
                .or_else(|| crate::skills::parse_tool_result_skill(b).map(|(n, _, _)| n))
        })
        .collect();
    assert_eq!(
        active_names,
        vec!["newest".to_owned(), "tail-skill".to_owned()],
        "oldest skill must be evicted when tail + newest exhaust the protection cap"
    );
}

#[tokio::test]
async fn test_si07_same_skill_reload_supersedes_old_version_while_dedup_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _) = runtime(dir.path(), Some(20_000));

    let base = prelude_block(Role::System, PRELUDE_BASE_SOURCE, "host");
    let env = prelude_block(Role::User, PRELUDE_ENV_SOURCE, "<env>\n</env>");

    // `alpha` v1 (standalone) followed by `alpha` v2 (ToolResult) in head_end..k,
    // and `beta` v1 in head_end..k followed by a dedup response for `beta` in k..
    let alpha_v1 = prelude_block(
        Role::User,
        SKILL_SOURCE,
        "<skill_content name=\"alpha\" sha256=\"h1\">\nAlpha old v1\n</skill_content>",
    );
    let c_alpha_v2 = load_skill_call("alpha");
    let r_alpha_v2 = load_skill_result(&c_alpha_v2, "alpha", "h2", "Alpha new v2");
    let c_beta = load_skill_call("beta");
    let r_beta = load_skill_result(&c_beta, "beta", "hb1", "Beta full body");
    let big = big_block(10_500);
    let u_tail = user("tail");
    let c_beta_dedup = load_skill_call("beta");
    let r_beta_dedup = load_skill_dedup_result(&c_beta_dedup, "beta", r_beta.id());
    let a_tail = assistant("ok");

    let mut context = Context::from_blocks(vec![
        base,
        env,
        alpha_v1.clone(),
        c_alpha_v2,
        r_alpha_v2,
        c_beta,
        r_beta,
        big,
        u_tail,
        c_beta_dedup,
        r_beta_dedup,
        a_tail,
    ])
    .unwrap();

    let rewrite = plan(&runtime, &context).await.unwrap();
    runtime.commit_rewrite(&mut context, rewrite).unwrap();
    let after = context.blocks();

    // `alpha` v1 must NOT be kept; only `alpha` v2 and `beta` (protected despite dedup in tail!) survive.
    assert!(!after.iter().any(|b| b.id() == alpha_v1.id()));
    let standalone: Vec<(String, String)> = after
        .iter()
        .filter_map(parse_standalone_skill_block)
        .collect();
    assert_eq!(
        standalone,
        vec![
            ("alpha".to_owned(), "h2".to_owned()),
            ("beta".to_owned(), "hb1".to_owned()),
        ]
    );
}
