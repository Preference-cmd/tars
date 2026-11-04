//! Brief instruction, brief request material and brief block building.
//!
//! The brief call uses the session's own model, tools and render options
//! through the same public gateway and keeps only the returned text. Its
//! material is the normal request up to the cut point `k` followed by one
//! instruction, so almost all of it hits the provider's prompt cache. The
//! brief that comes back is one user-role message marked with
//! `tars:compaction`; [`super::segment`] decides where it goes.

use super::segment;
use causa::kernel::{BlockContent, BlockId, BlockMeta, ContextBlock, Role};

/// Upper bound on generated summary tokens; the session output limit wins when
/// it is smaller. Candidates keep this a constant until real data exists.
pub(crate) const SUMMARY_MAX_TOKENS: u32 = 16_000;

/// Fixed prefix of the block that carries the summary to the model.
pub(crate) const SUMMARY_PREFIX: &str = "The conversation before this point was compacted into the working brief below. The original messages can be read back with the `recall` tool using the id at the end of the brief.\n\n";

/// Block provenance marking a TARS-written summary; only TARS reads it.
pub const SUMMARY_SOURCE: &str = "tars:compaction";

/// Structured instruction appended after the material to summarize.
///
/// `request_in_progress` is set when the current work's request lies in the
/// summarized interval; it stays verbatim after the brief, so the model is
/// told not to copy it and to say what of it is already done.
pub(crate) fn instruction(request_in_progress: bool) -> String {
    let mut text = String::from(
        "Prepare a continuation brief for the conversation above. Rewrite it as a compact, self-contained working brief that lets you continue the task without seeing the original messages. Earlier briefs above are background: cover only the conversation after them and do not repeat them. The most recent messages are kept verbatim after the brief.\n\n\
         Use these sections:\n\
         - Goal: what the user wants and any constraints.\n\
         - Decisions: what was decided and why.\n\
         - Progress: completed, in progress and blocked work.\n\
         - Next steps: what remains to be done.\n\
         - Relevant files: paths, commands, identifiers and error text, reproduced verbatim.\n\
         - Important context: anything else needed to continue.\n\n\
         Rules:\n\
         - Preserve paths, commands, identifiers and error text exactly.\n\
         - Quote any question the user asked that is still unanswered.\n\
         - Do not mention that this is a summary or that the conversation was condensed.\n\
         - Output only the brief.",
    );
    if request_in_progress {
        text.push_str(
            "\n\nThe last user message above is the request still in progress. It stays verbatim right after the brief, so do not copy it. End the brief with a section \"In progress\" listing what of that request is already done.",
        );
    }
    text
}

/// Material of the brief request: `blocks[..k]`, trimmed when it is over
/// `limit` tokens.
///
/// Trimming never drops the head or a protected block. It drops the earliest
/// interval blocks up to a cut point, so no kept result loses its call, and
/// declares the tool changes the dropped part made. When the head carries no
/// user message, the kept part starts at one, as providers expect. When
/// nothing fits, only the head, the declaration and the protected blocks are
/// sent. The dropped blocks are still summarized away and kept in the record.
pub(crate) fn request_material(
    blocks: &[ContextBlock],
    head_end: usize,
    k: usize,
    protected: &[usize],
    limit: Option<u64>,
) -> Vec<ContextBlock> {
    let full = &blocks[..k];
    let Some(limit) = limit.filter(|&limit| segment::blocks_estimate(full) > limit) else {
        return full.to_vec();
    };
    let head = &blocks[..head_end];
    let head_has_user = head.iter().any(is_user_message);
    let candidate = |restart: usize| {
        let mut material = head.to_vec();
        material.extend(segment::tool_delta(head, &blocks[..restart]));
        material.extend(
            protected
                .iter()
                .filter(|&&index| index < restart)
                .map(|&index| blocks[index].clone()),
        );
        material.extend_from_slice(&blocks[restart..k]);
        material
    };
    for restart in head_end + 1..k {
        if !segment::is_cut_point(blocks, restart) {
            continue;
        }
        let material = candidate(restart);
        let first_message = material[head_end..]
            .iter()
            .find(|block| matches!(block.content(), BlockContent::Message { .. }));
        if !head_has_user && !first_message.is_some_and(is_user_message) {
            continue;
        }
        if segment::blocks_estimate(&material) <= limit {
            return material;
        }
    }
    candidate(k)
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

/// The instruction message appended after the material.
pub(crate) fn instruction_block(text: String) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, text),
        BlockMeta::default(),
    )
}

/// The brief message.
pub(crate) fn summary_block(id: BlockId, text: &str) -> ContextBlock {
    ContextBlock::new(
        id,
        BlockContent::text(Role::User, format!("{SUMMARY_PREFIX}{text}")),
        BlockMeta::from_source(SUMMARY_SOURCE),
    )
}

/// The brief text followed by the recall pointer line.
pub(crate) fn summary_text(brief: &str, record_id: &str) -> String {
    format!("{brief}\n\nOriginal messages: recall id={record_id}")
}

/// Fresh block identity for brief blocks.
pub(crate) fn new_block_id() -> BlockId {
    BlockId::new(uuid::Uuid::now_v7())
}
