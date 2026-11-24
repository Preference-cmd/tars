//! Zero-LLM micro-compaction of old and superseded `ToolResult` blocks.
//!
//! Replaces large or superseded `ToolResult` blocks in `head_end..k` with
//! single-line stubs pointing at the original material (`<record_id>/<block_id>`
//! in the rewrite record, or the inherited `artifact:<id>` when already spilled),
//! strips sealed items from assistant replies after the first edit (BE24), and
//! gates execution so it neither thrashes a warm prompt cache nor triggers right
//! before an unavoidable LLM summary.

use super::HistoryTrigger;
use super::estimate;
use super::history::{PlannedReplacement, Rewrite, RewriteKind};
use super::runtime::compute_interval_bounds;
use super::{sealed, segment};
use crate::definition::{BuiltinTool, WorkspaceTool};
use crate::workspace::Workspace;
use causa::kernel::{
    ArtifactKind, ArtifactRef, BlockContent, BlockId, ContextBlock, Role, ToolCallPayload,
    ToolResultPayload, ToolResultStatus, Truncation,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Minimum estimated tokens for a regular `ToolResult` block to be folded.
pub(crate) const MIN_FOLD_BLOCK_TOKENS: u64 = 400;

/// Minimum estimated tokens for a superseded `read_file` `ToolResult` block to be folded.
pub(crate) const MIN_SUPERSEDED_READ_TOKENS: u64 = 200;

/// Minimum net token savings required on the hot-cache pre-summary path.
pub(crate) const MIN_HOT_SAVINGS_TOKENS: u64 = 2_000;

/// Maximum share (percent) of `trigger_line` allowed after hot-cache micro-compaction.
pub(crate) const HOT_TARGET_PERCENT: u64 = 75;

/// Idle duration (ms) after which the prompt cache is considered cold (5 minutes).
pub(crate) const COLD_CACHE_IDLE_MS: u64 = 5 * 60 * 1_000;

/// Minimum net token savings required on the cold-cache opportunistic path.
pub(crate) const MIN_COLD_SAVINGS_TOKENS: u64 = 4_000;

/// Minimum net savings share denominator (`saved_tokens * 4 >= estimate_before`).
pub(crate) const MIN_COLD_SAVINGS_SHARE_DENOM: u64 = 4;

/// Maximum Unicode scalar values kept in a sanitized tool name or argument snippet.
const MAX_LABEL_SNIPPET_CHARS: usize = 64;

/// Cache-safety gate evaluated before committing a micro-compaction rewrite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MicroCompactGate {
    /// Path A: pre-summary gate when auto or passive compaction is triggered.
    Hot {
        /// Configured trigger line in tokens (`window * threshold / 100`).
        trigger_line: u64,
        /// Configured context window in tokens.
        window: u64,
        /// Reserved output tokens (`max_tokens`), if configured.
        max_tokens: Option<u32>,
        /// Whether the prompt cache is already cold (`now - last_call >= COLD_CACHE_IDLE_MS`).
        cache_is_cold: bool,
    },
    /// Path B: opportunistic gate when the prompt cache has gone cold.
    Cold {
        /// Timestamp (Unix ms) of the most recent successful model call, if any.
        last_call_at_ms: Option<u64>,
        /// Current wall-clock timestamp in Unix ms.
        now_ms: u64,
    },
}

/// Planned micro-compaction rewrite ready to be committed via `HistoryStore::commit_rewrite`.
#[derive(Clone, Debug)]
pub(crate) struct MicroCompactPlan {
    /// Rewrite ready for `HistoryStore::commit_rewrite`.
    pub(crate) rewrite: Rewrite,
    /// Number of `ToolResult` blocks folded into stubs.
    pub(crate) cleared_results: usize,
}

/// Plan a zero-LLM micro-compaction over `blocks`, or return `None` when no
/// eligible candidates exist or the cache-safety `gate` rejects the edit.
pub(crate) fn plan(
    blocks: &[ContextBlock],
    window: Option<u64>,
    threshold: u8,
    current_work_inputs: &HashSet<BlockId>,
    estimate_before: u64,
    trigger: HistoryTrigger,
    gate: MicroCompactGate,
) -> Option<MicroCompactPlan> {
    if let MicroCompactGate::Cold {
        last_call_at_ms,
        now_ms,
    } = gate
    {
        let ts = last_call_at_ms?;
        if now_ms.saturating_sub(ts) < COLD_CACHE_IDLE_MS {
            return None;
        }
    }

    if blocks
        .last()
        .is_some_and(|block| matches!(block.content(), BlockContent::ToolResult(_)))
    {
        let loop_start = blocks
            .iter()
            .rposition(|block| {
                matches!(
                    block.content(),
                    BlockContent::Message {
                        role: Role::User,
                        ..
                    }
                )
            })
            .map_or(0, |index| index + 1);
        if blocks[loop_start..].iter().any(sealed::has_sealed) {
            return None;
        }
    }

    let bounds = compute_interval_bounds(blocks, window, threshold, current_work_inputs).ok()?;
    let trailing_results_start = blocks
        .iter()
        .rposition(|block| !matches!(block.content(), BlockContent::ToolResult(_)))
        .map_or(0, |index| index + 1);
    let scan_end = bounds.k.min(trailing_results_start);
    if bounds.head_end >= scan_end {
        return None;
    }

    let calls: HashMap<BlockId, &ToolCallPayload> = blocks
        .iter()
        .filter_map(|block| match block.content() {
            BlockContent::ToolCall(call) => Some((block.id(), call)),
            _ => None,
        })
        .collect();

    let record_id = super::history::new_record_id();
    let mut candidates: Vec<(usize, ContextBlock)> = Vec::new();

    for idx in bounds.head_end..scan_end {
        if bounds.protected.contains(&idx) {
            continue;
        }
        let orig_block = &blocks[idx];
        let BlockContent::ToolResult(payload) = orig_block.content() else {
            continue;
        };
        if crate::skills::is_skill_tool_output(&payload.output) || !payload.media.is_empty() {
            continue;
        }

        let orig_block_bytes = segment::block_bytes(orig_block);
        let est_tokens = estimate::estimate_bytes(orig_block_bytes);
        let superseded = is_superseded_read(blocks, idx, &calls, payload);
        let min_tokens = if superseded {
            MIN_SUPERSEDED_READ_TOKENS
        } else {
            MIN_FOLD_BLOCK_TOKENS
        };
        if est_tokens <= min_tokens {
            continue;
        }

        let call = calls.get(&payload.call_block_id).copied();
        let tool_label = format_tool_label(call);
        let stub = build_stub_block(
            orig_block,
            payload,
            &record_id,
            est_tokens,
            &tool_label,
            superseded,
        );
        if segment::block_bytes(&stub) >= orig_block_bytes {
            continue;
        }
        candidates.push((idx, stub));
    }

    if candidates.is_empty() {
        return None;
    }

    let first_edited = candidates[0].0;
    let cleared_results = candidates.len();
    let replacements: Vec<PlannedReplacement> = candidates
        .into_iter()
        .map(|(idx, stub)| PlannedReplacement {
            range: idx..idx + 1,
            with: vec![stub],
        })
        .collect();
    let replacements = sealed::strip_after(blocks, first_edited, replacements);

    let (removed_bytes, inserted_bytes) = sealed::replaced_bytes(blocks, &replacements);
    if inserted_bytes >= removed_bytes {
        return None;
    }
    let saved_bytes = removed_bytes - inserted_bytes;
    let saved_tokens = estimate::estimate_bytes(saved_bytes);
    let estimate_after = estimate_before.saturating_sub(saved_tokens);

    if !passes_gate(
        blocks,
        first_edited,
        removed_bytes,
        inserted_bytes,
        saved_bytes,
        saved_tokens,
        estimate_before,
        estimate_after,
        gate,
    ) {
        return None;
    }

    Some(MicroCompactPlan {
        rewrite: Rewrite {
            record_id,
            trigger,
            kind: RewriteKind::MicroCompact,
            estimate_before,
            estimate_after,
            replacements,
        },
        cleared_results,
    })
}

#[allow(clippy::too_many_arguments)]
fn passes_gate(
    blocks: &[ContextBlock],
    first_edited: usize,
    removed_bytes: u64,
    inserted_bytes: u64,
    saved_bytes: u64,
    saved_tokens: u64,
    estimate_before: u64,
    estimate_after: u64,
    gate: MicroCompactGate,
) -> bool {
    match gate {
        MicroCompactGate::Hot {
            trigger_line,
            window,
            max_tokens,
            cache_is_cold,
        } => {
            if saved_tokens < MIN_HOT_SAVINGS_TOKENS {
                return false;
            }
            let max_after = u128::from(trigger_line) * u128::from(HOT_TARGET_PERCENT) / 100;
            if u128::from(estimate_after) > max_after {
                return false;
            }
            if max_tokens.is_some_and(|m| estimate_after.saturating_add(u64::from(m)) > window) {
                return false;
            }
            if !cache_is_cold {
                let suffix_after_bytes = segment::blocks_bytes(&blocks[first_edited..])
                    .saturating_sub(removed_bytes)
                    .saturating_add(inserted_bytes);
                if saved_bytes < suffix_after_bytes {
                    return false;
                }
            }
            true
        }
        MicroCompactGate::Cold {
            last_call_at_ms,
            now_ms,
        } => {
            let Some(ts) = last_call_at_ms else {
                return false;
            };
            if now_ms.saturating_sub(ts) < COLD_CACHE_IDLE_MS {
                return false;
            }
            if saved_tokens < MIN_COLD_SAVINGS_TOKENS {
                return false;
            }
            u128::from(saved_tokens) * u128::from(MIN_COLD_SAVINGS_SHARE_DENOM)
                >= u128::from(estimate_before)
        }
    }
}

fn build_stub_block(
    orig_block: &ContextBlock,
    payload: &ToolResultPayload,
    record_id: &str,
    est_tokens: u64,
    tool_label: &str,
    superseded: bool,
) -> ContextBlock {
    let est_tokens_usize = usize::try_from(est_tokens).unwrap_or(usize::MAX);
    let (artifact, original_tokens, recall_id) = match &payload.output.artifact {
        Some(existing) => {
            let original_tokens = payload
                .output
                .meta
                .as_ref()
                .and_then(|meta| meta.original_tokens)
                .unwrap_or(est_tokens_usize);
            (existing.clone(), original_tokens, existing.id.clone())
        }
        None => {
            let recall_id = format!("{record_id}/{}", orig_block.id().0);
            let orig_content_bytes = super::block_to_text(orig_block).len();
            let artifact = ArtifactRef::new(
                recall_id.clone(),
                orig_content_bytes,
                ArtifactKind::FullOutput,
                true,
            );
            (artifact, est_tokens_usize, recall_id)
        }
    };

    let stub_text = if superseded {
        format!(
            "[cleared superseded tool_result: {tool_label} (~{original_tokens} tokens, reread/edited later); full output: recall id={recall_id}]"
        )
    } else {
        format!(
            "[cleared tool_result: {tool_label} (~{original_tokens} tokens); full output: recall id={recall_id}]"
        )
    };

    let mut stub_payload = payload.clone();
    stub_payload.output.truncation = Truncation::Middle;
    let meta = stub_payload
        .output
        .meta
        .get_or_insert_with(Default::default);
    meta.original_tokens = Some(original_tokens);
    stub_payload.output.artifact = Some(artifact);
    stub_payload.output.content = Value::String(stub_text);

    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(stub_payload),
        orig_block.meta().clone(),
    )
}

/// Returns `true` when `earlier_payload` is a succeeded `read_file` result
/// superseded by a later succeeded `edit_file` / `write_file` on the same path,
/// or a later succeeded `read_file` on the same path whose returned line range
/// covers `[earlier_start, earlier_end]`.
fn is_superseded_read(
    blocks: &[ContextBlock],
    idx: usize,
    calls: &HashMap<BlockId, &ToolCallPayload>,
    earlier_payload: &ToolResultPayload,
) -> bool {
    if earlier_payload.status != ToolResultStatus::Succeeded {
        return false;
    }
    let Some(earlier_call) = calls.get(&earlier_payload.call_block_id) else {
        return false;
    };
    if earlier_call.tool_name != WorkspaceTool::ReadFile.name() {
        return false;
    }
    let Some(earlier_path) = extract_tool_path(earlier_call, &earlier_payload.output.content)
    else {
        return false;
    };
    let earlier_range = extract_line_range(&earlier_payload.output.content);

    for later_block in &blocks[idx + 1..] {
        let BlockContent::ToolResult(later_payload) = later_block.content() else {
            continue;
        };
        if later_payload.status != ToolResultStatus::Succeeded {
            continue;
        }
        let Some(later_call) = calls.get(&later_payload.call_block_id) else {
            continue;
        };
        let tool_name = later_call.tool_name.as_str();
        if tool_name == WorkspaceTool::EditFile.name()
            || tool_name == WorkspaceTool::WriteFile.name()
        {
            if extract_tool_path(later_call, &later_payload.output.content).as_deref()
                == Some(earlier_path.as_str())
            {
                return true;
            }
        } else if tool_name == WorkspaceTool::ReadFile.name()
            && let (Some((e_start, e_end)), Some((l_start, l_end))) = (
                earlier_range,
                extract_line_range(&later_payload.output.content),
            )
            && extract_tool_path(later_call, &later_payload.output.content).as_deref()
                == Some(earlier_path.as_str())
            && l_start <= e_start
            && l_end >= e_end
        {
            return true;
        }
    }
    false
}

/// Extract and normalize the workspace path from a tool result (`output.content["path"]`)
/// or its paired call arguments (`call.arguments["path"]`).
fn extract_tool_path(call: &ToolCallPayload, output_content: &Value) -> Option<String> {
    let raw = output_content
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| call.arguments.get("path").and_then(Value::as_str))?;
    normalize_path(raw)
}

fn normalize_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match Workspace::components(trimmed) {
        Ok(parts) if !parts.is_empty() => Some(parts.join("/")),
        Ok(_) => None,
        Err(_) => Some(trimmed.to_owned()),
    }
}

/// Extract 1-based inclusive `(start_line, end_line)` from a `read_file` output object.
fn extract_line_range(output_content: &Value) -> Option<(u64, u64)> {
    let start = output_content.get("start_line")?.as_u64()?;
    let end = output_content.get("end_line")?.as_u64()?;
    (start >= 1 && start <= end).then_some((start, end))
}

/// Build a compact, single-line, control-char-sanitized, bounded label for a tool call.
fn format_tool_label(call: Option<&ToolCallPayload>) -> String {
    let Some(call) = call else {
        return "tool".to_owned();
    };
    let name = sanitize_inline(&call.tool_name, MAX_LABEL_SNIPPET_CHARS);
    if name.is_empty() {
        return "tool".to_owned();
    }
    let arg_str = |key: &str| {
        call.arguments
            .get(key)
            .and_then(Value::as_str)
            .map(|value| sanitize_inline(value, MAX_LABEL_SNIPPET_CHARS))
            .filter(|value| !value.is_empty())
    };

    if call.tool_name == WorkspaceTool::ReadFile.name()
        || call.tool_name == WorkspaceTool::EditFile.name()
        || call.tool_name == WorkspaceTool::WriteFile.name()
        || call.tool_name == WorkspaceTool::ListDirectory.name()
    {
        if let Some(path) = arg_str("path") {
            return format!("{name} path=\"{path}\"");
        }
        return name;
    }

    if call.tool_name == WorkspaceTool::SearchText.name() {
        return match (arg_str("path"), arg_str("query")) {
            (Some(path), Some(query)) => format!("{name} path=\"{path}\" query=\"{query}\""),
            (Some(path), None) => format!("{name} path=\"{path}\""),
            (None, Some(query)) => format!("{name} query=\"{query}\""),
            (None, None) => name,
        };
    }

    if call.tool_name == WorkspaceTool::RunCommand.name() {
        if let Some(command) = arg_str("command") {
            return format!("{name} command=\"{command}\"");
        }
        return name;
    }

    if call.tool_name == BuiltinTool::Recall.name() {
        if let Some(id) = arg_str("id") {
            return format!("{name} id=\"{id}\"");
        }
        if let Some(query) = arg_str("query") {
            return format!("{name} query=\"{query}\"");
        }
        return name;
    }

    if let Some(path) = arg_str("path") {
        return format!("{name} path=\"{path}\"");
    }
    name
}

/// Collapse control characters and whitespace runs to single spaces and bound by scalar count.
fn sanitize_inline(raw: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut pending_space = false;
    let mut truncated = false;

    for ch in raw.chars() {
        if ch.is_control() || ch.is_whitespace() {
            if count > 0 {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            if count >= max_chars {
                truncated = true;
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= max_chars {
            truncated = true;
            break;
        }
        out.push(ch);
        count += 1;
    }

    if truncated {
        out.push_str("...");
    }
    out
}
