//! Isolated `0600` credential store (`credentials.toml`) with fail-closed Unix hardening.
//!
//! Stores named API keys outside `config.toml`, `SessionDefinition`, and shell
//! environment variables. Error messages and `Debug` representations never echo
//! secret values or raw TOML text.

use super::{toml_string, user_tars_dir};
use crate::definition::valid_credential_name;
use crate::fs_atomic::{ReplaceMode, atomic_write_bytes_with_mode};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Hard size limit for `credentials.toml` (`256 KiB`).
pub const MAX_CREDENTIALS_FILE_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentialsFile {
    version: u32,
    #[serde(default)]
    credentials: BTreeMap<String, RawCredentialEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCredentialEntry {
    api_key: String,
}

/// Read-only status of a `credentials.toml` file for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialsFileStatus {
    /// File does not exist yet.
    Missing,
    /// File exists, passes symlink/permission/schema checks, and contains `entry_count` entries.
    Ok {
        /// Number of stored credential entries (names only, never secret values).
        entry_count: usize,
    },
}

/// Errors produced when loading, validating, or updating `credentials.toml`.
///
/// Neither `Display` nor `Debug` ever contains secret values or raw TOML lines.
#[derive(Debug, thiserror::Error)]
pub enum CredentialsError {
    /// Filesystem I/O failed without exposing file contents.
    #[error("credentials file {path:?}: {kind:?}")]
    Io {
        /// Target file path.
        path: PathBuf,
        /// Original I/O error category.
        kind: io::ErrorKind,
    },
    /// Target path (or symlink traversal detected by `O_NOFOLLOW`) is a symlink.
    #[error("credentials file {0:?} must not be a symlink")]
    SymlinkNotAllowed(PathBuf),
    /// Target path exists but is not a regular file.
    #[error("credentials path {0:?} is not a regular file")]
    NotRegularFile(PathBuf),
    /// Target file identity changed between `symlink_metadata` and `open`.
    #[error("credentials file {0:?} changed while opening")]
    FileChanged(PathBuf),
    /// Unix file permissions expose group or other access (`mode & 0o077 != 0`).
    #[error(
        "credentials file {path:?} has insecure permissions {mode:04o}; run: chmod 600 {display_path}",
        display_path = path.display()
    )]
    InsecurePermissions {
        /// Target file path.
        path: PathBuf,
        /// Observed Unix permission bits (`mode & 0o777`).
        mode: u32,
    },
    /// File exceeds [`MAX_CREDENTIALS_FILE_BYTES`] (`256 KiB`).
    #[error("credentials file exceeds 256 KiB")]
    TooLarge,
    /// Invalid TOML syntax, unknown field, or wrong field type.
    #[error("invalid TOML or credentials field at line {line}, column {column}")]
    Syntax {
        /// One-based line number.
        line: usize,
        /// One-based column number.
        column: usize,
    },
    /// Unsupported file format version.
    #[error("unsupported credentials version {0}; expected 1")]
    Version(u32),
    /// Credential entry name does not match `^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$`.
    #[error(
        "credential name must be 1..=64 ASCII alphanumeric, '_' or '-' characters starting with alphanumeric"
    )]
    InvalidName,
    /// Secret value was blank or contained control characters.
    #[error("credential secret must be non-empty and must not contain control characters")]
    InvalidSecret,
    /// Named credential entry was not found in the store.
    #[error("credential {0:?} was not found in credentials store")]
    NotFound(String),
}

/// In-memory view of validated named credentials loaded from `credentials.toml`.
///
/// Implements [`fmt::Debug`] manually so secret values are never printed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CredentialsStore {
    entries: BTreeMap<String, String>,
}

impl fmt::Debug for CredentialsStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        f.debug_struct("CredentialsStore")
            .field("names", &names)
            .finish()
    }
}

/// Return the default user-level `credentials.toml` path under [`user_tars_dir`].
pub fn default_credentials_path() -> Option<PathBuf> {
    user_tars_dir().map(|dir| dir.join("credentials.toml"))
}

/// Resolve the `credentials.toml` path sibling to `config_path`, or fall back
/// to [`default_credentials_path`] when `config_path` is `None`.
///
/// Relative `config_path` values are resolved with [`std::path::absolute`]
/// before taking `.parent()` so `--config config.toml` yields `<cwd>/credentials.toml`.
pub fn credentials_path_for_config(config_path: Option<&Path>) -> Option<PathBuf> {
    match config_path {
        Some(path) => {
            if path.as_os_str().is_empty() {
                return None;
            }
            let abs = std::path::absolute(path).ok()?;
            abs.parent().map(|parent| parent.join("credentials.toml"))
        }
        None => default_credentials_path(),
    }
}

impl CredentialsStore {
    /// Load and validate `credentials.toml` at `path`, failing if the file is missing.
    pub fn load(path: &Path) -> Result<Self, CredentialsError> {
        match Self::load_optional(path)? {
            Some(store) => Ok(store),
            None => Err(CredentialsError::Io {
                path: path.to_path_buf(),
                kind: io::ErrorKind::NotFound,
            }),
        }
    }

    /// Load and validate `credentials.toml` at `path`, returning `Ok(None)` when absent.
    ///
    /// Inspects `symlink_metadata` before opening so dangling symlinks are
    /// rejected fail-closed rather than treated as missing files.
    pub fn load_optional(path: &Path) -> Result<Option<Self>, CredentialsError> {
        let lstat_meta = match std::fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(CredentialsError::Io {
                    path: path.to_path_buf(),
                    kind: error.kind(),
                });
            }
        };
        if lstat_meta.file_type().is_symlink() {
            return Err(CredentialsError::SymlinkNotAllowed(path.to_path_buf()));
        }
        if !lstat_meta.file_type().is_file() {
            return Err(CredentialsError::NotRegularFile(path.to_path_buf()));
        }

        let file = open_regular_nofollow(path, &lstat_meta)?;
        let mut text = String::new();
        file.take(MAX_CREDENTIALS_FILE_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|error| CredentialsError::Io {
                path: path.to_path_buf(),
                kind: error.kind(),
            })?;
        if text.len() > MAX_CREDENTIALS_FILE_BYTES {
            return Err(CredentialsError::TooLarge);
        }

        Self::parse_toml(&text).map(Some)
    }

    /// Return the trimmed non-empty secret value for `name`, if present.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    }

    /// Return sorted credential entry names without exposing any secret values.
    pub fn list_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Insert or update a credential entry in `path`, creating missing parent
    /// directories with `0o700` on Unix and writing `0o600` atomically.
    pub fn set(path: &Path, name: &str, api_key: &str) -> Result<(), CredentialsError> {
        if !valid_credential_name(name) {
            return Err(CredentialsError::InvalidName);
        }
        let secret = validate_secret(api_key)?;

        let mut store = Self::load_optional(path)?.unwrap_or_default();
        store.entries.insert(name.to_owned(), secret.to_owned());

        let rendered = store.render_toml()?;
        ensure_private_parent_dir(path)?;
        reject_existing_symlink(path)?;

        atomic_write_bytes_with_mode(
            path,
            Some(".credentials.toml.pending-"),
            rendered.as_bytes(),
            ReplaceMode::Overwrite,
            Some(0o600),
        )
        .map_err(|error| CredentialsError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;

        Ok(())
    }

    /// Remove a credential entry from `path`, persisting the updated store with `0o600` permissions.
    pub fn remove(path: &Path, name: &str) -> Result<(), CredentialsError> {
        if !valid_credential_name(name) {
            return Err(CredentialsError::InvalidName);
        }

        let mut store = Self::load_optional(path)?
            .ok_or_else(|| CredentialsError::NotFound(name.to_owned()))?;
        if store.entries.remove(name).is_none() {
            return Err(CredentialsError::NotFound(name.to_owned()));
        }

        let rendered = store.render_toml()?;
        reject_existing_symlink(path)?;

        atomic_write_bytes_with_mode(
            path,
            Some(".credentials.toml.pending-"),
            rendered.as_bytes(),
            ReplaceMode::Overwrite,
            Some(0o600),
        )
        .map_err(|error| CredentialsError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        })?;

        Ok(())
    }

    /// Inspect `path` read-only for diagnostics without exposing any secret values.
    pub fn check_permissions(path: &Path) -> Result<CredentialsFileStatus, CredentialsError> {
        match Self::load_optional(path)? {
            None => Ok(CredentialsFileStatus::Missing),
            Some(store) => Ok(CredentialsFileStatus::Ok {
                entry_count: store.entries.len(),
            }),
        }
    }

    fn parse_toml(text: &str) -> Result<Self, CredentialsError> {
        let raw: RawCredentialsFile = toml::from_str(text).map_err(|error: toml::de::Error| {
            let offset = error.span().map_or(0, |span| span.start).min(text.len());
            let prefix = &text[..text.floor_char_boundary(offset)];
            CredentialsError::Syntax {
                line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
                column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
            }
        })?;
        if raw.version != 1 {
            return Err(CredentialsError::Version(raw.version));
        }

        let mut entries = BTreeMap::new();
        for (name, entry) in raw.credentials {
            if !valid_credential_name(&name) {
                return Err(CredentialsError::InvalidName);
            }
            let secret = validate_secret(&entry.api_key)?;
            entries.insert(name, secret.to_owned());
        }
        Ok(Self { entries })
    }

    fn render_toml(&self) -> Result<String, CredentialsError> {
        let mut out = String::from("version = 1\n");
        for (name, secret) in &self.entries {
            out.push('\n');
            let _ = writeln!(out, "[credentials.{name}]");
            let _ = writeln!(out, "api_key = {}", toml_string(secret));
        }
        if out.len() > MAX_CREDENTIALS_FILE_BYTES {
            return Err(CredentialsError::TooLarge);
        }
        Ok(out)
    }
}

fn validate_secret(api_key: &str) -> Result<&str, CredentialsError> {
    let trimmed = api_key.trim();
    if trimmed.is_empty() || api_key.chars().any(char::is_control) {
        return Err(CredentialsError::InvalidSecret);
    }
    Ok(trimmed)
}

fn reject_existing_symlink(path: &Path) -> Result<(), CredentialsError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(CredentialsError::SymlinkNotAllowed(path.to_path_buf()))
        }
        Ok(meta) if !meta.file_type().is_file() => {
            Err(CredentialsError::NotRegularFile(path.to_path_buf()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CredentialsError::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
        }),
    }
}

fn ensure_private_parent_dir(path: &Path) -> Result<(), CredentialsError> {
    let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    let parent_existed = match std::fs::symlink_metadata(parent) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(CredentialsError::Io {
                path: parent.to_path_buf(),
                kind: error.kind(),
            });
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| CredentialsError::Io {
                path: parent.to_path_buf(),
                kind: error.kind(),
            })?;
        if !parent_existed {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| CredentialsError::Io {
                    path: parent.to_path_buf(),
                    kind: error.kind(),
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = parent_existed;
        std::fs::create_dir_all(parent).map_err(|error| CredentialsError::Io {
            path: parent.to_path_buf(),
            kind: error.kind(),
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_nofollow(
    path: &Path,
    lstat_meta: &std::fs::Metadata,
) -> Result<std::fs::File, CredentialsError> {
    use rustix::fs::{Mode, OFlags, open};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let fd = open(path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).map_err(|errno| {
        if errno == rustix::io::Errno::LOOP {
            CredentialsError::SymlinkNotAllowed(path.to_path_buf())
        } else {
            let io_err = io::Error::from_raw_os_error(errno.raw_os_error());
            CredentialsError::Io {
                path: path.to_path_buf(),
                kind: io_err.kind(),
            }
        }
    })?;
    let file = std::fs::File::from(fd);
    let fstat_meta = file.metadata().map_err(|error| CredentialsError::Io {
        path: path.to_path_buf(),
        kind: error.kind(),
    })?;
    if !fstat_meta.file_type().is_file() {
        return Err(CredentialsError::NotRegularFile(path.to_path_buf()));
    }
    if fstat_meta.dev() != lstat_meta.dev() || fstat_meta.ino() != lstat_meta.ino() {
        return Err(CredentialsError::FileChanged(path.to_path_buf()));
    }
    let mode = fstat_meta.permissions().mode() & 0o777;
    if (mode & 0o077) != 0 {
        return Err(CredentialsError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    if fstat_meta.len() > MAX_CREDENTIALS_FILE_BYTES as u64 {
        return Err(CredentialsError::TooLarge);
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular_nofollow(
    path: &Path,
    _lstat_meta: &std::fs::Metadata,
) -> Result<std::fs::File, CredentialsError> {
    let file = std::fs::File::open(path).map_err(|error| CredentialsError::Io {
        path: path.to_path_buf(),
        kind: error.kind(),
    })?;
    let meta = file.metadata().map_err(|error| CredentialsError::Io {
        path: path.to_path_buf(),
        kind: error.kind(),
    })?;
    if !meta.file_type().is_file() {
        return Err(CredentialsError::NotRegularFile(path.to_path_buf()));
    }
    if meta.len() > MAX_CREDENTIALS_FILE_BYTES as u64 {
        return Err(CredentialsError::TooLarge);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_path_for_config_resolves_relative_and_absolute_paths() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            credentials_path_for_config(Some(Path::new("config.toml"))),
            Some(cwd.join("credentials.toml"))
        );
        assert_eq!(
            credentials_path_for_config(Some(Path::new("nested/config.toml"))),
            Some(cwd.join("nested").join("credentials.toml"))
        );
        let abs_cfg = cwd.join("custom").join("config.toml");
        assert_eq!(
            credentials_path_for_config(Some(&abs_cfg)),
            Some(cwd.join("custom").join("credentials.toml"))
        );
        assert_eq!(credentials_path_for_config(Some(Path::new(""))), None);
        assert_eq!(
            credentials_path_for_config(None),
            default_credentials_path()
        );
    }

    #[test]
    fn set_get_list_remove_roundtrip_with_private_permissions_and_redacted_debug() {
        let dir = tempfile::tempdir().unwrap();
        let nested_dir = dir.path().join("private_tars");
        let cred_path = nested_dir.join("credentials.toml");

        assert!(matches!(
            CredentialsStore::check_permissions(&cred_path).unwrap(),
            CredentialsFileStatus::Missing
        ));
        assert!(
            CredentialsStore::load_optional(&cred_path)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            CredentialsStore::load(&cred_path).unwrap_err(),
            CredentialsError::Io {
                kind: io::ErrorKind::NotFound,
                ..
            }
        ));

        CredentialsStore::set(&cred_path, "zeta", "  sk-zeta-secret  ").unwrap();
        CredentialsStore::set(&cred_path, "alpha", "sk-alpha-secret").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = std::fs::metadata(&nested_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            let file_mode = std::fs::metadata(&cred_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
        }

        let store = CredentialsStore::load(&cred_path).unwrap();
        assert_eq!(store.list_names(), vec!["alpha", "zeta"]);
        assert_eq!(store.get("alpha"), Some("sk-alpha-secret"));
        assert_eq!(store.get("zeta"), Some("sk-zeta-secret"));
        assert_eq!(store.get("missing"), None);

        let debug_text = format!("{store:?}");
        assert!(debug_text.contains("alpha"));
        assert!(debug_text.contains("zeta"));
        assert!(!debug_text.contains("sk-alpha-secret"));
        assert!(!debug_text.contains("sk-zeta-secret"));

        assert_eq!(
            CredentialsStore::check_permissions(&cred_path).unwrap(),
            CredentialsFileStatus::Ok { entry_count: 2 }
        );

        let raw = std::fs::read_to_string(&cred_path).unwrap();
        assert!(raw.starts_with("version = 1\n"));
        assert!(raw.contains("[credentials.alpha]\napi_key = \"sk-alpha-secret\""));
        assert!(raw.contains("[credentials.zeta]\napi_key = \"sk-zeta-secret\""));

        CredentialsStore::remove(&cred_path, "alpha").unwrap();
        let after_remove = CredentialsStore::load(&cred_path).unwrap();
        assert_eq!(after_remove.list_names(), vec!["zeta"]);
        assert_eq!(after_remove.get("alpha"), None);

        let err = CredentialsStore::remove(&cred_path, "alpha").unwrap_err();
        assert!(matches!(err, CredentialsError::NotFound(ref n) if n == "alpha"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_live_and_dangling_symlinks_fail_closed() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        CredentialsStore::set(&real, "work", "sk-real").unwrap();
        let live_link = dir.path().join("live.toml");
        symlink(&real, &live_link).unwrap();

        let dangling_link = dir.path().join("dangling.toml");
        symlink(dir.path().join("nonexistent.toml"), &dangling_link).unwrap();

        for link in [&live_link, &dangling_link] {
            assert!(matches!(
                CredentialsStore::load(link).unwrap_err(),
                CredentialsError::SymlinkNotAllowed(_)
            ));
            assert!(matches!(
                CredentialsStore::load_optional(link).unwrap_err(),
                CredentialsError::SymlinkNotAllowed(_)
            ));
            assert!(matches!(
                CredentialsStore::check_permissions(link).unwrap_err(),
                CredentialsError::SymlinkNotAllowed(_)
            ));
            assert!(matches!(
                CredentialsStore::set(link, "work", "sk-new").unwrap_err(),
                CredentialsError::SymlinkNotAllowed(_)
            ));
            assert!(matches!(
                CredentialsStore::remove(link, "work").unwrap_err(),
                CredentialsError::SymlinkNotAllowed(_)
            ));
        }

        // Ensure neither symlink target was overwritten or created.
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            CredentialsStore::load(&real).unwrap().get("work"),
            Some("sk-real")
        );
        assert!(!dir.path().join("nonexistent.toml").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_insecure_unix_permissions_with_chmod_hint() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials.toml");
        CredentialsStore::set(&cred_path, "work", "sk-secret-value").unwrap();

        for bad_mode in [0o644, 0o640, 0o604, 0o700] {
            if bad_mode == 0o700 {
                // 0o700 has no group/other bits; verify 0o644/0o640/0o604 fail closed.
                continue;
            }
            std::fs::set_permissions(&cred_path, std::fs::Permissions::from_mode(bad_mode))
                .unwrap();

            let err = CredentialsStore::load(&cred_path).unwrap_err();
            assert!(
                matches!(
                    err,
                    CredentialsError::InsecurePermissions { mode, .. } if mode == bad_mode
                ),
                "expected InsecurePermissions for {bad_mode:04o}, got {err:?}"
            );
            let display = err.to_string();
            assert!(display.contains("chmod 600"), "{display}");
            assert!(!display.contains("sk-secret-value"));
            assert!(!format!("{err:?}").contains("sk-secret-value"));

            // set() and remove() must also refuse to touch an insecure file.
            assert!(matches!(
                CredentialsStore::set(&cred_path, "other", "sk-other").unwrap_err(),
                CredentialsError::InsecurePermissions { .. }
            ));
            assert!(matches!(
                CredentialsStore::remove(&cred_path, "work").unwrap_err(),
                CredentialsError::InsecurePermissions { .. }
            ));
        }
    }

    #[test]
    fn rejects_unknown_fields_version_oversize_and_invalid_inputs_without_secret_echo() {
        let dir = tempfile::tempdir().unwrap();
        let cred_path = dir.path().join("credentials.toml");

        // Helper to write a 0600 fixture cleanly on Unix.
        let write_fixture = |content: &str| {
            atomic_write_bytes_with_mode(
                &cred_path,
                None,
                content.as_bytes(),
                ReplaceMode::Overwrite,
                Some(0o600),
            )
            .unwrap();
        };

        // 1. Unknown top-level field
        write_fixture("version = 1\nextra = \"sk-secret-leak\"\n");
        let err = CredentialsStore::load(&cred_path).unwrap_err();
        assert!(matches!(err, CredentialsError::Syntax { .. }));
        assert!(!err.to_string().contains("sk-secret-leak"));
        assert!(!format!("{err:?}").contains("sk-secret-leak"));

        // 2. Unknown field inside [credentials.work]
        write_fixture("version = 1\n[credentials.work]\napi_key = \"sk-secret-leak\"\nextra = 1\n");
        let err = CredentialsStore::load(&cred_path).unwrap_err();
        assert!(matches!(err, CredentialsError::Syntax { .. }));
        assert!(!err.to_string().contains("sk-secret-leak"));
        assert!(!format!("{err:?}").contains("sk-secret-leak"));

        // 3. Unsupported version
        write_fixture("version = 2\n[credentials.work]\napi_key = \"sk-secret-leak\"\n");
        let err = CredentialsStore::load(&cred_path).unwrap_err();
        assert!(matches!(err, CredentialsError::Version(2)));
        assert!(!err.to_string().contains("sk-secret-leak"));

        // 4. Blank or control-char secret in file and in set()
        write_fixture("version = 1\n[credentials.work]\napi_key = \"   \"\n");
        assert!(matches!(
            CredentialsStore::load(&cred_path).unwrap_err(),
            CredentialsError::InvalidSecret
        ));
        for bad_secret in ["", "   ", "sk-newline\n", "sk-cr\r", "sk-nul\0", "sk\ttab"] {
            let err = CredentialsStore::set(&cred_path, "work", bad_secret).unwrap_err();
            assert!(matches!(err, CredentialsError::InvalidSecret));
            assert!(!err.to_string().contains("sk-"));
            assert!(!format!("{err:?}").contains("sk-"));
        }

        // 5. Invalid credential name never echoes offending text
        for bad_name in ["", "work.prod", "_bad", "sk-pasted=secret"] {
            let err = CredentialsStore::set(&cred_path, bad_name, "sk-valid").unwrap_err();
            assert!(matches!(err, CredentialsError::InvalidName));
            assert!(!err.to_string().contains("sk-valid"));
            assert!(!err.to_string().contains("pasted"));
            assert!(!format!("{err:?}").contains("sk-valid"));
        }

        // 6. Oversized file (> 256 KiB)
        let mut big = String::from("version = 1\n[credentials.big]\napi_key = \"");
        big.push_str(&"x".repeat(MAX_CREDENTIALS_FILE_BYTES + 64));
        big.push_str("\"\n");
        write_fixture(&big);
        assert!(matches!(
            CredentialsStore::load(&cred_path).unwrap_err(),
            CredentialsError::TooLarge
        ));
    }
}
