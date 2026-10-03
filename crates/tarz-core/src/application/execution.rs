use causa::kernel::{BlockContent, BlockMeta, ContentPart, ContextBlock, TextPayload};
use causa::runtime::new_block_id;

/// Create a fresh identified user text block without changing existing materials.
pub fn text_input(text: impl Into<String>) -> ContextBlock {
    ContextBlock::new(
        new_block_id(),
        BlockContent::Parts(vec![ContentPart::Text(TextPayload::new(text))]),
        BlockMeta {
            source: Some("user".into()),
            ..Default::default()
        },
    )
}
