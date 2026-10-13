use causa::kernel::Context;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use super::StoreError;
use crate::definition::SessionDefinition;

pub(crate) const RECORD_FORMAT: &str = "tarz-session-v2";
/// Check the format before decoding the fixed permission-bearing definition.
pub(super) fn decode(bytes: &[u8], identity: &str) -> Result<SavedSessionRecord, StoreError> {
    #[derive(Deserialize)]
    struct Format {
        format: String,
    }
    let format: Format = serde_json::from_slice(bytes)?;
    if format.format != RECORD_FORMAT {
        return Err(StoreError::UnsupportedFormat(format.format));
    }
    let record: SavedSessionRecord = crate::material::decode_stored(bytes)?;
    record.validate(identity)?;
    Ok(record)
}

/// Full wire record; Context's deserializer rejects duplicate block IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedSessionRecord {
    pub(crate) format: String,
    pub(crate) conversation_id: String,
    pub(crate) revision: u64,
    pub(crate) last_commit_id: String,
    pub(crate) definition: SessionDefinition,
    pub(crate) context: Context,
}
impl PartialEq for SavedSessionRecord {
    fn eq(&self, other: &Self) -> bool {
        self.format == other.format
            && self.conversation_id == other.conversation_id
            && self.revision == other.revision
            && self.last_commit_id == other.last_commit_id
            && self.definition == other.definition
            && self.context.blocks() == other.context.blocks()
    }
}
impl SavedSessionRecord {
    pub(crate) fn new(
        conversation_id: String,
        last_commit_id: String,
        expected_revision: Option<u64>,
        definition: SessionDefinition,
        context: Context,
    ) -> Result<Self, StoreError> {
        let record = Self {
            format: RECORD_FORMAT.into(),
            conversation_id,
            revision: successor(expected_revision)?,
            last_commit_id,
            definition,
            context,
        };
        record.validate(&record.conversation_id)?;
        Ok(record)
    }
    pub(super) fn validate(&self, identity: &str) -> Result<(), StoreError> {
        if self.format != RECORD_FORMAT {
            return Err(StoreError::UnsupportedFormat(self.format.clone()));
        }
        canonical_uuid(&self.conversation_id)?;
        canonical_uuid(&self.last_commit_id)?;
        if self.conversation_id != identity {
            return Err(StoreError::IdentityMismatch {
                expected: identity.into(),
                actual: self.conversation_id.clone(),
            });
        }
        if self.revision == 0 {
            return Err(StoreError::InvalidRevision);
        }
        self.definition
            .validate()
            .map_err(|e| StoreError::Definition(Arc::new(e)))?;
        crate::material::validate_references(&self.context, None)
            .map_err(|e| StoreError::References(Arc::new(e)))
    }
}

/// Fixed before I/O and reused exactly on every retry/confirmation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedCommit {
    pub(crate) expected_revision: Option<u64>,
    pub(crate) record: SavedSessionRecord,
}
impl PreparedCommit {
    pub(crate) fn new(
        expected_revision: Option<u64>,
        record: SavedSessionRecord,
    ) -> Result<Self, StoreError> {
        if record.revision != successor(expected_revision)? {
            return Err(StoreError::InvalidRevision);
        }
        record.validate(&record.conversation_id)?;
        Ok(Self {
            expected_revision,
            record,
        })
    }
    pub(super) fn validate(&self, identity: &str) -> Result<(), StoreError> {
        if self.record.revision != successor(self.expected_revision)? {
            return Err(StoreError::InvalidRevision);
        }
        self.record.validate(identity)
    }
}
pub(super) fn canonical_uuid(id: &str) -> Result<(), StoreError> {
    match Uuid::parse_str(id) {
        Ok(uuid) if uuid.hyphenated().to_string() == id => Ok(()),
        _ => Err(StoreError::InvalidIdentity(id.into())),
    }
}
fn successor(revision: Option<u64>) -> Result<u64, StoreError> {
    revision
        .unwrap_or(0)
        .checked_add(1)
        .filter(|_| revision != Some(0))
        .ok_or(StoreError::InvalidRevision)
}

#[cfg(test)]
mod format_tests {
    #[test]
    fn legacy_format_is_rejected_before_decoding_permission_fields() {
        let bytes = br#"{"format":"tarz-session-v1","definition":{"tools":["execute_command"]}}"#;
        assert!(
            matches!(super::decode(bytes, "unused"), Err(super::StoreError::UnsupportedFormat(format)) if format == "tarz-session-v1")
        );
    }
}
