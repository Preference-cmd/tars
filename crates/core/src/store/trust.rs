//! Trusted workspace snapshots (`workspace_trust`) and trusted workspace folders (`trusted_workspaces`).

use super::{SessionStore, StoreError, schema};
use crate::permission::rules::MAX_RULES_FILE_BYTES;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Trust kind for `.tars/permissions.toml`.
pub(crate) const TRUST_KIND_PERMISSIONS_TOML: &str = "permissions_toml";
/// Trust kind for `.tars/agents/*.md`.
pub(crate) const TRUST_KIND_WORKSPACE_AGENTS: &str = "workspace_agents";

/// Persisted record of a user-trusted workspace directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedWorkspaceFolder {
    /// Canonicalized path to the trusted workspace root directory.
    pub workspace_root: PathBuf,
    /// Unix epoch timestamp in milliseconds when the folder was trusted.
    pub trusted_at_ms: i64,
}

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

    /// Read the trusted `.tars/agents/*.md` manifest snapshot for `workspace_root`, if present.
    pub(crate) fn read_workspace_agents_trust(
        &self,
        workspace_root: &Path,
    ) -> Result<Option<String>, StoreError> {
        self.read_workspace_trust(workspace_root, TRUST_KIND_WORKSPACE_AGENTS)
    }

    /// Atomically persist the trusted `.tars/agents/*.md` manifest snapshot for `workspace_root`.
    pub(crate) fn write_workspace_agents_trust(
        &self,
        workspace_root: &Path,
        manifest_text: &str,
    ) -> Result<(), StoreError> {
        self.write_workspace_trust(workspace_root, TRUST_KIND_WORKSPACE_AGENTS, manifest_text)
    }

    /// Return whether `workspace_root` is currently recorded in `trusted_workspaces`.
    ///
    /// Does not create `<store_root>/tars.db` when absent, and returns `Ok(false)`
    /// for non-existent paths, unmigrated schema v1 databases, or rows whose stored
    /// `workspace_root` does not match the canonicalized path.
    pub(crate) fn is_workspace_folder_trusted(
        &self,
        workspace_root: &Path,
    ) -> Result<bool, StoreError> {
        let canonical = match std::fs::canonicalize(workspace_root) {
            Ok(path) => path,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err.into()),
        };
        let Some(conn) = schema::open_read_only(self.db_path())? else {
            return Ok(false);
        };
        if schema::read_user_version(&conn)? < 2 {
            return Ok(false);
        }
        let hash_hex = sha256_hex(canonical.as_os_str().as_encoded_bytes());
        let stored_root: Option<String> = conn
            .query_row(
                "SELECT workspace_root
                 FROM trusted_workspaces
                 WHERE workspace_hash = ?1",
                params![hash_hex],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stored_root.is_some_and(|root| Path::new(&root) == canonical))
    }

    /// Record `workspace_root` as a trusted workspace folder in `trusted_workspaces`.
    pub(crate) fn trust_workspace_folder(&self, workspace_root: &Path) -> Result<(), StoreError> {
        let canonical = std::fs::canonicalize(workspace_root)?;
        if !canonical.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("workspace root is not a directory: {}", canonical.display()),
            )
            .into());
        }
        let canonical_str = canonical.to_str().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "canonical workspace root is not valid UTF-8",
            )
        })?;
        let workspace_hash = sha256_hex(canonical.as_os_str().as_encoded_bytes());
        let trusted_at_ms = i64::try_from(crate::time::current_time_ms())
            .map_err(|_| StoreError::InvalidDatabase("trusted_at_ms overflows i64".into()))?;

        let conn = schema::open_read_write(self.db_path())?;
        let tx = schema::ImmediateTx::begin(&conn)?;
        conn.execute(
            "INSERT INTO trusted_workspaces (
                workspace_hash, workspace_root, trusted_at_ms
             ) VALUES (?1, ?2, ?3)
             ON CONFLICT(workspace_hash) DO UPDATE SET
                workspace_root = excluded.workspace_root,
                trusted_at_ms = excluded.trusted_at_ms",
            params![workspace_hash, canonical_str, trusted_at_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// List all persisted trusted workspace folders ordered by most recently trusted first.
    ///
    /// Does not create `<store_root>/tars.db` when absent and returns an empty list
    /// for unmigrated schema v1 databases.
    pub(crate) fn list_trusted_workspace_folders(
        &self,
    ) -> Result<Vec<TrustedWorkspaceFolder>, StoreError> {
        let Some(conn) = schema::open_read_only(self.db_path())? else {
            return Ok(Vec::new());
        };
        if schema::read_user_version(&conn)? < 2 {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT workspace_root, trusted_at_ms
             FROM trusted_workspaces
             ORDER BY trusted_at_ms DESC, workspace_root ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            let workspace_root: String = row.get(0)?;
            let trusted_at_ms: i64 = row.get(1)?;
            Ok(TrustedWorkspaceFolder {
                workspace_root: PathBuf::from(workspace_root),
                trusted_at_ms,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Revoke folder-level trust for `workspace_root`, returning whether a row was removed.
    ///
    /// Never creates `<store_root>/tars.db` and never upgrades an unmigrated schema v1
    /// database. Supports revoking deleted directories by canonicalizing the deepest
    /// existing ancestor of the absolute path. Never touches `sessions` or `workspace_trust`.
    pub(crate) fn revoke_workspace_folder_trust(
        &self,
        workspace_root: &Path,
    ) -> Result<bool, StoreError> {
        {
            let Some(conn) = schema::open_read_only(self.db_path())? else {
                return Ok(false);
            };
            if schema::read_user_version(&conn)? < 2 {
                return Ok(false);
            }
        }
        if workspace_root.as_os_str().is_empty() {
            return Ok(false);
        }
        let resolved = resolve_workspace_path_for_revoke(workspace_root)?;
        let resolved_hash = sha256_hex(resolved.as_os_str().as_encoded_bytes());
        let resolved_str = resolved.to_str();
        let raw_str = workspace_root.to_str();

        let conn = schema::open_read_write(self.db_path())?;
        let tx = schema::ImmediateTx::begin(&conn)?;
        let affected = conn.execute(
            "DELETE FROM trusted_workspaces
             WHERE workspace_hash = ?1 OR workspace_root = ?2 OR workspace_root = ?3",
            params![resolved_hash, resolved_str, raw_str],
        )?;
        tx.commit()?;
        Ok(affected > 0)
    }
}

fn resolve_workspace_path_for_revoke(workspace_root: &Path) -> Result<PathBuf, StoreError> {
    match std::fs::canonicalize(workspace_root) {
        Ok(canonical) => Ok(canonical),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let abs = std::path::absolute(workspace_root)?;
            for ancestor in abs.ancestors() {
                match std::fs::canonicalize(ancestor) {
                    Ok(mut resolved) => {
                        if let Ok(suffix) = abs.strip_prefix(ancestor) {
                            for component in suffix.components() {
                                match component {
                                    Component::CurDir => {}
                                    Component::ParentDir => {
                                        resolved.pop();
                                    }
                                    Component::Normal(part) => resolved.push(part),
                                    Component::RootDir | Component::Prefix(_) => {}
                                }
                            }
                        }
                        return Ok(resolved);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(err.into()),
                }
            }
            Ok(abs)
        }
        Err(err) => Err(err.into()),
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
