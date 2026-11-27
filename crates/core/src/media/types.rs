use causa::kernel::{BlockContent, BlockMeta, ContentPart, Context, ContextBlock, MediaRef, Role};
use causa::providers::MediaKind;
use causa::runtime::new_block_id;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum raw byte size accepted for a single image (5 MiB).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Maximum number of images allowed in a single user message.
pub const MAX_IMAGES_PER_MESSAGE: usize = 8;

/// Maximum cumulative deduplicated raw image bytes retained per session (64 MiB).
pub const MAX_SESSION_MEDIA_BYTES: usize = 64 * 1024 * 1024;

/// Prefix for content-addressed managed image references.
pub const MEDIA_REF_PREFIX: &str = "media:sha256:";

/// Inline image ceiling configured on Causa's `KernelHttpGateway`.
///
/// Causa 0.3.0 estimates decoded length as `data_base64.len() * 3 / 4 <= ceiling`.
/// An exact 5 MiB (`5_242_880` bytes, `len % 3 == 2`) payload encodes to `6_990_508`
/// base64 chars, which evaluates to `5_242_881` under that formula (+1 byte due to
/// padding). Adding 3 bytes prevents an exact 5 MiB image from degrading into a
/// text placeholder while TARS's ingestion limit remains strictly `5 MiB`.
pub const GATEWAY_INLINE_IMAGE_LIMIT_BYTES: usize = MAX_IMAGE_BYTES + 3;

const MAX_IMAGE_NAME_CHARS: usize = 120;

/// Structured failures for media validation, ingestion, quota, and resolution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum MediaError {
    /// Input image file or byte slice is empty.
    #[error("image file is empty")]
    EmptyFile,
    /// File header did not match PNG, JPEG, GIF, or WebP magic bytes.
    #[error("unsupported or unrecognized image format (supported: PNG, JPEG, GIF, WebP)")]
    UnsupportedFormat,
    /// Image exceeds [`MAX_IMAGE_BYTES`].
    #[error("image size ({size_bytes} bytes) exceeds maximum allowed ({max_bytes} bytes)")]
    FileTooLarge {
        /// Observed size in bytes.
        size_bytes: u64,
        /// Configured single-image byte ceiling.
        max_bytes: usize,
    },
    /// Message includes more than [`MAX_IMAGES_PER_MESSAGE`] images.
    #[error("too many images in one message ({count} > max {max_images})")]
    TooManyImages {
        /// Number of images supplied.
        count: usize,
        /// Maximum allowed per message.
        max_images: usize,
    },
    /// Adding the batch would exceed [`MAX_SESSION_MEDIA_BYTES`].
    #[error(
        "session media quota exceeded: current {current_bytes} + additional {additional_bytes} > max {max_bytes} bytes"
    )]
    SessionQuotaExceeded {
        /// Deduplicated bytes currently held by the session.
        current_bytes: usize,
        /// New deduplicated bytes required by this batch.
        additional_bytes: usize,
        /// Configured per-session byte ceiling.
        max_bytes: usize,
    },
    /// Path does not refer to a regular file (e.g. directory, FIFO, or device).
    #[error("image path is not a regular file: {}", path.display())]
    NotRegularFile {
        /// Path supplied by the caller.
        path: PathBuf,
    },
    /// Media reference or MIME type is outside TARS's managed contract.
    #[error("invalid media reference '{reference}': {reason}")]
    InvalidReference {
        /// Reference string that failed validation.
        reference: String,
        /// Why the reference was rejected.
        reason: String,
    },
    /// Referenced managed media blob is absent from storage or vault.
    #[error("missing managed media blob for reference '{reference}'")]
    MissingMedia {
        /// Missing content-addressed reference.
        reference: String,
    },
    /// Stored or sidecar media blob failed integrity, size, or magic-byte verification.
    #[error("corrupt media blob for reference '{reference}': {reason}")]
    CorruptMedia {
        /// Reference associated with the blob.
        reference: String,
        /// Integrity check failure reason.
        reason: String,
    },
    /// Input contains neither non-empty text nor any image.
    #[error("input must contain non-empty text or at least one image")]
    EmptyInput,
    /// Underlying filesystem I/O failure.
    #[error("media I/O failed: {0}")]
    Io(#[source] Arc<io::Error>),
}

impl From<io::Error> for MediaError {
    fn from(source: io::Error) -> Self {
        Self::Io(Arc::new(source))
    }
}

/// Verified raster image ready to be admitted into a session [`crate::media::MediaVault`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedImage {
    /// Magic-byte verified MIME type (`image/png`, `image/jpeg`, `image/gif`, or `image/webp`).
    pub media_type: &'static str,
    /// Content-addressed reference (`media:sha256:<64-lowercase-hex>`).
    pub reference: String,
    /// Sanitized basename for display, if non-empty after filtering control characters.
    pub name: Option<String>,
    /// Verified raw image bytes.
    pub bytes: Arc<[u8]>,
    /// Exact raw byte length (`bytes.len()`).
    pub size_bytes: usize,
}

impl PreparedImage {
    /// Convert this prepared image into a Causa [`MediaRef`] carrying no raw bytes.
    pub fn media_ref(&self) -> MediaRef {
        let mut media_ref = MediaRef::new(self.media_type, &self.reference);
        if let Some(name) = &self.name {
            media_ref = media_ref.with_name(name);
        }
        media_ref
    }
}

/// Detect supported raster image MIME types strictly from file header magic bytes.
///
/// Recognizes PNG, JPEG, GIF87a/GIF89a, and WebP (`RIFF....WEBP`). Never trusts
/// file extensions and returns `None` for SVG, text, truncated headers, or any
/// other format.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xFF\xD8\xFF") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Validate raw image bytes and build a content-addressed [`PreparedImage`].
pub fn prepare_image_from_bytes(
    bytes: Vec<u8>,
    name: Option<&str>,
) -> Result<PreparedImage, MediaError> {
    if bytes.is_empty() {
        return Err(MediaError::EmptyFile);
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(MediaError::FileTooLarge {
            size_bytes: bytes.len() as u64,
            max_bytes: MAX_IMAGE_BYTES,
        });
    }
    let media_type = detect_image_mime(&bytes).ok_or(MediaError::UnsupportedFormat)?;
    let reference = format!("{MEDIA_REF_PREFIX}{}", sha256_hex(&bytes));
    let sanitized_name = name.and_then(sanitize_image_name);
    let size_bytes = bytes.len();
    Ok(PreparedImage {
        media_type,
        reference,
        name: sanitized_name,
        bytes: Arc::from(bytes),
        size_bytes,
    })
}

/// Safely read a local regular image file with bounded I/O and build a [`PreparedImage`].
///
/// Rejects non-regular files (directories, FIFOs, devices) before opening to avoid
/// blocking on special files, re-verifies metadata on the opened handle, and caps
/// the read at `MAX_IMAGE_BYTES + 1` bytes.
pub fn prepare_image_from_path(path: impl AsRef<Path>) -> Result<PreparedImage, MediaError> {
    let path = path.as_ref();
    let pre_meta = std::fs::metadata(path)?;
    if !pre_meta.is_file() {
        return Err(MediaError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    if pre_meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(MediaError::FileTooLarge {
            size_bytes: pre_meta.len(),
            max_bytes: MAX_IMAGE_BYTES,
        });
    }

    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(MediaError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(MediaError::FileTooLarge {
            size_bytes: meta.len(),
            max_bytes: MAX_IMAGE_BYTES,
        });
    }

    let cap = usize::try_from(meta.len())
        .unwrap_or(MAX_IMAGE_BYTES)
        .min(MAX_IMAGE_BYTES);
    let mut bytes = Vec::with_capacity(cap);
    file.take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(MediaError::FileTooLarge {
            size_bytes: bytes.len() as u64,
            max_bytes: MAX_IMAGE_BYTES,
        });
    }

    let name = path.file_name().and_then(|s| s.to_str());
    prepare_image_from_bytes(bytes, name)
}

/// Extract the 64-character lowercase SHA-256 hex digest from a `media:sha256:<hex>` reference.
pub fn parse_managed_media_sha256(reference: &str) -> Option<&str> {
    let hex = reference.strip_prefix(MEDIA_REF_PREFIX)?;
    if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Some(hex)
    } else {
        None
    }
}

/// Return whether `mime` is one of the four supported raster image MIME types.
pub fn is_supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Return whether `reference` is a canonical `media:sha256:<64hex>` reference.
pub fn is_managed_media_ref(reference: &str) -> bool {
    parse_managed_media_sha256(reference).is_some()
}

/// Return whether `reference` is a canonical `artifact:<uuid>` reference.
pub fn is_artifact_media_ref(reference: &str) -> bool {
    crate::compact::history::parse_spilled_artifact_id(reference).is_ok()
}

/// Validate that a [`MediaRef`] uses a managed reference scheme and compatible MIME type.
pub fn validate_managed_media_ref(media: &MediaRef) -> Result<(), MediaError> {
    if is_managed_media_ref(&media.reference) {
        if is_supported_image_mime(&media.media_type) {
            return Ok(());
        }
        return Err(MediaError::InvalidReference {
            reference: media.reference.clone(),
            reason: format!(
                "unsupported managed image media_type '{}'",
                media.media_type
            ),
        });
    }
    if is_artifact_media_ref(&media.reference) {
        if MediaKind::of(&media.media_type).is_some() {
            return Ok(());
        }
        return Err(MediaError::InvalidReference {
            reference: media.reference.clone(),
            reason: format!("unsupported artifact media_type '{}'", media.media_type),
        });
    }
    Err(MediaError::InvalidReference {
        reference: media.reference.clone(),
        reason: "only managed media:sha256:<64hex> and artifact:<uuid> references are permitted"
            .into(),
    })
}

/// Collect all media reference strings appearing in `ContentPart::Media` and `ToolResult.media`.
pub fn collect_blocks_media_refs(blocks: &[ContextBlock]) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    for block in blocks {
        match block.content() {
            BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let ContentPart::Media(media) = part {
                        refs.insert(media.reference.clone());
                    }
                }
            }
            BlockContent::ToolResult(result) => {
                for media in &result.media {
                    refs.insert(media.reference.clone());
                }
            }
            BlockContent::ToolCall(_) | BlockContent::ToolChange(_) => {}
        }
    }
    refs
}

/// Collect all media reference strings appearing in `context`.
pub fn collect_context_media_refs(context: &Context) -> BTreeSet<String> {
    collect_blocks_media_refs(context.blocks())
}

pub(crate) fn collect_blocks_managed_media_refs(blocks: &[ContextBlock]) -> BTreeSet<String> {
    collect_blocks_media_refs(blocks)
        .into_iter()
        .filter(|reference| is_managed_media_ref(reference))
        .collect()
}

pub(crate) fn collect_context_managed_media_refs(context: &Context) -> BTreeSet<String> {
    collect_blocks_managed_media_refs(context.blocks())
}

/// Construct a user message [`ContextBlock`] carrying optional text and/or managed images.
///
/// Rejects messages with more than [`MAX_IMAGES_PER_MESSAGE`] images, invalid
/// unmanaged media references, or neither non-empty text nor any image.
pub fn multimodal_input(
    text: Option<&str>,
    images: &[MediaRef],
) -> Result<ContextBlock, MediaError> {
    if images.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(MediaError::TooManyImages {
            count: images.len(),
            max_images: MAX_IMAGES_PER_MESSAGE,
        });
    }
    for media in images {
        validate_managed_media_ref(media)?;
    }

    let text = text.filter(|value| !value.is_empty());
    if text.is_none() && images.is_empty() {
        return Err(MediaError::EmptyInput);
    }

    let mut parts = Vec::with_capacity(usize::from(text.is_some()) + images.len());
    if let Some(text) = text {
        parts.push(ContentPart::Text(causa::kernel::TextPayload::new(text)));
    }
    for media in images {
        parts.push(ContentPart::Media(media.clone()));
    }

    Ok(ContextBlock::new(
        new_block_id(),
        BlockContent::Message {
            role: Role::User,
            parts,
        },
        BlockMeta::from_source("user"),
    ))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

pub(crate) fn verify_managed_blob(
    reference: &str,
    bytes: &[u8],
) -> Result<&'static str, MediaError> {
    let expected_hex =
        parse_managed_media_sha256(reference).ok_or_else(|| MediaError::InvalidReference {
            reference: reference.to_owned(),
            reason: "expected media:sha256:<64-lowercase-hex>".into(),
        })?;
    if bytes.is_empty() {
        return Err(MediaError::CorruptMedia {
            reference: reference.to_owned(),
            reason: "media blob is empty".into(),
        });
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(MediaError::CorruptMedia {
            reference: reference.to_owned(),
            reason: format!(
                "media blob size ({} bytes) exceeds limit ({MAX_IMAGE_BYTES} bytes)",
                bytes.len()
            ),
        });
    }
    let mime = detect_image_mime(bytes).ok_or_else(|| MediaError::CorruptMedia {
        reference: reference.to_owned(),
        reason: "unrecognized image magic bytes".into(),
    })?;
    let actual_hex = sha256_hex(bytes);
    if actual_hex != expected_hex {
        return Err(MediaError::CorruptMedia {
            reference: reference.to_owned(),
            reason: format!("SHA-256 digest mismatch (expected {expected_hex}, got {actual_hex})"),
        });
    }
    Ok(mime)
}

fn sanitize_image_name(raw: &str) -> Option<String> {
    let base = raw
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())?;
    let filtered: String = base
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_IMAGE_NAME_CHARS)
        .collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}
