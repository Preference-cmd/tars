use causa::kernel::{ArtifactRef, BlockContent, BlockId, ContentPart, Context, MediaRef};
use std::io;

use super::MaterialError;

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
fn indexed(history: &[crate::compact::HistoryRecordEntry], id: &str) -> bool {
    crate::compact::history::parse_artifact_id(id)
        .is_ok_and(|(record_id, _)| history.iter().any(|entry| entry.record_id == record_id))
}
