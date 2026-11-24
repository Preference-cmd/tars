//! Tests for zero-LLM micro-compaction (MC01–MC05):
//! candidate selection, superseded read_file detection, hot/cold cache-safety
//! gates, BE24 sealed stripping & format invariants, and expansion/recall/export.

use super::super::HistoryTrigger;
use super::super::artifacts::HistoryArtifacts;
use super::super::estimate::{UsageTracker, context_bytes, estimate_bytes};
use super::super::expansion::{BlockStatus, ExpansionCursor, ExpansionStep, expand_conversation};
use super::super::history::{HISTORY_FORMAT, HistoryStore, PlannedReplacement, RewriteKind};
use super::super::micro::{
    COLD_CACHE_IDLE_MS, HOT_TARGET_PERCENT, MIN_COLD_SAVINGS_TOKENS, MIN_FOLD_BLOCK_TOKENS,
    MIN_HOT_SAVINGS_TOKENS, MIN_SUPERSEDED_READ_TOKENS, MicroCompactGate, plan,
};
use super::super::{sealed, segment};
use super::{ToolThenDone, definition, recall_call, recall_control};
use crate::compact::SUMMARY_SOURCE;
use crate::material::{DETACHED_HISTORY_NOTE, load_session_materials, save_new};
use crate::session::{ToolReporter, WorkRef};
use crate::skills::{SKILL_SOURCE, make_standalone_skill_block};
use crate::store::SavedSessionRecord;
use causa::kernel::{
    ArtifactHint, ArtifactKind, ArtifactRef, ArtifactStore, BlockContent, BlockId, BlockMeta,
    ContentPart, Context, ContextBlock, MediaRef, ModelGateway, Role, SealedPart, TextPayload,
    Tool, ToolCallPayload, ToolOutput, ToolOutputMeta, ToolResultPayload, ToolResultStatus,
    Truncation, TurnId,
};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

fn sys_block(text: &str) -> ContextBlock {
    ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::System, text)
}

fn user_block(text: &str) -> ContextBlock {
    ContextBlock::text(BlockId::new(uuid::Uuid::now_v7()), Role::User, text)
}

fn sealed_part(thought: &str) -> ContentPart {
    ContentPart::Sealed(SealedPart::new(
        "anthropic_messages",
        json!({ "type": "thinking", "thinking": thought, "signature": "sig" }),
    ))
}

fn thinking_only_block(thought: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![sealed_part(thought)],
        },
        BlockMeta::default(),
    )
}

fn reply_with_thinking(thought: &str, text: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![
                sealed_part(thought),
                ContentPart::Text(TextPayload::new(text)),
            ],
        },
        BlockMeta::default(),
    )
}

fn tool_call_block(name: &str, arguments: serde_json::Value) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(name, arguments)),
        BlockMeta::default(),
    )
}

fn tool_result_block(
    call: &ContextBlock,
    status: ToolResultStatus,
    content: serde_json::Value,
) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            call.id(),
            status,
            ToolOutput::new(content),
        )),
        BlockMeta::default(),
    )
}

fn read_call(path: &str, start_line: Option<usize>, max_lines: Option<usize>) -> ContextBlock {
    let mut args = json!({ "path": path });
    if let Some(s) = start_line {
        args["start_line"] = json!(s);
    }
    if let Some(m) = max_lines {
        args["max_lines"] = json!(m);
    }
    tool_call_block("read_file", args)
}

fn read_result(
    call: &ContextBlock,
    status: ToolResultStatus,
    path: &str,
    start_line: u64,
    end_line: Option<u64>,
    text: &str,
) -> ContextBlock {
    tool_result_block(
        call,
        status,
        json!({
            "path": path,
            "start_line": start_line,
            "end_line": end_line,
            "text": text,
            "scanned_bytes": text.len(),
            "truncated": false,
            "limit_reason": null,
        }),
    )
}

fn load_skill_result(call: &ContextBlock, name: &str, body: &str) -> ContextBlock {
    let mut output = ToolOutput::new(serde_json::Value::String(body.to_owned()));
    let mut meta = ToolOutputMeta::default();
    meta.extra = Some(json!({
        "source": SKILL_SOURCE,
        "skill": name,
        "sha256": "deadbeef",
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

fn token_sized_text(target_tokens: usize, tag: &str) -> String {
    let target_bytes = target_tokens * 3;
    let mut out = format!("{tag}: ");
    if out.len() < target_bytes {
        out.push_str(&"x".repeat(target_bytes - out.len()));
    }
    out
}

fn permissive_cold_gate() -> MicroCompactGate {
    MicroCompactGate::Cold {
        last_call_at_ms: Some(1_000),
        now_ms: 1_000 + COLD_CACHE_IDLE_MS,
    }
}

#[test]
fn mc01_candidate_selection_boundaries_protection_and_artifact_inheritance() {
    let sys = sys_block("system contract");
    let brief = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(Role::User, "earlier brief"),
        BlockMeta::from_source(SUMMARY_SOURCE),
    );
    let standalone_skill = make_standalone_skill_block(format!(
        "<skill_content name=\"rust-doc\">\n{}\n</skill_content>",
        "s".repeat(2_000)
    ));

    // load_skill tool result (> 400 tokens, must never be folded)
    let skill_call = tool_call_block("load_skill", json!({ "name": "coding" }));
    let skill_res = load_skill_result(&skill_call, "coding", &"k".repeat(2_000));

    // Media-carrying tool result (> 400 tokens, must never be folded)
    let media_call = tool_call_block("run_command", json!({ "command": "render.sh" }));
    let mut media_payload = ToolResultPayload::new(
        media_call.id(),
        ToolResultStatus::Succeeded,
        ToolOutput::new(serde_json::Value::String(token_sized_text(
            600,
            "media-text",
        ))),
    );
    media_payload.media = vec![MediaRef::new("image/png", "sha256:1234")];
    let media_res = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(media_payload),
        BlockMeta::default(),
    );

    // Protected current work input (> 400 tokens, must never be folded)
    let protected_call = tool_call_block("run_command", json!({ "command": "pwd" }));
    let protected_res = tool_result_block(
        &protected_call,
        ToolResultStatus::Succeeded,
        serde_json::Value::String(token_sized_text(600, "protected-work-input")),
    );
    let mut current_work_inputs = HashSet::new();
    current_work_inputs.insert(protected_res.id());

    // Small tool result (<= 400 tokens, not superseded, must not be folded)
    let small_call = tool_call_block("run_command", json!({ "command": "echo hi" }));
    let small_res = tool_result_block(
        &small_call,
        ToolResultStatus::Succeeded,
        serde_json::Value::String("a".repeat(300)),
    );
    assert!(estimate_bytes(segment::block_bytes(&small_res)) <= MIN_FOLD_BLOCK_TOKENS);

    // Eligible normal tool results (> 400 tokens)
    let big_call_1 = tool_call_block(
        "run_command",
        json!({ "command": "cargo\ncheck\x1b[31m --all-targets\t&& echo done" }),
    );
    let mut big_res_1_payload = ToolResultPayload::new(
        big_call_1.id(),
        ToolResultStatus::Succeeded,
        ToolOutput::new(serde_json::Value::String(token_sized_text(
            2_500,
            "cmd-output-1",
        ))),
    )
    .with_notes(vec![TextPayload::new("exit 0")]);
    big_res_1_payload.output.truncation = Truncation::None;
    let big_res_1 = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(big_res_1_payload),
        BlockMeta::from_source("tool-source"),
    );

    // Entry-spilled 2K preview block (> 400 tokens) with existing artifact:<uuid>
    let spilled_call = read_call("src/huge_module.rs", Some(1), Some(200));
    let spilled_uuid = uuid::Uuid::now_v7().to_string();
    let spilled_art_id = format!("artifact:{spilled_uuid}");
    let mut spilled_output = ToolOutput::new(json!({
        "path": "src/huge_module.rs",
        "start_line": 1,
        "end_line": 200,
        "text": token_sized_text(1_800, "spilled-preview"),
        "scanned_bytes": 40_000,
        "truncated": true,
        "limit_reason": null,
    }));
    spilled_output.truncation = Truncation::Middle;
    spilled_output.artifact = Some(ArtifactRef::new(
        spilled_art_id.clone(),
        40_000,
        ArtifactKind::FullOutput,
        true,
    ));
    let mut spilled_meta = ToolOutputMeta::default();
    spilled_meta.original_tokens = Some(13_333);
    spilled_output.meta = Some(spilled_meta);
    let spilled_res = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            spilled_call.id(),
            ToolResultStatus::Succeeded,
            spilled_output,
        )),
        BlockMeta::default(),
    );

    // Retained tail (last user + huge tool call/result that must stay inside k..)
    let tail_user = user_block("latest user request");
    let tail_call = read_call("src/tail.rs", Some(1), Some(100));
    let tail_res = read_result(
        &tail_call,
        ToolResultStatus::Succeeded,
        "src/tail.rs",
        1,
        Some(100),
        &token_sized_text(1_500, "tail-read"),
    );

    let blocks = vec![
        sys,
        brief,
        user_block("turn 1"),
        standalone_skill,
        skill_call,
        skill_res.clone(),
        media_call,
        media_res.clone(),
        protected_call,
        protected_res.clone(),
        small_call,
        small_res.clone(),
        big_call_1.clone(),
        big_res_1.clone(),
        spilled_call,
        spilled_res.clone(),
        tail_user,
        tail_call,
        tail_res.clone(),
    ];
    let context = Context::from_blocks(blocks.clone()).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // window = 16_000, threshold = 70 -> retained_tail_budget ≈ 2_750 tokens,
    // so tail_user + tail_call + tail_res (~1_600 tokens) stay in k.. while
    // spilled_res (~1_800 tokens) and everything before it are in head_end..k.
    let micro_plan = plan(
        context.blocks(),
        Some(16_000),
        70,
        &current_work_inputs,
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should plan micro-compaction");

    assert_eq!(micro_plan.rewrite.kind, RewriteKind::MicroCompact);
    assert_eq!(micro_plan.cleared_results, 2);
    assert_eq!(micro_plan.rewrite.replacements.len(), 2);

    let r0 = &micro_plan.rewrite.replacements[0];
    let r1 = &micro_plan.rewrite.replacements[1];
    assert_eq!(r0.range, 13..14);
    assert_eq!(r1.range, 15..16);

    // Verify stub 0 (unspilled -> <record_id>/<orig_block_id>, sanitized label, metadata preserved)
    let stub_0 = &r0.with[0];
    assert_ne!(stub_0.id(), big_res_1.id());
    assert_eq!(stub_0.meta(), big_res_1.meta());
    let BlockContent::ToolResult(stub_0_payload) = stub_0.content() else {
        panic!("expected ToolResult");
    };
    assert_eq!(stub_0_payload.call_block_id, big_call_1.id());
    assert_eq!(stub_0_payload.status, ToolResultStatus::Succeeded);
    assert_eq!(stub_0_payload.notes, vec![TextPayload::new("exit 0")]);
    assert_eq!(stub_0_payload.output.truncation, Truncation::Middle);
    let expected_recall_0 = format!("{}/{}", micro_plan.rewrite.record_id, big_res_1.id().0);
    assert_eq!(
        stub_0_payload.output.artifact.as_ref().unwrap().id,
        expected_recall_0
    );
    let stub_0_str = stub_0_payload.output.content.as_str().unwrap();
    assert!(!stub_0_str.contains('\n') && !stub_0_str.contains('\x1b'));
    assert!(stub_0_str.starts_with(
        "[cleared tool_result: run_command command=\"cargo check [31m --all-targets && echo done\""
    ));
    assert!(stub_0_str.contains(&format!("full output: recall id={expected_recall_0}]")));

    // Verify stub 1 (spilled preview -> inherits artifact:<id> and original_tokens=13333)
    let stub_1 = &r1.with[0];
    let BlockContent::ToolResult(stub_1_payload) = stub_1.content() else {
        panic!("expected ToolResult");
    };
    assert_eq!(
        stub_1_payload.output.artifact.as_ref().unwrap().id,
        spilled_art_id
    );
    assert_eq!(
        stub_1_payload
            .output
            .meta
            .as_ref()
            .and_then(|m| m.original_tokens),
        Some(13_333)
    );
    let stub_1_str = stub_1_payload.output.content.as_str().unwrap();
    assert!(stub_1_str.contains("read_file path=\"src/huge_module.rs\" (~13333 tokens)"));
    assert!(stub_1_str.contains(&format!("full output: recall id={spilled_art_id}]")));

    // Apply edit to a copy and verify second micro-compact pass does not re-fold the stubs
    let preview = micro_plan.rewrite.preview(&context).unwrap();
    let second = plan(
        preview.blocks(),
        Some(16_000),
        70,
        &current_work_inputs,
        estimate_bytes(context_bytes(&preview)),
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    );
    assert!(second.is_none(), "stubs must never be re-folded");
}

#[test]
fn mc01_unpaired_tools_and_prefix_too_large_return_none() {
    let call = read_call("src/lib.rs", Some(1), Some(50));
    let unpaired = vec![user_block("hi"), call];
    assert!(
        plan(
            &unpaired,
            Some(10_000),
            70,
            &HashSet::new(),
            100,
            HistoryTrigger::Auto,
            permissive_cold_gate(),
        )
        .is_none()
    );

    // PrefixTooLarge: fixed prefix > trigger_line / 2
    let huge_sys = sys_block(&"s".repeat(12_000)); // ~4000 tokens
    let c = read_call("src/a.rs", Some(1), Some(50));
    let r = read_result(
        &c,
        ToolResultStatus::Succeeded,
        "src/a.rs",
        1,
        Some(50),
        &token_sized_text(5_000, "big"),
    );
    let blocks = vec![huge_sys, user_block("u"), c, r, user_block("tail")];
    // window = 10_000, threshold = 70 -> trigger_line = 7_000, half = 3_500 < 4_000
    assert!(
        plan(
            &blocks,
            Some(10_000),
            70,
            &HashSet::new(),
            9_000,
            HistoryTrigger::Auto,
            permissive_cold_gate(),
        )
        .is_none()
    );
}

#[test]
fn mc02_superseded_read_file_positive_and_counterexamples() {
    // Each test read_file result is ~260 tokens (> 200 superseded threshold, <= 400 normal threshold)
    // except a big filler so the cold gate (>= 4_000 tokens & >= 25%) passes cleanly.
    let filler_call = tool_call_block("run_command", json!({ "command": "cargo test" }));
    let filler_res = tool_result_block(
        &filler_call,
        ToolResultStatus::Succeeded,
        serde_json::Value::String(token_sized_text(5_000, "filler")),
    );

    // 1. Superseded by covering read_file (lines 10..50 covered by 1..100, with ./ normalization)
    let c_sup_read = read_call("./src/a.rs", Some(10), Some(41));
    let r_sup_read = read_result(
        &c_sup_read,
        ToolResultStatus::Succeeded,
        "./src/a.rs",
        10,
        Some(50),
        &token_sized_text(240, "sup-by-read"),
    );

    // 2. Superseded by succeeded edit_file on same path
    let c_sup_edit = read_call("src/b.rs", Some(1), Some(20));
    let r_sup_edit = read_result(
        &c_sup_edit,
        ToolResultStatus::Succeeded,
        "src/b.rs",
        1,
        Some(20),
        &token_sized_text(240, "sup-by-edit"),
    );

    // 3. Superseded by succeeded write_file on same path
    let c_sup_write = read_call("src/c.rs", Some(1), Some(20));
    let r_sup_write = read_result(
        &c_sup_write,
        ToolResultStatus::Succeeded,
        "src/c.rs",
        1,
        Some(20),
        &token_sized_text(240, "sup-by-write"),
    );

    // 4. Counterexample: different path (240 tokens -> not superseded -> skipped!)
    let c_diff_path = read_call("src/d.rs", Some(1), Some(20));
    let r_diff_path = read_result(
        &c_diff_path,
        ToolResultStatus::Succeeded,
        "src/d.rs",
        1,
        Some(20),
        &token_sized_text(240, "diff-path"),
    );

    // 5. Counterexample: disjoint line range on src/e.rs (1..20 then 21..40 -> not superseded)
    let c_disjoint = read_call("src/e.rs", Some(1), Some(20));
    let r_disjoint = read_result(
        &c_disjoint,
        ToolResultStatus::Succeeded,
        "src/e.rs",
        1,
        Some(20),
        &token_sized_text(240, "disjoint"),
    );

    // 6. Counterexample: partial overlap (10..50 then 20..80, later_start > earlier_start -> not superseded)
    let c_partial = read_call("src/f.rs", Some(10), Some(41));
    let r_partial = read_result(
        &c_partial,
        ToolResultStatus::Succeeded,
        "src/f.rs",
        10,
        Some(50),
        &token_sized_text(240, "partial"),
    );

    // 7. Counterexample: later edit_file FAILED / REJECTED on src/g.rs -> not superseded
    let c_failed_later = read_call("src/g.rs", Some(1), Some(20));
    let r_failed_later = read_result(
        &c_failed_later,
        ToolResultStatus::Succeeded,
        "src/g.rs",
        1,
        Some(20),
        &token_sized_text(240, "later-failed"),
    );

    // 8. Counterexample: earlier read_file itself FAILED (240 tokens, even with later write) -> not superseded
    let c_failed_earlier = read_call("src/h.rs", Some(1), Some(20));
    let r_failed_earlier = read_result(
        &c_failed_earlier,
        ToolResultStatus::Failed,
        "src/h.rs",
        1,
        Some(20),
        &token_sized_text(240, "earlier-failed"),
    );

    // 9. Counterexample: <= 200 tokens even when superseded -> NOT folded
    let c_tiny_sup = read_call("src/b.rs", Some(1), Some(5));
    let r_tiny_sup = read_result(
        &c_tiny_sup,
        ToolResultStatus::Succeeded,
        "src/b.rs",
        1,
        Some(5),
        "tiny",
    );
    assert!(estimate_bytes(segment::block_bytes(&r_tiny_sup)) <= MIN_SUPERSEDED_READ_TOKENS);

    // Later calls (placed in head_end..k or tail; let's put covering read in tail to test cross-boundary too!)
    let later_edit = tool_call_block("edit_file", json!({ "path": "src/b.rs", "edits": [] }));
    let later_edit_res = tool_result_block(
        &later_edit,
        ToolResultStatus::Succeeded,
        json!({ "path": "src/b.rs", "action": "edited" }),
    );
    let later_write = tool_call_block(
        "write_file",
        json!({ "path": "src/c.rs", "content": "new" }),
    );
    let later_write_res = tool_result_block(
        &later_write,
        ToolResultStatus::Succeeded,
        json!({ "path": "src/c.rs", "action": "created" }),
    );
    let later_disjoint = read_call("src/e.rs", Some(21), Some(20));
    let later_disjoint_res = read_result(
        &later_disjoint,
        ToolResultStatus::Succeeded,
        "src/e.rs",
        21,
        Some(40),
        "disjoint-later",
    );
    let later_partial = read_call("src/f.rs", Some(20), Some(61));
    let later_partial_res = read_result(
        &later_partial,
        ToolResultStatus::Succeeded,
        "src/f.rs",
        20,
        Some(80),
        "partial-later",
    );
    let later_failed_edit =
        tool_call_block("edit_file", json!({ "path": "src/g.rs", "edits": [] }));
    let later_failed_edit_res = tool_result_block(
        &later_failed_edit,
        ToolResultStatus::Failed,
        json!({ "error": "boom" }),
    );
    let later_rejected_write =
        tool_call_block("write_file", json!({ "path": "src/g.rs", "content": "x" }));
    let later_rejected_write_res = tool_result_block(
        &later_rejected_write,
        ToolResultStatus::Rejected,
        json!({ "error": "rejected" }),
    );
    let later_write_h =
        tool_call_block("write_file", json!({ "path": "src/h.rs", "content": "x" }));
    let later_write_h_res = tool_result_block(
        &later_write_h,
        ToolResultStatus::Succeeded,
        json!({ "path": "src/h.rs", "action": "created" }),
    );
    let tail_user = user_block(&token_sized_text(900, "now re-read a.rs"));
    let later_read_a = read_call("src/a.rs", Some(1), Some(100));
    let later_read_a_res = read_result(
        &later_read_a,
        ToolResultStatus::Succeeded,
        "src/a.rs",
        1,
        Some(100),
        "full a.rs",
    );

    let blocks = vec![
        sys_block("sys"),
        user_block("start"),
        filler_call,
        filler_res.clone(),
        c_sup_read,
        r_sup_read.clone(),
        c_sup_edit,
        r_sup_edit.clone(),
        c_sup_write,
        r_sup_write.clone(),
        c_diff_path,
        r_diff_path,
        c_disjoint,
        r_disjoint,
        c_partial,
        r_partial,
        c_failed_later,
        r_failed_later,
        c_failed_earlier,
        r_failed_earlier,
        c_tiny_sup,
        r_tiny_sup,
        later_edit,
        later_edit_res,
        later_write,
        later_write_res,
        later_disjoint,
        later_disjoint_res,
        later_partial,
        later_partial_res,
        later_failed_edit,
        later_failed_edit_res,
        later_rejected_write,
        later_rejected_write_res,
        later_write_h,
        later_write_h_res,
        tail_user,
        later_read_a,
        later_read_a_res,
    ];
    let context = Context::from_blocks(blocks).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // window = 6_000, threshold = 70 -> retained_tail_budget ≈ 1_040 tokens,
    // so tail_user + later_read_a (~980 tokens) stay in k.. while all earlier
    // reads and later edits/writes stay in head_end..k.
    let micro_plan = plan(
        context.blocks(),
        Some(6_000),
        70,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should select filler + 3 superseded reads");

    assert_eq!(micro_plan.cleared_results, 4);
    let replaced_ids: Vec<BlockId> = micro_plan
        .rewrite
        .replacements
        .iter()
        .map(|r| context.blocks()[r.range.start].id())
        .collect();
    assert_eq!(
        replaced_ids,
        vec![
            filler_res.id(),
            r_sup_read.id(),
            r_sup_edit.id(),
            r_sup_write.id(),
        ]
    );
    for (idx, rep) in micro_plan.rewrite.replacements.iter().enumerate() {
        let BlockContent::ToolResult(stub) = rep.with[0].content() else {
            panic!("expected ToolResult");
        };
        let text = stub.output.content.as_str().unwrap();
        if idx == 0 {
            assert!(text.starts_with("[cleared tool_result:"), "{text}");
        } else {
            assert!(
                text.starts_with("[cleared superseded tool_result:")
                    && text.contains("reread/edited later)"),
                "{text}"
            );
        }
    }
}

#[test]
fn mc03_hot_and_cold_gates_positive_and_counterexamples() {
    // Build a Context with ~4_000 tokens in old tool results and a ~1_000 token suffix after first_edited.
    let c1 = read_call("src/one.rs", Some(1), Some(500));
    let r1 = read_result(
        &c1,
        ToolResultStatus::Succeeded,
        "src/one.rs",
        1,
        Some(500),
        &token_sized_text(4_500, "one"),
    );
    let tail_u = user_block(&token_sized_text(1_000, "tail-question"));
    let blocks = vec![sys_block("sys"), user_block("u0"), c1, r1, tail_u];
    let context = Context::from_blocks(blocks).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));
    // est_before is ~5_600 tokens; saved_tokens is ~4_400 tokens; estimate_after is ~1_200 tokens.

    // --- Path A: Hot gate ---
    // Passing hot gate: trigger_line = 5_000 (75% = 3_750 >= 1_200), window = 10_000, max_tokens = 2_000
    let ok_hot = plan(
        context.blocks(),
        Some(10_000),
        50,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        MicroCompactGate::Hot {
            trigger_line: 5_000,
            window: 10_000,
            max_tokens: Some(2_000),
            cache_is_cold: false,
        },
    )
    .expect("hot gate should pass");
    let saved_tokens = est_before - ok_hot.rewrite.estimate_after;
    assert!(saved_tokens >= MIN_HOT_SAVINGS_TOKENS);
    assert!(ok_hot.rewrite.estimate_after <= 5_000 * HOT_TARGET_PERCENT / 100);

    // Hot Counterexample 1: saved_tokens < MIN_HOT_SAVINGS_TOKENS (2_000)
    let small_c = read_call("src/small.rs", Some(1), Some(200));
    let small_r = read_result(
        &small_c,
        ToolResultStatus::Succeeded,
        "src/small.rs",
        1,
        Some(200),
        &token_sized_text(1_200, "small-savings"),
    );
    let small_ctx = Context::from_blocks(vec![
        sys_block("sys"),
        user_block("u0"),
        small_c,
        small_r,
        user_block("tail"),
    ])
    .unwrap();
    assert!(
        plan(
            small_ctx.blocks(),
            Some(10_000),
            50,
            &HashSet::new(),
            estimate_bytes(context_bytes(&small_ctx)),
            HistoryTrigger::Auto,
            MicroCompactGate::Hot {
                trigger_line: 5_000,
                window: 10_000,
                max_tokens: Some(1_000),
                cache_is_cold: true,
            },
        )
        .is_none(),
        "saved_tokens < MIN_HOT_SAVINGS_TOKENS must be rejected"
    );

    // Hot Counterexample 2: estimate_after > trigger_line * 75 / 100
    assert!(
        plan(
            context.blocks(),
            Some(10_000),
            50,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Hot {
                // 75% of 1_000 is 750 < estimate_after (~1_200)
                trigger_line: 1_000,
                window: 10_000,
                max_tokens: None,
                cache_is_cold: false,
            },
        )
        .is_none(),
        "estimate_after above 75% of trigger_line must be rejected"
    );

    // Hot Counterexample 3: estimate_after + max_tokens > window
    assert!(
        plan(
            context.blocks(),
            Some(10_000),
            50,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Hot {
                trigger_line: 5_000,
                window: 10_000,
                max_tokens: Some(9_500),
                cache_is_cold: false,
            },
        )
        .is_none(),
        "estimate_after + max_tokens > window must be rejected"
    );

    // Hot Counterexample 4: !cache_is_cold && saved_bytes < suffix_after_bytes
    // Place an assistant + tool_result of 2_500 tokens, followed by a large 4_000-token retained tail
    // (window=100_000, threshold=70 -> tail budget=17_500, so 4_000-token tail stays in k..).
    let hc = read_call("src/h.rs", Some(1), Some(300));
    let hr = read_result(
        &hc,
        ToolResultStatus::Succeeded,
        "src/h.rs",
        1,
        Some(300),
        &token_sized_text(2_500, "saved-part"),
    );
    let big_tail = user_block(&token_sized_text(4_000, "heavy-tail"));
    let heavy_tail_ctx =
        Context::from_blocks(vec![sys_block("sys"), user_block("u"), hc, hr, big_tail]).unwrap();
    let heavy_est = estimate_bytes(context_bytes(&heavy_tail_ctx));
    assert!(
        plan(
            heavy_tail_ctx.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            heavy_est,
            HistoryTrigger::Auto,
            MicroCompactGate::Hot {
                trigger_line: 70_000,
                window: 100_000,
                max_tokens: None,
                cache_is_cold: false,
            },
        )
        .is_none(),
        "warm cache with saved_bytes < suffix_after_bytes must be rejected"
    );
    // Same layout passes once cache_is_cold == true
    assert!(
        plan(
            heavy_tail_ctx.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            heavy_est,
            HistoryTrigger::Auto,
            MicroCompactGate::Hot {
                trigger_line: 70_000,
                window: 100_000,
                max_tokens: None,
                cache_is_cold: true,
            },
        )
        .is_some(),
        "cold cache exempts suffix_after_bytes check"
    );

    // --- Path B: Cold gate ---
    // Passing cold gate: saved_tokens (~4_400 >= 4_000) and >= 25% of est_before (~5_600)
    assert!(
        plan(
            context.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Cold {
                last_call_at_ms: Some(10_000),
                now_ms: 10_000 + COLD_CACHE_IDLE_MS,
            },
        )
        .is_some()
    );

    // Cold Counterexample 1: last_call_at_ms == None
    assert!(
        plan(
            context.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Cold {
                last_call_at_ms: None,
                now_ms: 10_000 + COLD_CACHE_IDLE_MS,
            },
        )
        .is_none()
    );

    // Cold Counterexample 2: idle < COLD_CACHE_IDLE_MS
    assert!(
        plan(
            context.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            MicroCompactGate::Cold {
                last_call_at_ms: Some(10_000),
                now_ms: 10_000 + COLD_CACHE_IDLE_MS - 1,
            },
        )
        .is_none()
    );

    // Cold Counterexample 3: saved_tokens < MIN_COLD_SAVINGS_TOKENS (4_000)
    let mid_c = read_call("src/mid.rs", Some(1), Some(300));
    let mid_r = read_result(
        &mid_c,
        ToolResultStatus::Succeeded,
        "src/mid.rs",
        1,
        Some(300),
        &token_sized_text(3_500, "below-cold-min"),
    );
    let mid_ctx = Context::from_blocks(vec![
        sys_block("sys"),
        user_block("u"),
        mid_c,
        mid_r,
        user_block("t"),
    ])
    .unwrap();
    let mid_est = estimate_bytes(context_bytes(&mid_ctx));
    assert!(
        plan(
            mid_ctx.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            mid_est,
            HistoryTrigger::Auto,
            permissive_cold_gate(),
        )
        .is_none()
    );

    // Cold Counterexample 4: saved_tokens >= 4_000 but < 25% of estimate_before
    // est_before artificially / realistically large: e.g. 25_000 tokens (> 4 * 4_400 = 17_600)
    assert!(saved_tokens >= MIN_COLD_SAVINGS_TOKENS);
    assert!(
        plan(
            context.blocks(),
            Some(100_000),
            70,
            &HashSet::new(),
            saved_tokens * 4 + 1,
            HistoryTrigger::Auto,
            permissive_cold_gate(),
        )
        .is_none(),
        "saved_tokens < 25% of estimate_before must be rejected"
    );
}

#[test]
fn mc04_be24_sealed_stripping_and_format_enums() {
    assert_eq!(RewriteKind::MicroCompact.name(), "micro-compact");
    assert_eq!(
        serde_json::to_value(RewriteKind::MicroCompact).unwrap(),
        json!("micro-compact")
    );
    assert_eq!(
        serde_json::from_value::<RewriteKind>(json!("micro-compact")).unwrap(),
        RewriteKind::MicroCompact
    );

    let sample_id = "0190a000-0000-7000-8000-000000000042";
    let status = BlockStatus::MicroCompacted(sample_id.to_owned());
    assert_eq!(status.to_string(), format!("micro-compacted ({sample_id})"));
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        json!({ "status": "micro_compacted", "record_id": sample_id })
    );

    let before_reply = reply_with_thinking("before-thought", "before-text");
    let c1 = read_call("src/file.rs", Some(1), Some(400));
    let r1 = read_result(
        &c1,
        ToolResultStatus::Succeeded,
        "src/file.rs",
        1,
        Some(400),
        &token_sized_text(4_500, "huge-read"),
    );
    let after_thinking = thinking_only_block("after-only-thought");
    let after_reply = reply_with_thinking("after-thought", "after-text");
    let tail_user = user_block("follow-up");

    let blocks = vec![
        sys_block("sys"),
        user_block("u0"),
        before_reply.clone(),
        c1,
        r1.clone(),
        after_thinking.clone(),
        after_reply.clone(),
        tail_user,
    ];
    let mut context = Context::from_blocks(blocks.clone()).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    let micro_plan = plan(
        context.blocks(),
        Some(100_000),
        70,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should plan micro-compaction with BE24 stripping");

    let dir = tempfile::tempdir().unwrap();
    let conv_id = uuid::Uuid::now_v7().to_string();
    let store = HistoryStore::new(dir.path(), &conv_id, Vec::new());
    let entry = store
        .commit_rewrite(&mut context, micro_plan.rewrite)
        .unwrap();
    assert_eq!(entry.kind, RewriteKind::MicroCompact);

    let after = context.blocks();
    // before_reply (index 2) keeps its sealed part untouched!
    assert_eq!(after[2], before_reply);
    assert!(sealed::has_sealed(&after[2]));

    // after_thinking (index 5) was removed into range 4..6, and after_reply (index 6) was stripped!
    assert!(
        after[3..].iter().all(|b| !sealed::has_sealed(b)),
        "no sealed parts may remain after first_edited"
    );
    assert!(
        after.iter().any(|b| match b.content() {
            BlockContent::Message { parts, .. } =>
                parts == &[ContentPart::Text(TextPayload::new("after-text"))],
            _ => false,
        }),
        "stripped reply keeps its text"
    );

    let doc_json: String = crate::store::schema::open_read_only(&dir.path().join("tars.db"))
        .unwrap()
        .unwrap()
        .query_row(
            "SELECT document_json FROM history_records
                 WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
            rusqlite::params![&conv_id, &entry.record_id],
            |row| row.get(0),
        )
        .unwrap();
    let record_json: serde_json::Value = serde_json::from_str(&doc_json).unwrap();
    assert_eq!(record_json["format"], HISTORY_FORMAT);
    assert_eq!(record_json["kind"], "micro-compact");
}

#[tokio::test]
async fn mc05_expansion_nested_summarize_recall_and_export_materials_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = uuid::Uuid::now_v7().to_string();
    let mut def = definition(Some(100_000), None);
    def.workspace_root = Some(dir.path().to_path_buf());
    def.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    let gateway: Arc<dyn ModelGateway> = Arc::new(ToolThenDone {
        main_calls: AtomicUsize::new(0),
        summary_calls: Arc::new(AtomicUsize::new(0)),
        summary_inputs: Arc::new(std::sync::Mutex::new(Vec::new())),
        tool_call_first: false,
        cache_read_tokens: None,
    });
    let runtime = super::super::CompactionRuntime::new(
        &def,
        &conv_id.as_str().into(),
        Some(dir.path()),
        Vec::new(),
        None,
        None,
        gateway,
        Arc::new(UsageTracker::default()),
    )
    .unwrap();
    let store = runtime.history().unwrap();
    let artifacts = HistoryArtifacts::new(store.clone());

    // Pre-spill one oversized tool output to .artifacts/
    let spilled_full_text = format!(
        "SPILLED_ORIGINAL_HEADER\n{}\nSPILLED_ORIGINAL_FOOTER",
        "z".repeat(20_000)
    );
    let spilled_call = read_call("src/spilled.rs", Some(1), Some(500));
    let spilled_ref = artifacts
        .persist(
            spilled_full_text.as_bytes(),
            ArtifactHint::new("read_file", spilled_call.id(), ArtifactKind::FullOutput),
        )
        .await
        .unwrap();
    let mut spilled_out = ToolOutput::new(json!({
        "path": "src/spilled.rs",
        "start_line": 1,
        "end_line": 500,
        "text": token_sized_text(1_800, "spilled-preview"),
        "scanned_bytes": spilled_full_text.len(),
        "truncated": true,
        "limit_reason": null,
    }));
    spilled_out.truncation = Truncation::Middle;
    spilled_out.artifact = Some(spilled_ref.clone());
    let mut spilled_meta = ToolOutputMeta::default();
    spilled_meta.original_tokens = Some(7_000);
    spilled_out.meta = Some(spilled_meta);
    let spilled_res = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(ToolResultPayload::new(
            spilled_call.id(),
            ToolResultStatus::Succeeded,
            spilled_out,
        )),
        BlockMeta::default(),
    );

    // Unspilled tool result with unique searchable marker
    let unspilled_call = read_call("src/unspilled.rs", Some(1), Some(300));
    let unspilled_body = format!(
        "UNSPILLED_ORIGINAL_LINE\n{}\nUNSPILLED_TAIL_MARKER",
        "u".repeat(10_500)
    );
    let unspilled_res = read_result(
        &unspilled_call,
        ToolResultStatus::Succeeded,
        "src/unspilled.rs",
        1,
        Some(300),
        &unspilled_body,
    );

    let sealed_assistant = reply_with_thinking("mid-thought", "mid-reply");
    let tail_user = user_block("current question");

    let orig_blocks = vec![
        sys_block("sys"),
        user_block("turn 1"),
        spilled_call.clone(),
        spilled_res.clone(),
        unspilled_call.clone(),
        unspilled_res.clone(),
        sealed_assistant.clone(),
        tail_user.clone(),
    ];
    let mut context = Context::from_blocks(orig_blocks.clone()).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // window = 12_000, threshold = 70 -> retained_tail_budget ≈ 2_050 tokens,
    // so tail_user + sealed_assistant stay in k.. while spilled_res & unspilled_res
    // stay in head_end..k.
    let micro_plan = plan(
        context.blocks(),
        Some(12_000),
        70,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should plan micro-compaction");
    let mc_entry = store
        .commit_rewrite(&mut context, micro_plan.rewrite)
        .unwrap();

    // 1. In-place expansion: 0 dividers, originals restored, MicroCompacted on folded results, Original on stripped reply
    let mut cursor = ExpansionCursor::new(context.blocks(), store.entries());
    let mut rewrite_marks = 0;
    let mut expanded_rev = Vec::new();
    loop {
        match cursor.next() {
            ExpansionStep::Block(b) => expanded_rev.push(*b),
            ExpansionStep::Rewrite(_) => rewrite_marks += 1,
            ExpansionStep::NeedRecord(id) => {
                let rec = store.read(&id).map_err(|e| e.to_string());
                cursor.supply(&id, rec);
            }
            ExpansionStep::Unreadable(u) => panic!("unexpected unreadable: {u:?}"),
            ExpansionStep::End => break,
        }
    }
    assert_eq!(rewrite_marks, 0, "MicroCompact must emit 0 dividers");
    expanded_rev.reverse();
    let expanded_blocks: Vec<ContextBlock> = expanded_rev.iter().map(|b| b.block.clone()).collect();
    assert_eq!(expanded_blocks, orig_blocks);

    let mc_status = BlockStatus::MicroCompacted(mc_entry.record_id.clone());
    assert_eq!(expanded_rev[0].status, BlockStatus::Original);
    assert_eq!(expanded_rev[1].status, BlockStatus::Original);
    assert_eq!(expanded_rev[2].status, BlockStatus::Original);
    assert_eq!(expanded_rev[3].status, mc_status);
    assert_eq!(expanded_rev[4].status, BlockStatus::Original);
    assert_eq!(expanded_rev[5].status, mc_status);
    assert_eq!(
        expanded_rev[6].status,
        BlockStatus::Original,
        "stripped assistant reply keeps Original status"
    );
    assert_eq!(expanded_rev[7].status, BlockStatus::Original);

    // 2. Recall reads back both <mc_record_id>/<orig_block_id> and inherited artifact:<id>, plus list & search
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: conv_id.as_str().into(),
            turn_id: TurnId::new(uuid::Uuid::now_v7().to_string()),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    let recall_tool = super::super::RecallTool::new(runtime.clone(), reporter, 60);
    let control = recall_control();

    let unspilled_recall_id = format!("{}/{}", mc_entry.record_id, unspilled_res.id().0);
    let read_unspilled = recall_tool
        .execute(
            &recall_call(json!({ "id": unspilled_recall_id })),
            &control,
            None,
        )
        .await;
    assert_eq!(read_unspilled.status, ToolResultStatus::Succeeded);
    let text_unspilled = read_unspilled.output.content.as_str().unwrap();
    assert!(text_unspilled.contains("UNSPILLED_ORIGINAL_LINE"));
    assert!(text_unspilled.contains("UNSPILLED_TAIL_MARKER"));

    let read_spilled = recall_tool
        .execute(
            &recall_call(json!({ "id": spilled_ref.id })),
            &control,
            None,
        )
        .await;
    assert_eq!(read_spilled.status, ToolResultStatus::Succeeded);
    let text_spilled = read_spilled.output.content.as_str().unwrap();
    assert!(text_spilled.contains("SPILLED_ORIGINAL_HEADER"));
    assert!(text_spilled.contains(&format!("more: recall id={} offset=12000", spilled_ref.id)));

    let read_spilled_page2 = recall_tool
        .execute(
            &recall_call(json!({ "id": spilled_ref.id, "offset": 12_000, "limit": 12_000 })),
            &control,
            None,
        )
        .await;
    assert_eq!(read_spilled_page2.status, ToolResultStatus::Succeeded);
    assert!(
        read_spilled_page2
            .output
            .content
            .as_str()
            .unwrap()
            .contains("SPILLED_ORIGINAL_FOOTER")
    );

    let listed = recall_tool
        .execute(
            &recall_call(json!({ "id": mc_entry.record_id })),
            &control,
            None,
        )
        .await;
    assert_eq!(listed.status, ToolResultStatus::Succeeded);
    assert!(
        listed
            .output
            .content
            .as_str()
            .unwrap()
            .contains("micro-compact")
    );

    let searched = recall_tool
        .execute(
            &recall_call(json!({ "query": "UNSPILLED_TAIL_MARKER" })),
            &control,
            None,
        )
        .await;
    assert_eq!(searched.status, ToolResultStatus::Succeeded);
    let search_text = searched.output.content.as_str().unwrap();
    assert!(search_text.contains(&unspilled_recall_id));
    assert!(search_text.contains("UNSPILLED_ORIGINAL_LINE"));

    // 3. SavedSessionRecord validation & export-materials (load_session_materials -> save_new -> load)
    let saved_record = SavedSessionRecord::new_with_compaction(
        conv_id.clone(),
        uuid::Uuid::now_v7().to_string(),
        None,
        def,
        context.clone(),
        None,
        None,
        None,
        store.entries(),
        None,
        None,
        1_000,
    )
    .expect("SavedSessionRecord must validate micro-compacted references");
    assert_eq!(saved_record.format, "tars-session-v2.4");

    let exported_src = dir.path().join("session.json");
    let exported_out = dir.path().join("exported.json");
    std::fs::write(&exported_src, serde_json::to_vec(&saved_record).unwrap()).unwrap();
    let detached = load_session_materials(&exported_src).unwrap();
    save_new(&exported_out, &detached).unwrap();
    let imported = crate::material::load(&exported_out).unwrap();
    assert_eq!(imported.blocks().len(), context.blocks().len());
    for idx in [3usize, 5usize] {
        let BlockContent::ToolResult(res) = imported.blocks()[idx].content() else {
            panic!("expected ToolResult");
        };
        assert!(res.output.artifact.is_none());
        assert!(res.notes.iter().any(|note| note.0 == DETACHED_HISTORY_NOTE));
    }

    // 4. Nested Summarize -> MicroCompact expansion
    let brief = ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(Role::User, "summary brief"),
        BlockMeta::from_source(SUMMARY_SOURCE),
    );
    let sum_rewrite = super::super::history::Rewrite {
        record_id: super::super::history::new_record_id(),
        trigger: HistoryTrigger::Manual,
        kind: RewriteKind::Summarize,
        estimate_before: 10_000,
        estimate_after: 500,
        replacements: vec![PlannedReplacement {
            range: 1..7,
            with: vec![brief],
        }],
    };
    let sum_entry = store.commit_rewrite(&mut context, sum_rewrite).unwrap();
    assert_eq!(context.blocks().len(), 3, "[sys, brief, tail_user]");

    let mut nested_cursor = ExpansionCursor::new(context.blocks(), store.entries());
    let mut nested_marks = Vec::new();
    loop {
        match nested_cursor.next() {
            ExpansionStep::Block(_) => {}
            ExpansionStep::Rewrite(mark) => nested_marks.push(mark.kind),
            ExpansionStep::NeedRecord(id) => {
                let rec = store.read(&id).map_err(|e| e.to_string());
                nested_cursor.supply(&id, rec);
            }
            ExpansionStep::Unreadable(u) => panic!("unexpected unreadable: {u:?}"),
            ExpansionStep::End => break,
        }
    }
    assert_eq!(
        nested_marks,
        vec![RewriteKind::Summarize],
        "only Summarize emits a divider; MicroCompact does not"
    );

    let nested_expanded = expand_conversation(context.blocks(), &store.entries(), None, |id| {
        store.read(id)
    });
    assert!(nested_expanded.unreadable_records.is_empty());
    let nested_blocks: Vec<ContextBlock> = nested_expanded
        .blocks
        .iter()
        .map(|b| b.block.clone())
        .collect();
    assert_eq!(nested_blocks, orig_blocks);

    let sum_status = BlockStatus::Summarized(sum_entry.record_id);
    assert_eq!(nested_expanded.blocks[0].status, BlockStatus::Original);
    assert_eq!(nested_expanded.blocks[1].status, sum_status);
    assert_eq!(nested_expanded.blocks[2].status, sum_status);
    assert_eq!(nested_expanded.blocks[3].status, mc_status);
    assert_eq!(nested_expanded.blocks[4].status, sum_status);
    assert_eq!(nested_expanded.blocks[5].status, mc_status);
    assert_eq!(
        nested_expanded.blocks[6].status, sum_status,
        "stripped reply swallowed by Summarize carries outer Summarized status"
    );
    assert_eq!(nested_expanded.blocks[7].status, BlockStatus::Original);
}

#[test]
fn mc01_open_tool_round_with_sealed_thinking_returns_none() {
    let old_call = read_call("src/old.rs", Some(1), Some(400));
    let old_res = read_result(
        &old_call,
        ToolResultStatus::Succeeded,
        "src/old.rs",
        1,
        Some(400),
        &token_sized_text(5_000, "huge-old-read"),
    );
    let open_call = read_call("src/open.rs", Some(1), Some(20));
    let open_res = read_result(
        &open_call,
        ToolResultStatus::Succeeded,
        "src/open.rs",
        1,
        Some(20),
        "small current result",
    );
    let blocks = vec![
        sys_block("sys"),
        user_block("turn 1"),
        old_call,
        old_res,
        user_block("turn 2"),
        reply_with_thinking("active-round-thought", "calling open.rs"),
        open_call,
        open_res,
    ];
    let context = Context::from_blocks(blocks).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // Even with huge old results and a cold cache, micro::plan must refuse to
    // edit before an open tool round that carries sealed thinking.
    assert!(
        plan(
            context.blocks(),
            Some(12_000),
            70,
            &HashSet::new(),
            est_before,
            HistoryTrigger::Auto,
            permissive_cold_gate(),
        )
        .is_none(),
        "open tool round with sealed thinking must return None"
    );
}

#[test]
fn mc01_open_tool_round_without_thinking_keeps_unseen_trailing_tool_results() {
    let old_call = read_call("src/old.rs", Some(1), Some(400));
    let old_res = read_result(
        &old_call,
        ToolResultStatus::Succeeded,
        "src/old.rs",
        1,
        Some(400),
        &token_sized_text(5_000, "consumed-old-read"),
    );
    let new_call = read_call("src/brand_new.rs", Some(1), Some(500));
    // Brand-new unseen trailing ToolResult (~3_500 tokens) exceeds retained_tail_budget (~2_100 tokens
    // for window=12_000, threshold=70), so segment::tail_start returns blocks.len().
    let new_res = read_result(
        &new_call,
        ToolResultStatus::Succeeded,
        "src/brand_new.rs",
        1,
        Some(500),
        &token_sized_text(3_500, "brand-new-unseen"),
    );
    let blocks = vec![
        sys_block("sys"),
        user_block("turn 1"),
        old_call,
        old_res.clone(),
        user_block("turn 2"),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            Role::Assistant,
            "reading brand_new.rs",
        ),
        new_call,
        new_res.clone(),
    ];
    let context = Context::from_blocks(blocks).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // Verify tail_start really lands at blocks.len() here.
    let head_end = segment::head_end(context.blocks());
    let budget = segment::retained_tail_budget(
        Some(12_000),
        70,
        segment::fixed_prefix_estimate(context.blocks()),
    )
    .unwrap();
    assert_eq!(
        segment::tail_start(context.blocks(), head_end, budget),
        context.blocks().len()
    );

    let micro_plan = plan(
        context.blocks(),
        Some(12_000),
        70,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should fold earlier consumed ToolResult while preserving trailing unseen result");

    assert_eq!(micro_plan.cleared_results, 1);
    assert_eq!(micro_plan.rewrite.replacements.len(), 1);
    assert_eq!(micro_plan.rewrite.replacements[0].range, 3..4);

    let after = micro_plan.rewrite.preview(&context).unwrap();
    assert_eq!(
        after.blocks().last().unwrap(),
        &new_res,
        "brand-new trailing ToolResult must remain completely untouched"
    );
    assert_ne!(
        after.blocks()[3].id(),
        old_res.id(),
        "earlier consumed ToolResult must be folded"
    );
}

#[test]
fn mc05_deleted_micro_compact_record_reports_unreadable_without_divider() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = uuid::Uuid::now_v7().to_string();
    let store = HistoryStore::new(dir.path(), &conv_id, Vec::new());

    let c1 = read_call("src/first.rs", Some(1), Some(300));
    let r1 = read_result(
        &c1,
        ToolResultStatus::Succeeded,
        "src/first.rs",
        1,
        Some(300),
        &token_sized_text(2_500, "first-read"),
    );
    let middle_user = user_block("middle separator message");
    let c2 = read_call("src/second.rs", Some(1), Some(300));
    let r2 = read_result(
        &c2,
        ToolResultStatus::Succeeded,
        "src/second.rs",
        1,
        Some(300),
        &token_sized_text(2_500, "second-read"),
    );
    let tail_user = user_block("latest question");

    let blocks = vec![
        sys_block("sys"),
        user_block("turn 1"),
        c1.clone(),
        r1,
        middle_user.clone(),
        c2.clone(),
        r2,
        tail_user.clone(),
    ];
    let mut context = Context::from_blocks(blocks).unwrap();
    let est_before = estimate_bytes(context_bytes(&context));

    // window = 8_000, threshold = 70 -> retained_tail_budget ≈ 1_350 tokens, keeping only tail_user in k..
    let micro_plan = plan(
        context.blocks(),
        Some(8_000),
        70,
        &HashSet::new(),
        est_before,
        HistoryTrigger::Auto,
        permissive_cold_gate(),
    )
    .expect("should fold both separated ToolResults");
    assert_eq!(micro_plan.cleared_results, 2);
    assert_eq!(micro_plan.rewrite.replacements.len(), 2);

    let mc_entry = store
        .commit_rewrite(&mut context, micro_plan.rewrite)
        .unwrap();
    let stub1_id = context.blocks()[3].id();
    let stub2_id = context.blocks()[6].id();

    // Delete the MicroCompact record row from SQLite so expansion hits an unreadable record.
    store.remove(&mc_entry.record_id);

    let mut cursor = ExpansionCursor::new(context.blocks(), store.entries());
    let mut rewrite_marks = 0usize;
    let mut unreadable_events = Vec::new();
    let mut expanded_rev = Vec::new();
    loop {
        match cursor.next() {
            ExpansionStep::Block(b) => expanded_rev.push(*b),
            ExpansionStep::Rewrite(_) => rewrite_marks += 1,
            ExpansionStep::Unreadable(u) => unreadable_events.push(u),
            ExpansionStep::NeedRecord(id) => {
                let rec = store.read(&id).map_err(|e| e.to_string());
                cursor.supply(&id, rec);
            }
            ExpansionStep::End => break,
        }
    }

    assert_eq!(rewrite_marks, 0, "must emit zero Rewrite dividers");
    assert_eq!(
        unreadable_events.len(),
        1,
        "Unreadable event must be emitted once for the shared missing record"
    );
    assert_eq!(unreadable_events[0].record_id, mc_entry.record_id);

    expanded_rev.reverse();
    let expected_unreadable = BlockStatus::Unreadable(mc_entry.record_id);
    let statuses: Vec<(BlockId, BlockStatus)> = expanded_rev
        .iter()
        .map(|b| (b.block.id(), b.status.clone()))
        .collect();
    assert_eq!(
        statuses,
        vec![
            (context.blocks()[0].id(), BlockStatus::Original),
            (context.blocks()[1].id(), BlockStatus::Original),
            (c1.id(), BlockStatus::Original),
            (stub1_id, expected_unreadable.clone()),
            (middle_user.id(), BlockStatus::Original),
            (c2.id(), BlockStatus::Original),
            (stub2_id, expected_unreadable),
            (tail_user.id(), BlockStatus::Original),
        ]
    );
}
