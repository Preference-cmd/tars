//! Summary instruction, request input trimming and replacement block building.
//!
//! The summary call uses the session's own model and render options through the
//! same public gateway, adds no tools and returns plain text. A summary replaces
//! the whole Context: one tool declaration matching the bound tools plus one
//! user-role message marked with `tars:compaction`.

use causa::kernel::{
    BlockContent, BlockId, BlockMeta, ContextBlock, Role, ToolChange, ToolDefinition, current_tools,
};

/// Upper bound on generated summary tokens; the session output limit wins when
/// it is smaller. Candidates keep this a constant until real data exists.
pub(crate) const SUMMARY_MAX_TOKENS: u32 = 16_000;

/// Fixed prefix of the block that carries the summary to the model.
pub(crate) const SUMMARY_PREFIX: &str = "The conversation before this point was compacted into the working brief below. It is the only record of the earlier messages.\n\n";

/// Block provenance marking a TARS-written summary; only TARS reads it.
pub const SUMMARY_SOURCE: &str = "tars:compaction";

/// Structured instruction appended after the old material.
pub(crate) fn instruction(current_request: Option<&str>) -> String {
    let mut text = String::from(
        "Prepare a continuation brief for the conversation above. Rewrite it as a compact, self-contained working brief that lets you continue the task without seeing the original messages.\n\n\
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
    if let Some(request) = current_request {
        text.push_str("\n\nThe user's current request, verbatim:\n---\n");
        text.push_str(request);
        text.push_str(
            "\n---\nMake sure the brief gives you everything needed to continue this request.",
        );
    }
    text
}

/// Token estimate of one block list, serialized the same way as the context.
fn blocks_estimate(blocks: &[ContextBlock]) -> u64 {
    let mut bytes: u64 = 0;
    for block in blocks {
        let size = serde_json::to_vec(block)
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        bytes = bytes.saturating_add(size);
    }
    super::estimate::estimate_bytes(bytes)
}

/// Drop the earliest blocks until the input fits `limit` tokens.
///
/// A tool result whose declaration was dropped is dropped too, so the summary
/// request keeps valid pairing. When nothing fits, the instruction alone is sent.
pub(crate) fn trim_to_limit(blocks: &[ContextBlock], limit: u64) -> Vec<ContextBlock> {
    if blocks_estimate(blocks) <= limit {
        return blocks.to_vec();
    }
    let sizes: Vec<u64> = blocks
        .iter()
        .map(|block| {
            serde_json::to_vec(block)
                .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
                .unwrap_or(u64::MAX)
        })
        .collect();
    let mut total: u64 = sizes.iter().copied().sum();
    let mut start = 0usize;
    while start < blocks.len() && total.div_ceil(3) > limit {
        total = total.saturating_sub(sizes[start]);
        start += 1;
    }
    let retained = &blocks[start..];
    // A dropped declaration leaves its later result unpaired; drop those too.
    let retained_calls = retained
        .iter()
        .filter_map(|block| match block.content() {
            BlockContent::ToolCall(_) => Some(block.id()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();
    retained
        .iter()
        .filter(|block| match block.content() {
            BlockContent::ToolResult(result) => retained_calls.contains(&result.call_block_id),
            _ => true,
        })
        .cloned()
        .collect()
}

/// A fresh tool declaration equal to the current effective set, if any.
pub(crate) fn tool_change_block(blocks: &[ContextBlock]) -> Option<ContextBlock> {
    let tools: Vec<ToolDefinition> = current_tools(blocks);
    if tools.is_empty() {
        return None;
    }
    Some(ContextBlock::new(
        new_block_id(),
        BlockContent::ToolChange(ToolChange::between(&[], &tools)),
        BlockMeta::default(),
    ))
}

/// The instruction message appended after the old material.
pub(crate) fn instruction_block(text: String) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, text),
        BlockMeta::default(),
    )
}

/// The replacement summary message.
pub(crate) fn summary_block(id: BlockId, text: &str) -> ContextBlock {
    ContextBlock::new(
        id,
        BlockContent::text(Role::User, format!("{SUMMARY_PREFIX}{text}")),
        BlockMeta::from_source(SUMMARY_SOURCE),
    )
}

/// Build the replacement block list: optional tool declaration plus summary.
pub(crate) fn replacement_blocks(
    old_blocks: &[ContextBlock],
    summary_id: BlockId,
    summary_text: &str,
) -> Vec<ContextBlock> {
    let mut blocks = Vec::with_capacity(2);
    if let Some(tool_change) = tool_change_block(old_blocks) {
        blocks.push(tool_change);
    }
    blocks.push(summary_block(summary_id, summary_text));
    blocks
}

/// Fresh block identity for summary and tool-declaration blocks.
pub(crate) fn new_block_id() -> BlockId {
    BlockId::new(uuid::Uuid::now_v7())
}
