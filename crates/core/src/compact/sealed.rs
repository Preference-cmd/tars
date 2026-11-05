//! Sealed provider items, such as thinking, after a rewrite.
//!
//! A model's reply can carry wire items TARS cannot read: Anthropic thinking
//! blocks, Responses reasoning items, Chat reasoning fields. Causa keeps them
//! as sealed parts of assistant blocks and sends them back verbatim. Some are
//! bound to the request prefix they were produced after, so once a rewrite
//! changes that prefix they can no longer be sent. Every rewrite therefore
//! removes the sealed parts of the assistant blocks after its first change,
//! in the same edit and record.
//!
//! The reply the Context ends on while its tool round is still open is the
//! exception: providers expect it back whole, so a summary covers it instead
//! of stripping it (see [`tail_start_outside_open_loop`]).

use super::history::PlannedReplacement;
use super::segment;
use causa::kernel::{BlockContent, BlockId, ContentPart, ContextBlock, Role};

/// Whether a block is an assistant message that carries sealed items.
pub(crate) fn has_sealed(block: &ContextBlock) -> bool {
    matches!(
        block.content(),
        BlockContent::Message { role: Role::Assistant, parts }
            if parts.iter().any(|part| matches!(part, ContentPart::Sealed(_)))
    )
}

/// The block without its sealed parts, under a new id, or none when nothing
/// else is left (a block that held only thinking).
pub(crate) fn without_sealed(block: &ContextBlock) -> Option<ContextBlock> {
    let BlockContent::Message { role, parts } = block.content() else {
        return Some(block.clone());
    };
    let parts: Vec<ContentPart> = parts
        .iter()
        .filter(|part| !matches!(part, ContentPart::Sealed(_)))
        .cloned()
        .collect();
    (!parts.is_empty()).then(|| {
        ContextBlock::new(
            BlockId::new(uuid::Uuid::now_v7()),
            BlockContent::Message { role: *role, parts },
            block.meta().clone(),
        )
    })
}

/// Start of the retained tail once the open tool round is kept out of it.
///
/// While the Context ends on tool results, the replies since the last user
/// message form the round in progress. Providers expect its sealed items back
/// unchanged (Anthropic requires the last reply of a tool round to start with
/// its thinking), yet a rewrite before them changes their prefix. So when any
/// of those replies at or after `k` carries sealed items, the tail starts at
/// the first cut point after the last of them, and the summary covers the
/// rest of the round. Otherwise `k` stands.
pub(crate) fn tail_start_outside_open_loop(blocks: &[ContextBlock], k: usize) -> usize {
    if !blocks
        .last()
        .is_some_and(|block| matches!(block.content(), BlockContent::ToolResult(_)))
    {
        return k;
    }
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
    let Some(last_sealed) = (k.max(loop_start)..blocks.len())
        .rev()
        .find(|&index| has_sealed(&blocks[index]))
    else {
        return k;
    };
    (last_sealed + 1..=blocks.len())
        .find(|&index| segment::is_cut_point(blocks, index))
        .unwrap_or(blocks.len())
}

/// Add to `replacements` the removal of sealed parts from every block from
/// `from` on that no replacement already covers.
///
/// A block that keeps other parts becomes a one-block range with a new id. A
/// block left empty joins the range just before it when one ends there;
/// otherwise the block before it is put back unchanged as the range's only
/// inserted block, the shape expansion already reads for a protected block.
/// `replacements` must be non-empty and its first range must start at `from`
/// (the first change). `replacements` must be ascending and non-overlapping;
/// so is the result.
pub(crate) fn strip_after(
    blocks: &[ContextBlock],
    from: usize,
    replacements: Vec<PlannedReplacement>,
) -> Vec<PlannedReplacement> {
    debug_assert!(
        !replacements.is_empty() && replacements[0].range.start == from,
        "`replacements` must be non-empty and start at `from`",
    );
    let mut out: Vec<PlannedReplacement> = Vec::with_capacity(replacements.len());
    let mut planned = replacements.into_iter().peekable();
    let mut index = from;
    while index < blocks.len() {
        if let Some(next) = planned.next_if(|next| next.range.start <= index) {
            index = index.max(next.range.end);
            out.push(next);
            continue;
        }
        let block = &blocks[index];
        if has_sealed(block) {
            match without_sealed(block) {
                Some(stripped) => out.push(PlannedReplacement {
                    range: index..index + 1,
                    with: vec![stripped],
                }),
                None => match out.last_mut() {
                    Some(last) if last.range.end == index => last.range.end = index + 1,
                    _ if index > from && !has_sealed(&blocks[index - 1]) => {
                        out.push(PlannedReplacement {
                            range: index - 1..index + 1,
                            with: vec![blocks[index - 1].clone()],
                        })
                    }
                    // Unreachable under the precondition: leave the block unchanged.
                    _ => {}
                },
            }
        }
        index += 1;
    }
    out.extend(planned);
    out
}

/// Serialized bytes a set of replacements removes and inserts. A block put
/// back counts on both sides.
pub(crate) fn replaced_bytes(
    blocks: &[ContextBlock],
    replacements: &[PlannedReplacement],
) -> (u64, u64) {
    replacements
        .iter()
        .fold((0u64, 0u64), |(removed, inserted), replacement| {
            (
                removed.saturating_add(segment::blocks_bytes(&blocks[replacement.range.clone()])),
                inserted.saturating_add(segment::blocks_bytes(&replacement.with)),
            )
        })
}
