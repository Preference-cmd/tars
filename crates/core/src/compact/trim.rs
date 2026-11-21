//! Temporary frame trimming when a round's full frame exceeds the window.
//!
//! Unlike [`super::runtime`] rewrites, trimming operates on a copy of the
//! frame only: the Context, rewrite records, history index and directories
//! stay untouched. It never truncates inside a block; it drops whole oldest
//! intervals at cut points after the head until the estimate plus `max_tokens`
//! fits the window, folding dropped tool changes, dropping any middle system
//! block that would become adjacent to the fixed prefix, and stripping sealed
//! provider items from kept replies after the first change.

use super::sealed;
use super::segment;
use causa::kernel::{BlockContent, BlockId, ContextBlock, ContextFrame, Role};
use std::collections::HashSet;
use std::sync::Mutex;

/// Block IDs of the work currently in flight, protected during compaction and
/// temporary frame trimming so the request being answered is never dropped.
#[derive(Debug, Default)]
pub(crate) struct ProtectedInputs {
    ids: Mutex<HashSet<BlockId>>,
}

impl ProtectedInputs {
    /// Remember the active work's submitted input block IDs.
    pub(crate) fn set(&self, blocks: &[ContextBlock]) {
        *self.ids.lock().expect("work inputs have no callbacks") =
            blocks.iter().map(ContextBlock::id).collect();
    }

    /// Stop protecting the finished work's input blocks.
    pub(crate) fn clear(&self) {
        self.ids
            .lock()
            .expect("work inputs have no callbacks")
            .clear();
    }

    /// Snapshot of currently protected block IDs.
    pub(crate) fn snapshot(&self) -> HashSet<BlockId> {
        self.ids
            .lock()
            .expect("work inputs have no callbacks")
            .clone()
    }
}

/// Outcome of checking and optionally trimming a frame to fit the window.
pub(crate) struct TrimResult {
    /// Prepared frame (either trimmed or the minimal retainable frame).
    pub(crate) frame: ContextFrame,
    /// Whether the frame dropped or modified any blocks relative to the input.
    pub(crate) trimmed: bool,
    /// Estimated tokens of the returned frame on the caller's scale.
    pub(crate) after_tokens: u64,
}

/// Whether `estimate_tokens` plus reserved `max_tokens` exceeds `window`.
pub(crate) fn exceeds_window(estimate_tokens: u64, window: u64, max_tokens: Option<u32>) -> bool {
    let output = u64::from(max_tokens.unwrap_or(0));
    estimate_tokens.saturating_add(output) > window
}

/// Trim a copy of `blocks` when `estimate_before + max_tokens > window`.
///
/// Returns `None` when the full frame already fits or nothing after the head
/// can be dropped.
pub(crate) fn trim_frame(
    blocks: &[ContextBlock],
    window: u64,
    max_tokens: Option<u32>,
    estimate_before: u64,
    protected_ids: &HashSet<BlockId>,
) -> Option<TrimResult> {
    if !exceeds_window(estimate_before, window, max_tokens) {
        return None;
    }
    let leading_sys_end = segment::leading_system_end(blocks);
    let fixed_end = segment::fixed_prefix_end(blocks);
    let head_end = segment::head_end(blocks);
    if head_end >= blocks.len() {
        return None;
    }
    let target = window.saturating_sub(u64::from(max_tokens.unwrap_or(0)));
    let head = &blocks[..head_end];
    let head_has_user = head.iter().any(segment::is_user_message);
    let full_blocks_tokens = segment::blocks_estimate(blocks);

    let mut last_valid: Option<(Candidate, u64)> = None;
    for restart in head_end + 1..blocks.len() {
        if !segment::is_cut_point(blocks, restart) {
            continue;
        }
        let candidate = build_candidate(blocks, head_end, restart, protected_ids);
        if !candidate_valid(
            &candidate.blocks,
            leading_sys_end,
            fixed_end,
            head_end,
            head_has_user,
        ) || !has_conversation_message(&candidate.blocks, fixed_end)
        {
            continue;
        }
        let after_tokens = scaled_estimate(&candidate.blocks, estimate_before, full_blocks_tokens);
        if after_tokens <= target {
            return Some(TrimResult {
                frame: ContextFrame {
                    blocks: candidate.blocks,
                },
                trimmed: candidate.changed,
                after_tokens,
            });
        }
        last_valid = Some((candidate, after_tokens));
    }

    // Nothing fits: emit the minimal retainable frame (head, folded tool
    // declaration, and protected blocks) when it still carries conversation
    // content; otherwise fall back to the smallest valid interval candidate.
    let minimal = build_candidate(blocks, head_end, blocks.len(), protected_ids);
    if candidate_valid(
        &minimal.blocks,
        leading_sys_end,
        fixed_end,
        head_end,
        head_has_user,
    ) && has_conversation_message(&minimal.blocks, fixed_end)
    {
        let after_tokens = scaled_estimate(&minimal.blocks, estimate_before, full_blocks_tokens);
        return Some(TrimResult {
            frame: ContextFrame {
                blocks: minimal.blocks,
            },
            trimmed: minimal.changed,
            after_tokens,
        });
    }

    let (fallback, after_tokens) = last_valid?;
    fallback.changed.then_some(TrimResult {
        frame: ContextFrame {
            blocks: fallback.blocks,
        },
        trimmed: true,
        after_tokens,
    })
}

struct Candidate {
    blocks: Vec<ContextBlock>,
    changed: bool,
}

fn protected_with_latest_prelude_updates(
    blocks: &[ContextBlock],
    head_end: usize,
    restart: usize,
    protected_ids: &HashSet<BlockId>,
) -> HashSet<BlockId> {
    let mut later_sections = HashSet::new();
    for block in &blocks[restart..] {
        if let Some(sec) = block
            .meta()
            .source
            .as_deref()
            .and_then(crate::prelude::dynamic_section_from_update_source)
        {
            later_sections.insert(sec);
        }
    }
    let mut effective = protected_ids.clone();
    let mut seen_dropped = HashSet::new();
    for block in blocks[head_end..restart].iter().rev() {
        if let Some(sec) = block
            .meta()
            .source
            .as_deref()
            .and_then(crate::prelude::dynamic_section_from_update_source)
            && !later_sections.contains(&sec)
            && seen_dropped.insert(sec)
        {
            effective.insert(block.id());
        }
    }
    effective
}

fn build_candidate(
    blocks: &[ContextBlock],
    head_end: usize,
    restart: usize,
    protected_ids: &HashSet<BlockId>,
) -> Candidate {
    let head = &blocks[..head_end];
    let mut out = head.to_vec();
    let delta = segment::tool_delta(head, &blocks[..restart]);
    let has_delta = delta.is_some();
    if let Some(change) = delta {
        out.push(change);
    }
    let effective_protected =
        protected_with_latest_prelude_updates(blocks, head_end, restart, protected_ids);
    let protected = segment::protected_positions(blocks, head_end, restart, &effective_protected);
    let first_change = if has_delta {
        head_end
    } else {
        (head_end..restart)
            .find(|index| !protected.contains(index))
            .unwrap_or(restart)
    };
    let changed = has_delta || first_change < restart;

    for &index in &protected {
        push_kept_block(&mut out, &blocks[index], has_delta || index > first_change);
    }
    for (offset, block) in blocks[restart..].iter().enumerate() {
        let index = restart + offset;
        push_kept_block(&mut out, block, has_delta || index > first_change);
    }

    Candidate {
        blocks: out,
        changed,
    }
}

fn push_kept_block(out: &mut Vec<ContextBlock>, block: &ContextBlock, strip_sealed: bool) {
    if strip_sealed && sealed::has_sealed(block) {
        if let Some(stripped) = sealed::without_sealed(block) {
            out.push(stripped);
        }
    } else {
        out.push(block.clone());
    }
}

/// Reject cut points that would promote a middle system block into the leading
/// system prompt (BE27) or start a headerless conversation with a non-user block.
fn candidate_valid(
    candidate: &[ContextBlock],
    leading_sys_end: usize,
    fixed_end: usize,
    head_end: usize,
    head_has_user: bool,
) -> bool {
    if segment::leading_system_end(candidate) != leading_sys_end {
        return false;
    }
    if segment::fixed_prefix_end(candidate) != fixed_end {
        return false;
    }
    let first_after_prefix = candidate[fixed_end..]
        .iter()
        .find(|block| !matches!(block.content(), BlockContent::ToolChange(_)));
    if matches!(
        first_after_prefix.map(ContextBlock::content),
        Some(BlockContent::Message {
            role: Role::System,
            ..
        })
    ) {
        return false;
    }
    if !head_has_user {
        let first_after_head = candidate[head_end..]
            .iter()
            .find(|block| !matches!(block.content(), BlockContent::ToolChange(_)));
        if let Some(first) = first_after_head
            && !segment::is_user_message(first)
        {
            return false;
        }
    }
    true
}

/// Whether `candidate` still holds at least one conversation block (message,
/// tool call, or tool result) after the leading system prefix.
fn has_conversation_message(candidate: &[ContextBlock], fixed_end: usize) -> bool {
    candidate[fixed_end..]
        .iter()
        .any(|block| !matches!(block.content(), BlockContent::ToolChange(_)))
}

/// Estimate `candidate` on the same scale as `estimate_before` when upstream
/// calibration exceeded the raw byte-based estimate.
fn scaled_estimate(
    candidate: &[ContextBlock],
    estimate_before: u64,
    full_blocks_tokens: u64,
) -> u64 {
    let raw = segment::blocks_estimate(candidate);
    if estimate_before > full_blocks_tokens && full_blocks_tokens > 0 {
        let scaled = (u128::from(raw) * u128::from(estimate_before))
            .div_ceil(u128::from(full_blocks_tokens));
        u64::try_from(scaled).unwrap_or(u64::MAX).max(raw)
    } else {
        raw
    }
}
