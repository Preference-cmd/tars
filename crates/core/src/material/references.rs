use causa::kernel::{
    ArtifactRef, BlockContent, BlockId, ContentPart, Context, ContextBlock, MediaRef, TextPayload,
};
use std::io;

use super::MaterialError;

/// Note added to a tool result when its history artifact reference is detached during export.
pub const DETACHED_HISTORY_NOTE: &str =
    "Full original stays in the original session and cannot be read back here.";

/// A borrowed external asset whose bytes are not contained in the material file.
#[derive(Debug, Clone, Copy)]
pub enum MaterialReference<'a> {
    /// Media occurring in a message or a tool result.
    Media(&'a MediaRef),
    /// Out-of-band tool output, such as a spilled full result.
    Artifact(&'a ArtifactRef),
}

type ReferenceValidator<'a> = dyn FnMut(MaterialReference<'_>) -> io::Result<()> + 'a;

pub(crate) fn validate(
    context: &Context,
    validator: Option<&mut ReferenceValidator<'_>>,
) -> Result<(), MaterialError> {
    validate_with_history(context, None, validator)
}

/// Like [`validate`], but a tool result's artifact reference to an original
/// block of a rewrite record listed in `history` is accepted without reading
/// any file. Every other reference goes through `validator` as usual.
pub(crate) fn validate_with_history(
    context: &Context,
    history: Option<&[crate::compact::HistoryRecordEntry]>,
    mut validator: Option<&mut ReferenceValidator<'_>>,
) -> Result<(), MaterialError> {
    let mut check = |block_id: BlockId, reference| match validator.as_mut() {
        None => Err(MaterialError::ReferenceValidationRequired { block_id }),
        Some(validate) => validate(reference)
            .map_err(|source| MaterialError::ReferenceUnavailable { block_id, source }),
    };
    for block in context.blocks() {
        match block.content() {
            BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let ContentPart::Media(media) = part {
                        check(block.id(), MaterialReference::Media(media))?;
                    }
                }
            }
            BlockContent::ToolResult(result) => {
                for media in &result.media {
                    check(block.id(), MaterialReference::Media(media))?;
                }
                if let Some(artifact) = &result.output.artifact
                    && !history.is_some_and(|history| indexed(history, &artifact.id))
                {
                    check(block.id(), MaterialReference::Artifact(artifact))?;
                }
            }
            BlockContent::ToolCall(_) | BlockContent::ToolChange(_) => {}
        }
    }
    Ok(())
}

/// Whether `id` is `<record id>/<block id>` with the record in `history`.
pub(crate) fn indexed(history: &[crate::compact::HistoryRecordEntry], id: &str) -> bool {
    crate::compact::history::parse_artifact_id(id)
        .is_ok_and(|(record_id, _)| history.iter().any(|entry| entry.record_id == record_id))
}

/// Return a copy of `context` with all artifact references to records in `history` detached.
///
/// Every tool result whose `output.artifact.id` parses as `<record id>/<block id>` with
/// `record_id` listed in `history` has:
/// - `output.artifact = None`;
/// - `content`, `truncation`, `meta`, `media`, `status`, `call_block_id` and existing `notes` unchanged;
/// - one added note ([`DETACHED_HISTORY_NOTE`]) stating that the full original stays in the original
///   session and cannot be read back here.
///
/// Block IDs and ordering are unchanged. References to records not in `history` and media references
/// are left untouched. Calling this operation multiple times is idempotent (no duplicate note added).
pub fn detach_history_references(
    context: &Context,
    history: &[crate::compact::HistoryRecordEntry],
) -> Context {
    if context.blocks().is_empty() {
        return Context::new();
    }
    let new_blocks: Vec<ContextBlock> = context
        .blocks()
        .iter()
        .map(|block| match block.content() {
            BlockContent::ToolResult(result) => {
                if let Some(artifact) = &result.output.artifact
                    && indexed(history, &artifact.id)
                {
                    let mut detached = result.clone();
                    detached.output.artifact = None;
                    if !detached
                        .notes
                        .iter()
                        .any(|note| note.0 == DETACHED_HISTORY_NOTE)
                    {
                        detached.notes.push(TextPayload::new(DETACHED_HISTORY_NOTE));
                    }
                    ContextBlock::new(
                        block.id(),
                        BlockContent::ToolResult(detached),
                        block.meta().clone(),
                    )
                } else {
                    block.clone()
                }
            }
            _ => block.clone(),
        })
        .collect();
    Context::from_blocks(new_blocks).expect("detaching artifact references preserves valid context")
}
