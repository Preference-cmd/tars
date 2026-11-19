//! Artifact store backed by a session's rewrite records and `.artifacts/` directory.
//!
//! Exposes both original blocks from committed history rewrite records
//! (`<record id>/<block id>`) and spilled oversized tool outputs
//! (`artifact:<artifact id>`) persisted atomically in `<store>/sessions/<id>.artifacts/`.

use super::history::{
    ARTIFACT_ID_PREFIX, HistoryStore, parse_artifact_id, parse_spilled_artifact_id,
};
use async_trait::async_trait;
use causa::kernel::{ArtifactHint, ArtifactRef, ArtifactStore, StoreError};
use std::ops::Range;
use std::sync::Arc;

/// An artifact store bound to one session's history records and spilled artifacts.
#[derive(Clone)]
pub(crate) struct HistoryArtifacts {
    history: Arc<HistoryStore>,
}

impl HistoryArtifacts {
    /// Bind an artifact store to a session's history and artifact directories.
    pub(crate) fn new(history: Arc<HistoryStore>) -> Self {
        Self { history }
    }
}

#[async_trait]
impl ArtifactStore for HistoryArtifacts {
    async fn persist(&self, data: &[u8], hint: ArtifactHint) -> Result<ArtifactRef, StoreError> {
        let size_bytes = data.len();
        let bytes = data.to_vec();
        let history = self.history.clone();
        let artifact_id =
            tokio::task::spawn_blocking(move || history.persist_spilled_artifact(&bytes))
                .await
                .map_err(|e| StoreError::Persist(format!("storage task failed: {e}")))?
                .map_err(|e| StoreError::Persist(e.to_string()))?;
        Ok(ArtifactRef::new(
            format!("{ARTIFACT_ID_PREFIX}{artifact_id}"),
            size_bytes,
            hint.kind,
            true,
        ))
    }

    async fn read(&self, id: &str, range: Option<Range<u64>>) -> Result<Vec<u8>, StoreError> {
        if id.starts_with(ARTIFACT_ID_PREFIX) {
            let artifact_id = parse_spilled_artifact_id(id)
                .map_err(|reason| {
                    StoreError::Read(format!("malformed artifact id {id}: {reason}"))
                })?
                .to_owned();
            let history = self.history.clone();
            return tokio::task::spawn_blocking(move || {
                history.read_spilled_artifact(&artifact_id, range)
            })
            .await
            .map_err(|e| StoreError::Read(format!("storage task failed: {e}")))?
            .map_err(|e| StoreError::Read(e.to_string()));
        }

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
