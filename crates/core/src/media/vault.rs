use super::types::{
    MAX_IMAGE_BYTES, MAX_IMAGES_PER_MESSAGE, MAX_SESSION_MEDIA_BYTES, MediaError, PreparedImage,
    is_managed_media_ref, is_supported_image_mime, parse_managed_media_sha256, verify_managed_blob,
};
use crate::compact::history::{HistoryStore, parse_spilled_artifact_id};
use base64::Engine as _;
use causa::kernel::MediaRef;
use causa::providers::{MediaKind, MediaPayload, MediaResolver};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, OnceLock, RwLock};

/// Content-addressed media reference paired with its verified raw image bytes.
pub type ManagedMediaBlob = (String, Arc<[u8]>);

struct MediaEntry {
    media_type: &'static str,
    bytes: Arc<[u8]>,
    base64: OnceLock<String>,
}

#[derive(Default)]
struct VaultState {
    entries: HashMap<String, MediaEntry>,
    total_bytes: usize,
}

struct VaultInner {
    state: RwLock<VaultState>,
    history_store: RwLock<Option<Arc<HistoryStore>>>,
}

/// Thread-safe, cloneable in-memory media vault backing one session instance.
///
/// Serves both user-submitted `media:sha256:<64hex>` images from memory and
/// MCP-produced `artifact:<uuid>` media lazily from the bound history store.
#[derive(Clone)]
pub struct MediaVault {
    inner: Arc<VaultInner>,
}

impl Default for MediaVault {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MediaVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (count, total_bytes) = match self.inner.state.read() {
            Ok(state) => (state.entries.len(), state.total_bytes),
            Err(_) => (0, 0),
        };
        f.debug_struct("MediaVault")
            .field("entries", &count)
            .field("total_bytes", &total_bytes)
            .finish_non_exhaustive()
    }
}

impl MediaVault {
    /// Construct an empty media vault with no bound history store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(VaultInner {
                state: RwLock::new(VaultState::default()),
                history_store: RwLock::new(None),
            }),
        }
    }

    /// Bind the session's [`HistoryStore`] for resolving MCP `artifact:<uuid>` media.
    pub(crate) fn bind_history_store(&self, store: Arc<HistoryStore>) {
        if let Ok(mut slot) = self.inner.history_store.write() {
            *slot = Some(store);
        }
    }

    /// Return whether `reference` is currently present in this vault.
    pub fn contains(&self, reference: &str) -> bool {
        self.inner
            .state
            .read()
            .is_ok_and(|state| state.entries.contains_key(reference))
    }

    /// Number of unique managed media blobs held in memory.
    pub fn len(&self) -> usize {
        self.inner
            .state
            .read()
            .map_or(0, |state| state.entries.len())
    }

    /// Return whether the vault holds no managed media blobs.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Total deduplicated raw bytes currently held in memory.
    pub fn total_bytes(&self) -> usize {
        self.inner.state.read().map_or(0, |state| state.total_bytes)
    }

    /// Atomically insert a batch of prepared images if message and session quotas allow.
    pub fn insert_batch(&self, images: &[PreparedImage]) -> Result<Vec<MediaRef>, MediaError> {
        let (refs, _) = self.insert_batch_with_added(images)?;
        Ok(refs)
    }

    /// Atomically insert a batch of prepared images and return `(refs, newly_added_keys)`.
    ///
    /// If a later admission step fails, callers can pass `newly_added_keys` to
    /// [`Self::remove_keys`] without disturbing any previously committed media.
    pub(crate) fn insert_batch_with_added(
        &self,
        images: &[PreparedImage],
    ) -> Result<(Vec<MediaRef>, Vec<String>), MediaError> {
        if images.len() > MAX_IMAGES_PER_MESSAGE {
            return Err(MediaError::TooManyImages {
                count: images.len(),
                max_images: MAX_IMAGES_PER_MESSAGE,
            });
        }
        for image in images {
            if parse_managed_media_sha256(&image.reference).is_none() {
                return Err(MediaError::InvalidReference {
                    reference: image.reference.clone(),
                    reason: "expected media:sha256:<64-lowercase-hex>".into(),
                });
            }
            if !is_supported_image_mime(image.media_type) {
                return Err(MediaError::UnsupportedFormat);
            }
            if image.bytes.is_empty() {
                return Err(MediaError::EmptyFile);
            }
            if image.bytes.len() > MAX_IMAGE_BYTES || image.bytes.len() != image.size_bytes {
                return Err(MediaError::FileTooLarge {
                    size_bytes: image.bytes.len() as u64,
                    max_bytes: MAX_IMAGE_BYTES,
                });
            }
        }

        let mut state = self.inner.state.write().expect("media vault lock poisoned");
        let mut additional_bytes: usize = 0;
        let mut seen_new: HashSet<&str> = HashSet::new();
        for image in images {
            if !state.entries.contains_key(&image.reference)
                && seen_new.insert(image.reference.as_str())
            {
                additional_bytes = additional_bytes.checked_add(image.size_bytes).ok_or(
                    MediaError::SessionQuotaExceeded {
                        current_bytes: state.total_bytes,
                        additional_bytes: usize::MAX,
                        max_bytes: MAX_SESSION_MEDIA_BYTES,
                    },
                )?;
            }
        }
        let next_total = state.total_bytes.checked_add(additional_bytes).ok_or(
            MediaError::SessionQuotaExceeded {
                current_bytes: state.total_bytes,
                additional_bytes,
                max_bytes: MAX_SESSION_MEDIA_BYTES,
            },
        )?;
        if next_total > MAX_SESSION_MEDIA_BYTES {
            return Err(MediaError::SessionQuotaExceeded {
                current_bytes: state.total_bytes,
                additional_bytes,
                max_bytes: MAX_SESSION_MEDIA_BYTES,
            });
        }

        let mut added_keys = Vec::with_capacity(seen_new.len());
        let mut refs = Vec::with_capacity(images.len());
        for image in images {
            if !state.entries.contains_key(&image.reference) {
                state.entries.insert(
                    image.reference.clone(),
                    MediaEntry {
                        media_type: image.media_type,
                        bytes: image.bytes.clone(),
                        base64: OnceLock::new(),
                    },
                );
                state.total_bytes += image.size_bytes;
                added_keys.push(image.reference.clone());
            }
            refs.push(image.media_ref());
        }
        Ok((refs, added_keys))
    }

    /// Roll back specific keys added by a failed submission.
    pub(crate) fn remove_keys(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut state = self.inner.state.write().expect("media vault lock poisoned");
        for key in keys {
            if let Some(removed) = state.entries.remove(key) {
                state.total_bytes = state.total_bytes.saturating_sub(removed.bytes.len());
            }
        }
    }

    /// Verify SHA-256, magic bytes, and size limits for a persisted blob and load it into the vault.
    pub fn load_verified_blob(&self, reference: &str, bytes: Vec<u8>) -> Result<(), MediaError> {
        let mime = verify_managed_blob(reference, &bytes)?;
        let size = bytes.len();
        let mut state = self.inner.state.write().expect("media vault lock poisoned");
        if state.entries.contains_key(reference) {
            return Ok(());
        }
        let next_total =
            state
                .total_bytes
                .checked_add(size)
                .ok_or(MediaError::SessionQuotaExceeded {
                    current_bytes: state.total_bytes,
                    additional_bytes: size,
                    max_bytes: MAX_SESSION_MEDIA_BYTES,
                })?;
        if next_total > MAX_SESSION_MEDIA_BYTES {
            return Err(MediaError::SessionQuotaExceeded {
                current_bytes: state.total_bytes,
                additional_bytes: size,
                max_bytes: MAX_SESSION_MEDIA_BYTES,
            });
        }
        state.entries.insert(
            reference.to_owned(),
            MediaEntry {
                media_type: mime,
                bytes: Arc::from(bytes),
                base64: OnceLock::new(),
            },
        );
        state.total_bytes = next_total;
        Ok(())
    }

    /// Snapshot raw bytes for every `media:sha256:*` reference in `refs`.
    ///
    /// Fails closed with [`MediaError::MissingMedia`] if any referenced managed
    /// image is absent from the vault.
    pub fn snapshot_blobs_for_refs(
        &self,
        refs: &BTreeSet<String>,
    ) -> Result<Vec<ManagedMediaBlob>, MediaError> {
        let state = self.inner.state.read().expect("media vault lock poisoned");
        let mut blobs = Vec::new();
        for reference in refs {
            if !is_managed_media_ref(reference) {
                continue;
            }
            let entry = state
                .entries
                .get(reference)
                .ok_or_else(|| MediaError::MissingMedia {
                    reference: reference.clone(),
                })?;
            blobs.push((reference.clone(), entry.bytes.clone()));
        }
        Ok(blobs)
    }
}

impl MediaResolver for MediaVault {
    fn resolve(&self, r: &MediaRef) -> Option<MediaPayload> {
        if is_managed_media_ref(&r.reference) {
            let state = self.inner.state.read().ok()?;
            let entry = state.entries.get(&r.reference)?;
            if !r.media_type.eq_ignore_ascii_case(entry.media_type) {
                return None;
            }
            let data_base64 = entry
                .base64
                .get_or_init(|| base64::engine::general_purpose::STANDARD.encode(&entry.bytes))
                .clone();
            return Some(MediaPayload::inline(entry.media_type, data_base64));
        }

        if let Ok(artifact_id) = parse_spilled_artifact_id(&r.reference) {
            let kind = MediaKind::of(&r.media_type)?;
            let max_bytes = match kind {
                MediaKind::Image | MediaKind::Document => MAX_IMAGE_BYTES,
            };
            let store = self.inner.history_store.read().ok()?.clone()?;
            let bytes = store.read_artifact_bytes(artifact_id, max_bytes).ok()??;
            if bytes.is_empty() {
                return None;
            }
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Some(MediaPayload::inline(r.media_type.clone(), data_base64));
        }

        None
    }
}
