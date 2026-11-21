use super::{SessionStore, StoreError};
use crate::permission::rules::MAX_RULES_FILE_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const TRUST_DIR_NAME: &str = ".rules-trust";
const MAX_TRUST_RECORD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceRulesTrustRecord {
    workspace_root: PathBuf,
    content: String,
}

impl SessionStore {
    /// Read the trusted `.tars/permissions.toml` snapshot for `workspace_root`, if present.
    ///
    /// Returns `Ok(None)` when no trust record exists or when the stored canonical
    /// `workspace_root` does not match the current canonical workspace path.
    pub(crate) fn read_workspace_rules_trust(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<String>, StoreError> {
        let canonical = std::fs::canonicalize(workspace_root)?;
        let (path, _) = trust_record_path(self.directory(), &canonical);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() > MAX_TRUST_RECORD_BYTES as u64 {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_TRUST_RECORD_BYTES,
            });
        }
        let mut bytes = Vec::new();
        file.take(MAX_TRUST_RECORD_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_TRUST_RECORD_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_TRUST_RECORD_BYTES,
            });
        }
        let record: WorkspaceRulesTrustRecord = serde_json::from_slice(&bytes)?;
        if record.content.len() > MAX_RULES_FILE_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RULES_FILE_BYTES,
            });
        }
        if record.workspace_root != canonical {
            return Ok(None);
        }
        Ok(Some(record.content))
    }

    /// Atomically persist the trusted `.tars/permissions.toml` snapshot for `workspace_root`.
    pub(crate) fn write_workspace_rules_trust(
        &self,
        workspace_root: &Path,
        content: &str,
    ) -> Result<(), StoreError> {
        if content.len() > MAX_RULES_FILE_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RULES_FILE_BYTES,
            });
        }
        let canonical = std::fs::canonicalize(workspace_root)?;
        let trust_dir = self.directory().join(TRUST_DIR_NAME);
        fs::create_dir_all(&trust_dir)?;
        let (target_path, hash_hex) = trust_record_path(self.directory(), &canonical);
        let record = WorkspaceRulesTrustRecord {
            workspace_root: canonical,
            content: content.to_owned(),
        };
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() > MAX_TRUST_RECORD_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_TRUST_RECORD_BYTES,
            });
        }
        let mut pending = tempfile::Builder::new()
            .prefix(&format!(".{hash_hex}.pending-"))
            .tempfile_in(&trust_dir)?;
        pending.write_all(&bytes)?;
        pending.as_file().sync_all()?;
        pending.persist(&target_path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn trust_record_path(store_directory: &Path, canonical_workspace_root: &Path) -> (PathBuf, String) {
    let digest = Sha256::digest(canonical_workspace_root.as_os_str().as_encoded_bytes());
    let mut hash_hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hash_hex, "{byte:02x}");
    }
    let path = store_directory
        .join(TRUST_DIR_NAME)
        .join(format!("{hash_hex}.json"));
    (path, hash_hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::rules::MAX_RULES_FILE_BYTES;

    #[test]
    fn trust_record_roundtrip_is_under_dot_dir_and_skipped_by_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("store"));
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        assert_eq!(store.read_workspace_rules_trust(&workspace).unwrap(), None);

        let content =
            "version = 1\n[[command]]\nprefix = [\"cargo\", \"test\"]\ndecision = \"allow\"\n";
        store
            .write_workspace_rules_trust(&workspace, content)
            .unwrap();

        assert_eq!(
            store.read_workspace_rules_trust(&workspace).unwrap(),
            Some(content.to_owned())
        );

        let canonical = fs::canonicalize(&workspace).unwrap();
        let (expected_path, _) = trust_record_path(store.directory(), &canonical);
        assert!(expected_path.exists());
        assert!(
            expected_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        );

        // SessionStore::list() must ignore .rules-trust/ completely.
        assert!(store.list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn trust_record_uses_canonical_workspace_path_across_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("store"));
        let real_ws = dir.path().join("real-workspace");
        let link_ws = dir.path().join("link-workspace");
        fs::create_dir_all(&real_ws).unwrap();
        std::os::unix::fs::symlink(&real_ws, &link_ws).unwrap();

        let content = "version = 1\n";
        store
            .write_workspace_rules_trust(&link_ws, content)
            .unwrap();
        assert_eq!(
            store.read_workspace_rules_trust(&real_ws).unwrap(),
            Some(content.to_owned())
        );
    }

    #[test]
    fn trust_record_rejects_mismatched_workspace_root_and_oversized_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("store"));
        let ws_a = dir.path().join("ws-a");
        let ws_b = dir.path().join("ws-b");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();

        let too_large = "x".repeat(MAX_RULES_FILE_BYTES + 1);
        assert!(matches!(
            store.write_workspace_rules_trust(&ws_a, &too_large),
            Err(StoreError::RecordTooLarge {
                limit: MAX_RULES_FILE_BYTES
            })
        ));

        store
            .write_workspace_rules_trust(&ws_a, "version = 1\n")
            .unwrap();
        let canon_a = fs::canonicalize(&ws_a).unwrap();
        let canon_b = fs::canonicalize(&ws_b).unwrap();
        let (path_a, _) = trust_record_path(store.directory(), &canon_a);
        let tampered = serde_json::to_vec(&WorkspaceRulesTrustRecord {
            workspace_root: canon_b,
            content: "version = 1\n".into(),
        })
        .unwrap();
        fs::write(&path_a, tampered).unwrap();

        assert_eq!(store.read_workspace_rules_trust(&ws_a).unwrap(), None);
    }
}
