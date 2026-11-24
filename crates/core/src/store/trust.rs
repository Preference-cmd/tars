//! Trusted workspace snapshots persisted in SQLite `workspace_trust`.

use super::{SessionStore, StoreError, schema};
use crate::permission::rules::MAX_RULES_FILE_BYTES;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Trust kind for `.tars/permissions.toml`.
pub(crate) const TRUST_KIND_PERMISSIONS_TOML: &str = "permissions_toml";
/// Trust kind for `.tars/agents/*.md`.
#[allow(dead_code)]
pub(crate) const TRUST_KIND_WORKSPACE_AGENTS: &str = "workspace_agents";

impl SessionStore {
    /// Read a trusted workspace snapshot (`permissions_toml` or `workspace_agents`), if present.
    ///
    /// Does not create `<store_root>/tars.db` when absent. Returns `Ok(None)`
    /// when no trust record exists, when the stored canonical `workspace_root`
    /// does not match the current canonical workspace path, or when the stored
    /// SHA-256 digest does not match the stored content.
    pub(crate) fn read_workspace_trust(
        &self,
        workspace_root: &Path,
        kind: &str,
    ) -> Result<Option<String>, StoreError> {
        validate_kind(kind)?;
        let canonical = std::fs::canonicalize(workspace_root)?;
        let Some(conn) = schema::open_read_only(self.db_path())? else {
            return Ok(None);
        };
        let hash_hex = sha256_hex(canonical.as_os_str().as_encoded_bytes());
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT workspace_root, content_sha256, content
                 FROM workspace_trust
                 WHERE workspace_hash = ?1 AND kind = ?2",
                params![hash_hex, kind],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((stored_root, stored_sha256, content)) = row else {
            return Ok(None);
        };
        if content.len() > MAX_RULES_FILE_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RULES_FILE_BYTES,
            });
        }
        if Path::new(&stored_root) != canonical {
            return Ok(None);
        }
        if stored_sha256 != sha256_hex(content.as_bytes()) {
            return Ok(None);
        }
        Ok(Some(content))
    }

    /// Persist a trusted workspace snapshot (`permissions_toml` or `workspace_agents`).
    pub(crate) fn write_workspace_trust(
        &self,
        workspace_root: &Path,
        kind: &str,
        content: &str,
    ) -> Result<(), StoreError> {
        validate_kind(kind)?;
        if content.len() > MAX_RULES_FILE_BYTES {
            return Err(StoreError::RecordTooLarge {
                limit: MAX_RULES_FILE_BYTES,
            });
        }
        let canonical = std::fs::canonicalize(workspace_root)?;
        let canonical_str = canonical.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "canonical workspace root is not valid UTF-8",
            )
        })?;
        let workspace_hash = sha256_hex(canonical.as_os_str().as_encoded_bytes());
        let content_sha256 = sha256_hex(content.as_bytes());
        let trusted_at_ms = i64::try_from(crate::time::current_time_ms())
            .map_err(|_| StoreError::InvalidDatabase("trusted_at_ms overflows i64".into()))?;

        let conn = schema::open_read_write(self.db_path())?;
        let tx = schema::ImmediateTx::begin(&conn)?;
        conn.execute(
            "INSERT INTO workspace_trust (
                workspace_hash, kind, workspace_root, content_sha256, content, trusted_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(workspace_hash, kind) DO UPDATE SET
                workspace_root = excluded.workspace_root,
                content_sha256 = excluded.content_sha256,
                content = excluded.content,
                trusted_at_ms = excluded.trusted_at_ms",
            params![
                workspace_hash,
                kind,
                canonical_str,
                content_sha256,
                content,
                trusted_at_ms,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Read the trusted `.tars/permissions.toml` snapshot for `workspace_root`, if present.
    pub(crate) fn read_workspace_rules_trust(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<String>, StoreError> {
        self.read_workspace_trust(workspace_root, TRUST_KIND_PERMISSIONS_TOML)
    }

    /// Atomically persist the trusted `.tars/permissions.toml` snapshot for `workspace_root`.
    pub(crate) fn write_workspace_rules_trust(
        &self,
        workspace_root: &Path,
        content: &str,
    ) -> Result<(), StoreError> {
        self.write_workspace_trust(workspace_root, TRUST_KIND_PERMISSIONS_TOML, content)
    }
}

fn validate_kind(kind: &str) -> Result<(), StoreError> {
    match kind {
        TRUST_KIND_PERMISSIONS_TOML | TRUST_KIND_WORKSPACE_AGENTS => Ok(()),
        other => Err(StoreError::InvalidDatabase(format!(
            "unsupported workspace_trust kind: {other}"
        ))),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash_hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hash_hex, "{byte:02x}");
    }
    hash_hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::rules::MAX_RULES_FILE_BYTES;
    use std::fs;

    #[test]
    fn trust_record_roundtrip_persists_in_sqlite_without_touching_session_listing() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("store"));
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();

        assert_eq!(store.read_workspace_rules_trust(&workspace).unwrap(), None);
        // Read-only lookup must not create tars.db prematurely.
        assert!(!store.db_path().exists());

        let content =
            "version = 1\n[[command]]\nprefix = [\"cargo\", \"test\"]\ndecision = \"allow\"\n";
        store
            .write_workspace_rules_trust(&workspace, content)
            .unwrap();
        assert!(store.db_path().exists());

        assert_eq!(
            store.read_workspace_rules_trust(&workspace).unwrap(),
            Some(content.to_owned())
        );

        // Second trust kind is stored independently for the same workspace.
        let agents_manifest = "explore.md=abc123\n";
        store
            .write_workspace_trust(&workspace, TRUST_KIND_WORKSPACE_AGENTS, agents_manifest)
            .unwrap();
        assert_eq!(
            store
                .read_workspace_trust(&workspace, TRUST_KIND_WORKSPACE_AGENTS)
                .unwrap(),
            Some(agents_manifest.to_owned())
        );
        assert_eq!(
            store.read_workspace_rules_trust(&workspace).unwrap(),
            Some(content.to_owned())
        );

        // SessionStore::list() must stay empty when only workspace_trust rows exist.
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
    fn trust_record_rejects_mismatched_workspace_root_tampered_digest_and_oversized_content() {
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
        let hash_a = sha256_hex(canon_a.as_os_str().as_encoded_bytes());
        let conn = schema::open_read_write(store.db_path()).unwrap();
        conn.execute(
            "UPDATE workspace_trust SET workspace_root = ?1 WHERE workspace_hash = ?2",
            params![canon_b.to_str().unwrap(), hash_a],
        )
        .unwrap();
        assert_eq!(store.read_workspace_rules_trust(&ws_a).unwrap(), None);

        // Restore workspace_root but tamper with content without updating content_sha256.
        conn.execute(
            "UPDATE workspace_trust SET workspace_root = ?1, content = 'tampered' WHERE workspace_hash = ?2",
            params![canon_a.to_str().unwrap(), hash_a],
        )
        .unwrap();
        assert_eq!(store.read_workspace_rules_trust(&ws_a).unwrap(), None);
    }
}
