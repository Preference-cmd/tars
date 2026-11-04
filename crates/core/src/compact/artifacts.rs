//! Artifact store backed by a session's rewrite records.
//!
//! Exposes original blocks from committed history rewrite records as readable
//! artifacts. History artifacts are read-only and cannot be created via persist.

use super::history::{HistoryStore, parse_artifact_id};
use async_trait::async_trait;
use causa::kernel::{ArtifactHint, ArtifactRef, ArtifactStore, StoreError};
use std::ops::Range;
use std::sync::Arc;

/// An artifact store that exposes original blocks from rewrite records.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct HistoryArtifacts {
    history: Arc<HistoryStore>,
}

#[allow(dead_code)]
impl HistoryArtifacts {
    /// Bind an artifact store to a session's rewrite record store.
    pub(crate) fn new(history: Arc<HistoryStore>) -> Self {
        Self { history }
    }
}

#[async_trait]
impl ArtifactStore for HistoryArtifacts {
    async fn persist(&self, _data: &[u8], _hint: ArtifactHint) -> Result<ArtifactRef, StoreError> {
        Err(StoreError::Persist(
            "history artifacts are written only by context rewrites".into(),
        ))
    }

    async fn read(&self, id: &str, range: Option<Range<u64>>) -> Result<Vec<u8>, StoreError> {
        let (record_id, block_id) = parse_artifact_id(id).map_err(|reason| {
            StoreError::Read(format!("malformed history artifact id {id}: {reason}"))
        })?;
        if !self.history.has_record(record_id) {
            return Err(StoreError::Read(format!(
                "history record {record_id} not in session index"
            )));
        }
        let history = self.history.clone();
        let record_id = record_id.to_owned();
        let bytes = tokio::task::spawn_blocking(move || {
            history.read_original_block_text(&record_id, block_id)
        })
        .await
        .map_err(|e| StoreError::Read(format!("storage task failed: {e}")))?
        .map_err(|e| StoreError::Read(e.to_string()))?
        .into_bytes();

        let len = bytes.len() as u64;
        let slice = match range {
            None => bytes,
            Some(range) => {
                if range.start > len || range.start > range.end {
                    return Err(StoreError::Read(format!(
                        "invalid byte range {}..{} for artifact length {len}",
                        range.start, range.end
                    )));
                }
                let clamped_end = range.end.min(len);
                let start = usize::try_from(range.start).unwrap_or(usize::MAX);
                let end = usize::try_from(clamped_end).unwrap_or(usize::MAX);
                bytes[start..end].to_vec()
            }
        };
        Ok(slice)
    }
}
