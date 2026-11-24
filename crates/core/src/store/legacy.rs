//! Read-only `tars-session-v2.4` decoder and transactional SQLite importer.

use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

use causa::kernel::{BlockContent, BlockId, ContextBlock};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;

use super::{
    MAX_RECORD_BYTES, SavedSessionRecord, SessionStore, StoreError,
    record::{RECORD_FORMAT, canonical_uuid},
    schema::ImmediateTx,
    sqlite::{serialize_record_columns, upsert_session_and_entry_agent},
};
use crate::compact::history::{
    ARTIFACT_ID_PREFIX, HISTORY_FORMAT, HistoryRecord, MAX_ARTIFACT_BYTES, MAX_HISTORY_BYTES,
    parse_artifact_id, parse_spilled_artifact_id,
};

/// Summary returned after importing a legacy `tars-session-v2.4` record into SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyImportSummary {
    /// Imported canonical conversation identity.
    pub conversation_id: String,
    /// Preserved saved revision.
    pub revision: u64,
    /// Number of Context blocks in the imported record.
    pub block_count: usize,
    /// Number of history rewrite records imported into `history_records`.
    pub history_records: usize,
    /// Number of spilled artifacts imported into `artifacts`.
    pub artifacts: usize,
}

/// Read and strictly validate a `tars-session-v2.4` JSON file without modifying it.
pub(crate) fn read_legacy_v2_4(record_path: &Path) -> Result<SavedSessionRecord, StoreError> {
    let file = File::open(record_path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "legacy record path is not a regular file",
        )
        .into());
    }
    if metadata.len() > MAX_RECORD_BYTES as u64 {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::RecordTooLarge {
            limit: MAX_RECORD_BYTES,
        });
    }

    #[derive(Deserialize)]
    struct FormatHeader {
        format: String,
    }
    let header: FormatHeader = serde_json::from_slice(&bytes)?;
    if header.format != RECORD_FORMAT {
        return Err(StoreError::UnsupportedFormat(header.format));
    }

    #[derive(Deserialize)]
    struct IdentityHeader {
        conversation_id: String,
    }
    let identity: IdentityHeader = serde_json::from_slice(&bytes)?;
    canonical_uuid(&identity.conversation_id)?;

    super::record::decode(&bytes, &identity.conversation_id)
}

impl SessionStore {
    /// Read, validate, and import a legacy `tars-session-v2.4` record (plus its
    /// referenced `.history/*.json` and `.artifacts/*` files) into `<store_root>/tars.db`
    /// in a single transaction, leaving all source files untouched.
    pub(crate) fn import_legacy_v2_4(
        &self,
        record_path: &Path,
    ) -> Result<LegacyImportSummary, StoreError> {
        let record = read_legacy_v2_4(record_path)?;
        let legacy_dir = record_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let history_dir_name = format!("{}.history", record.conversation_id);
        let artifacts_dir_name = format!("{}.artifacts", record.conversation_id);
        let history_dir = legacy_dir.join(&history_dir_name);
        let artifacts_dir = legacy_dir.join(&artifacts_dir_name);

        let canon_legacy_dir = fs::canonicalize(legacy_dir)?;
        let history_dir_handle = open_checked_legacy_dir(
            &canon_legacy_dir,
            &history_dir,
            &history_dir_name,
            "legacy history directory",
        )?;
        let artifacts_dir_handle = open_checked_legacy_dir(
            &canon_legacy_dir,
            &artifacts_dir,
            &artifacts_dir_name,
            "legacy artifacts directory",
        )?;

        let mut history_docs: Vec<(i64, HistoryRecord, String, String)> =
            Vec::with_capacity(record.history.len());
        let mut history_originals: HashMap<String, Vec<BlockId>> =
            HashMap::with_capacity(record.history.len());
        let mut required_artifacts = BTreeSet::new();

        for (seq, entry) in record.history.iter().enumerate() {
            let file_name = format!("{}.json", entry.record_id);
            let doc_path = history_dir.join(&file_name);
            let doc = read_legacy_history_record(
                history_dir_handle.as_ref(),
                &file_name,
                &doc_path,
                &record.conversation_id,
                &entry.record_id,
            )?;
            if doc.index_entry() != *entry {
                return Err(StoreError::InvalidHistory(format!(
                    "history record {} does not match session index entry",
                    entry.record_id
                )));
            }
            let mut original_ids = Vec::new();
            for replacement in &doc.replacements {
                collect_spilled_ids(&replacement.originals, &mut required_artifacts)?;
                for original in &replacement.originals {
                    original_ids.push(original.id());
                }
            }
            history_originals.insert(entry.record_id.clone(), original_ids);

            let summary_json = serde_json::to_string(entry)?;
            let document_json = serde_json::to_string(&doc)?;
            if document_json.len() > MAX_HISTORY_BYTES {
                return Err(StoreError::HistoryTooLarge {
                    limit: MAX_HISTORY_BYTES,
                });
            }
            let seq_i64 = i64::try_from(seq)
                .map_err(|_| StoreError::InvalidDatabase("history seq overflows i64".into()))?;
            history_docs.push((seq_i64, doc, summary_json, document_json));
        }

        // Verify every external history and spilled artifact reference in the saved Context.
        for block in record.context.blocks() {
            let BlockContent::ToolResult(result) = block.content() else {
                continue;
            };
            let Some(artifact) = &result.output.artifact else {
                continue;
            };
            if artifact.id.starts_with(ARTIFACT_ID_PREFIX) {
                let artifact_id = parse_spilled_artifact_id(&artifact.id)
                    .map_err(|reason| StoreError::InvalidHistory(reason.to_owned()))?;
                required_artifacts.insert(artifact_id.to_owned());
            } else {
                let (record_id, block_id) = parse_artifact_id(&artifact.id)
                    .map_err(|reason| StoreError::InvalidHistory(reason.to_owned()))?;
                let originals = history_originals.get(record_id).ok_or_else(|| {
                    StoreError::InvalidHistory(format!(
                        "history record {record_id} is not in session index"
                    ))
                })?;
                if !originals.contains(&block_id) {
                    return Err(StoreError::InvalidHistory(format!(
                        "block {} is not among the originals of history record {record_id}",
                        block_id.0
                    )));
                }
            }
        }

        let mut artifact_blobs: Vec<(String, Vec<u8>)> =
            Vec::with_capacity(required_artifacts.len());
        for artifact_id in required_artifacts {
            let artifact_path = artifacts_dir.join(&artifact_id);
            let bytes =
                read_legacy_artifact(artifacts_dir_handle.as_ref(), &artifact_path, &artifact_id)?;
            artifact_blobs.push((artifact_id, bytes));
        }

        let columns = serialize_record_columns(&record)?;

        // Acquire the per-session OS advisory lock and reuse the writer's SQLite connection.
        let writer = self.acquire_writer(&record.conversation_id)?;
        writer.with_connection(|conn| {
            let tx = ImmediateTx::begin(conn)?;

            let exists: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE session_id = ?1",
                    params![record.conversation_id],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_some() {
                return Err(StoreError::AlreadyExists(record.conversation_id.clone()));
            }

            upsert_session_and_entry_agent(conn, &record, &columns)?;

            for (seq, doc, summary_json, document_json) in &history_docs {
                conn.execute(
                    "INSERT INTO history_records (
                        session_id, agent_id, record_id, seq, kind, summary_json, document_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        record.conversation_id,
                        record.conversation_id,
                        doc.record_id,
                        seq,
                        doc.kind.name(),
                        summary_json,
                        document_json,
                    ],
                )?;
            }

            for (artifact_id, content) in &artifact_blobs {
                conn.execute(
                    "INSERT INTO artifacts (session_id, agent_id, artifact_id, content)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        record.conversation_id,
                        record.conversation_id,
                        artifact_id,
                        content.as_slice(),
                    ],
                )?;
            }

            tx.commit()
        })?;

        Ok(LegacyImportSummary {
            conversation_id: record.conversation_id,
            revision: record.revision,
            block_count: record.context.blocks().len(),
            history_records: history_docs.len(),
            artifacts: artifact_blobs.len(),
        })
    }
}

#[cfg(unix)]
type LegacyDirHandle = std::os::fd::OwnedFd;
#[cfg(not(unix))]
type LegacyDirHandle = std::path::PathBuf;

fn open_checked_legacy_dir(
    canon_legacy_dir: &Path,
    dir: &Path,
    expected_name: &str,
    label: &str,
) -> Result<Option<LegacyDirHandle>, StoreError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(StoreError::InvalidHistory(format!(
            "{label} {} must not be a symlink",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(StoreError::InvalidHistory(format!(
            "{label} {} is not a directory",
            dir.display()
        )));
    }
    let canon_dir = fs::canonicalize(dir)?;
    if canon_dir != canon_legacy_dir.join(expected_name) {
        return Err(StoreError::InvalidHistory(format!(
            "{label} {} escapes the legacy directory",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, open};
        let fd = open(
            dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|errno| {
            if errno == rustix::io::Errno::LOOP {
                StoreError::InvalidHistory(format!(
                    "{label} {} must not be a symlink",
                    dir.display()
                ))
            } else {
                StoreError::from(io::Error::from_raw_os_error(errno.raw_os_error()))
            }
        })?;
        Ok(Some(fd))
    }
    #[cfg(not(unix))]
    {
        Ok(Some(canon_dir))
    }
}

fn open_legacy_file_no_follow(
    dir_handle: Option<&LegacyDirHandle>,
    file_name: &str,
    path: &Path,
    kind: &str,
    missing_message: &str,
) -> Result<File, StoreError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(StoreError::InvalidHistory(missing_message.to_owned()));
        }
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(StoreError::InvalidHistory(format!(
            "{kind} {} must not be a symlink",
            path.display()
        )));
    }
    let dir = dir_handle.ok_or_else(|| StoreError::InvalidHistory(missing_message.to_owned()))?;
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            dir,
            file_name,
            OFlags::RDONLY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|errno| {
            if errno == rustix::io::Errno::NOENT {
                StoreError::InvalidHistory(missing_message.to_owned())
            } else if errno == rustix::io::Errno::LOOP {
                StoreError::InvalidHistory(format!(
                    "{kind} {} must not be a symlink",
                    path.display()
                ))
            } else {
                StoreError::from(io::Error::from_raw_os_error(errno.raw_os_error()))
            }
        })?;
        Ok(File::from(fd))
    }
    #[cfg(not(unix))]
    {
        File::open(dir.join(file_name)).map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                StoreError::InvalidHistory(missing_message.to_owned())
            } else {
                StoreError::from(err)
            }
        })
    }
}

fn read_legacy_history_record(
    dir_handle: Option<&LegacyDirHandle>,
    file_name: &str,
    path: &Path,
    conversation_id: &str,
    record_id: &str,
) -> Result<HistoryRecord, StoreError> {
    let missing_message = format!(
        "missing referenced legacy history record {}",
        path.display()
    );
    let file = open_legacy_file_no_follow(
        dir_handle,
        file_name,
        path,
        "legacy history record",
        &missing_message,
    )?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(StoreError::InvalidHistory(format!(
            "legacy history record {} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_HISTORY_BYTES as u64 {
        return Err(StoreError::HistoryTooLarge {
            limit: MAX_HISTORY_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(MAX_HISTORY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_HISTORY_BYTES {
        return Err(StoreError::HistoryTooLarge {
            limit: MAX_HISTORY_BYTES,
        });
    }
    #[derive(Deserialize)]
    struct FormatCheck {
        format: String,
    }
    let check: FormatCheck = serde_json::from_slice(&bytes)?;
    if check.format != HISTORY_FORMAT {
        return Err(StoreError::InvalidHistory(format!(
            "unsupported history format: {}; expected {}",
            check.format, HISTORY_FORMAT
        )));
    }
    let document: HistoryRecord = serde_json::from_slice(&bytes)?;
    document.validate(conversation_id, record_id)?;
    Ok(document)
}

fn read_legacy_artifact(
    dir_handle: Option<&LegacyDirHandle>,
    path: &Path,
    artifact_id: &str,
) -> Result<Vec<u8>, StoreError> {
    let missing_message = format!("missing referenced legacy spilled artifact {artifact_id}");
    let file = open_legacy_file_no_follow(
        dir_handle,
        artifact_id,
        path,
        "legacy artifact",
        &missing_message,
    )?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(StoreError::InvalidHistory(format!(
            "legacy artifact {artifact_id} is not a regular file"
        )));
    }
    if meta.len() > MAX_ARTIFACT_BYTES as u64 {
        return Err(StoreError::HistoryTooLarge {
            limit: MAX_ARTIFACT_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    file.take(MAX_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(StoreError::HistoryTooLarge {
            limit: MAX_ARTIFACT_BYTES,
        });
    }
    Ok(bytes)
}

fn collect_spilled_ids(
    blocks: &[ContextBlock],
    into: &mut BTreeSet<String>,
) -> Result<(), StoreError> {
    for block in blocks {
        if let BlockContent::ToolResult(result) = block.content()
            && let Some(artifact) = &result.output.artifact
            && artifact.id.starts_with(ARTIFACT_ID_PREFIX)
        {
            let artifact_id = parse_spilled_artifact_id(&artifact.id)
                .map_err(|reason| StoreError::InvalidHistory(reason.to_owned()))?;
            into.insert(artifact_id.to_owned());
        }
    }
    Ok(())
}
