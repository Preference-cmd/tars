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
                if let Some(artifact) = &result.output.artifact {
                    check(block.id(), MaterialReference::Artifact(artifact))?;
                }
            }
            BlockContent::ToolCall(_) | BlockContent::ToolChange(_) => {}
        }
    }
    Ok(())
}
