//! Where a summary goes: head, fixed prefix, cut points and the retained tail.
//!
//! A Context that has been compacted has this shape:
//!
//! 1. the **fixed prefix**: the leading run of system text blocks, which every
//!    protocol renders as the system prompt, plus the tool state;
//! 2. the **brief chain**: earlier briefs, possibly with tool declarations
//!    between them;
//! 3. **protected blocks**: the current work's input, kept with its own ids;
//! 4. **plain material**, the newest of which is the retained tail.
//!
//! The fixed prefix and the chain form the head. A summary replaces the
//! interval from the end of the head to a cut point `k`, except its protected
//! blocks, and keeps `k..` verbatim. Everything here is a pure function of the
//! block list so the rules can be tested without a model.

use super::estimate;
use super::history::PlannedReplacement;
use super::summary::SUMMARY_SOURCE;
use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContentPart, ContextBlock, Role, ToolChange, current_tools,
};
use std::collections::HashSet;

/// Largest retained tail in tokens. A summary keeps at most this much of the
/// newest material verbatim, however large the window is.
pub(crate) const RETAINED_TAIL_CAP: u64 = 20_000;

/// The retained tail gets at most this fraction (1/4) of the trigger line
/// minus the fixed prefix, so a small window still leaves room for the brief.
pub(crate) const RETAINED_TAIL_SHARE: u64 = 4;

/// Compaction gives up when the fixed prefix alone is over this fraction
/// (1/2) of the trigger line: no summary could bring the request far enough
/// below the line to be worth a model call.
pub(crate) const PREFIX_LIMIT_SHARE: u64 = 2;

/// Serialized size of one block, the unit every estimate here is built from.
pub(crate) fn block_bytes(block: &ContextBlock) -> u64 {
    serde_json::to_vec(block)
        .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .unwrap_or(u64::MAX)
}

/// Serialized size of a block list.
pub(crate) fn blocks_bytes(blocks: &[ContextBlock]) -> u64 {
    blocks.iter().fold(0u64, |total, block| {
        total.saturating_add(block_bytes(block))
    })
}

/// Token estimate of a block list, with the same rule as the Context estimate.
pub(crate) fn blocks_estimate(blocks: &[ContextBlock]) -> u64 {
    estimate::estimate_bytes(blocks_bytes(blocks))
}

/// Whether a block is a system message carrying only text.
fn is_system_text(block: &ContextBlock) -> bool {
    matches!(
        block.content(),
        BlockContent::Message { role: Role::System, parts }
            if parts.iter().all(|part| matches!(part, ContentPart::Text(_)))
    )
}

fn is_user_message(block: &ContextBlock) -> bool {
    matches!(
        block.content(),
        BlockContent::Message {
            role: Role::User,
            ..
        }
    )
}

fn is_tool_result(block: &ContextBlock) -> bool {
    matches!(block.content(), BlockContent::ToolResult(_))
}

fn is_tool_change(block: &ContextBlock) -> bool {
    matches!(block.content(), BlockContent::ToolChange(_))
}

/// Whether a block is a brief written by an earlier summary.
pub(crate) fn is_brief(block: &ContextBlock) -> bool {
    block.meta().source.as_deref() == Some(SUMMARY_SOURCE)
}

/// End of the leading system run: one past its last system text block.
///
/// Tool declarations inside the run do not end it, as in Causa's rendering,
/// where a `ToolChange` is not a segment. A Context that does not start with
/// a system block has no fixed-prefix blocks.
pub(crate) fn fixed_prefix_end(blocks: &[ContextBlock]) -> usize {
    let mut end = 0;
    for (index, block) in blocks.iter().enumerate() {
        if is_system_text(block) {
            end = index + 1;
        } else if !is_tool_change(block) {
            break;
        }
    }
    end
}

/// End of the head: the fixed prefix followed by briefs and tool declarations.
pub(crate) fn head_end(blocks: &[ContextBlock]) -> usize {
    let mut end = fixed_prefix_end(blocks);
    while blocks
        .get(end)
        .is_some_and(|block| is_brief(block) || is_tool_change(block))
    {
        end += 1;
    }
    end
}

/// Estimate of the fixed prefix: the leading system blocks plus one tool
/// declaration of the whole current tool state.
///
/// The declaration is counted whole because Anthropic sends the folded tool
/// state with every request, wherever the `ToolChange` blocks sit.
pub(crate) fn fixed_prefix_estimate(blocks: &[ContextBlock]) -> u64 {
    let end = fixed_prefix_end(blocks);
    let mut bytes: u64 = blocks[..end]
        .iter()
        .filter(|block| is_system_text(block))
        .fold(0, |total, block| total.saturating_add(block_bytes(block)));
    let tools = current_tools(blocks);
    if !tools.is_empty() {
        let declaration = ContextBlock::new(
            BlockId::new(uuid::Uuid::nil()),
            BlockContent::ToolChange(ToolChange::between(&[], &tools)),
            BlockMeta::default(),
        );
        bytes = bytes.saturating_add(block_bytes(&declaration));
    }
    estimate::estimate_bytes(bytes)
}

/// The fixed prefix is too large for compaction to help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PrefixTooLarge;

/// Token budget of the retained tail.
///
/// With a known window the trigger line is `window × threshold%`; the tail
/// gets `min(cap, (line − prefix) / 4)`, and a prefix over half the line
/// stops compaction. Without a window (manual compaction only) the cap
/// applies.
pub(crate) fn retained_tail_budget(
    window: Option<u64>,
    threshold_percent: u8,
    fixed_prefix: u64,
) -> Result<u64, PrefixTooLarge> {
    let Some(window) = window else {
        return Ok(RETAINED_TAIL_CAP);
    };
    let line =
        u64::try_from(u128::from(window) * u128::from(threshold_percent) / 100).unwrap_or(u64::MAX);
    if fixed_prefix > line / PREFIX_LIMIT_SHARE {
        return Err(PrefixTooLarge);
    }
    Ok(RETAINED_TAIL_CAP.min(line.saturating_sub(fixed_prefix) / RETAINED_TAIL_SHARE))
}

/// The briefs of the chain, in order.
pub(crate) fn chain_briefs(blocks: &[ContextBlock]) -> Vec<&ContextBlock> {
    blocks[fixed_prefix_end(blocks)..head_end(blocks)]
        .iter()
        .filter(|block| is_brief(block))
        .collect()
}

/// Whether position `index` may start the retained tail.
///
/// A cut point is a user message, or the first block after a completed tool
/// round (a tool result followed by something that is not one). The end of
/// the Context is always a cut point. A call is therefore never separated
/// from its result, and parallel calls of one round stay together.
pub(crate) fn is_cut_point(blocks: &[ContextBlock], index: usize) -> bool {
    if index >= blocks.len() {
        return true;
    }
    if is_user_message(&blocks[index]) {
        return true;
    }
    index > 0 && is_tool_result(&blocks[index - 1]) && !is_tool_result(&blocks[index])
}

/// Start `k` of the retained tail: the earliest cut point after the head
/// whose tail `blocks[k..]` fits `budget` tokens.
///
/// The end of the Context always qualifies, so a tail that never fits is
/// empty. Returns `head_end` itself only when the head is the whole Context.
pub(crate) fn tail_start(blocks: &[ContextBlock], head_end: usize, budget: u64) -> usize {
    if head_end >= blocks.len() {
        return blocks.len();
    }
    // Suffix sizes, so each candidate is checked in constant time.
    let mut suffix = vec![0u64; blocks.len() + 1];
    for index in (0..blocks.len()).rev() {
        suffix[index] = suffix[index + 1].saturating_add(block_bytes(&blocks[index]));
    }
    (head_end + 1..=blocks.len())
        .find(|&index| {
            is_cut_point(blocks, index) && estimate::estimate_bytes(suffix[index]) <= budget
        })
        .unwrap_or(blocks.len())
}

/// Positions in `head_end..k` whose blocks are protected.
pub(crate) fn protected_positions(
    blocks: &[ContextBlock],
    head_end: usize,
    k: usize,
    protected: &HashSet<BlockId>,
) -> Vec<usize> {
    (head_end..k)
        .filter(|&index| protected.contains(&blocks[index].id()))
        .collect()
}

/// A declaration of the tool changes between two points, when there are any.
pub(crate) fn tool_delta(before: &[ContextBlock], after: &[ContextBlock]) -> Option<ContextBlock> {
    let change = ToolChange::between(&current_tools(before), &current_tools(after));
    (!change.is_empty()).then(|| {
        ContextBlock::new(
            BlockId::new(uuid::Uuid::now_v7()),
            BlockContent::ToolChange(change),
            BlockMeta::default(),
        )
    })
}

/// The replacements that summarize `head_end..k` around its protected blocks.
///
/// The interval is split at each protected block. The first range inserts
/// `new_blocks` (the optional tool declaration and the brief); every range
/// that starts at a protected block puts that block back unchanged, with its
/// own id, so each range inserts something and expansion can put each
/// range's originals back where its inserted blocks stand. A protected block
/// therefore appears both among a range's originals and its inserted ids;
/// Causa accepts the reuse because content and metadata are unchanged.
pub(crate) fn replacements(
    blocks: &[ContextBlock],
    head_end: usize,
    k: usize,
    protected: &[usize],
    new_blocks: Vec<ContextBlock>,
) -> Vec<PlannedReplacement> {
    let mut starts = vec![head_end];
    starts.extend(protected.iter().copied().filter(|&index| index > head_end));
    let mut new_blocks = Some(new_blocks);
    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).copied().unwrap_or(k);
            let mut with = new_blocks.take().unwrap_or_default();
            if protected.contains(&start) {
                with.push(blocks[start].clone());
            }
            PlannedReplacement {
                range: start..end,
                with,
            }
        })
        .collect()
}
