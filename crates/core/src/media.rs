//! Safe multimodal image ingestion, content-addressed media storage, and resolver wiring.
//!
//! Supported raster images (PNG, JPEG, GIF, WebP) are identified strictly by
//! magic bytes, bounded by per-image, per-message, and per-session limits, and
//! addressed by `media:sha256:<64-lowercase-hex>` references.

mod types;
mod vault;

pub use types::{
    GATEWAY_INLINE_IMAGE_LIMIT_BYTES, MAX_IMAGE_BYTES, MAX_IMAGES_PER_MESSAGE,
    MAX_SESSION_MEDIA_BYTES, MEDIA_REF_PREFIX, MediaError, PreparedImage,
    collect_blocks_media_refs, collect_context_media_refs, detect_image_mime,
    is_artifact_media_ref, is_managed_media_ref, is_supported_image_mime, multimodal_input,
    parse_managed_media_sha256, prepare_image_from_bytes, prepare_image_from_path,
    validate_managed_media_ref,
};
pub(crate) use types::{collect_context_managed_media_refs, verify_managed_blob};
pub use vault::{ManagedMediaBlob, MediaVault};

#[cfg(test)]
mod tests;
