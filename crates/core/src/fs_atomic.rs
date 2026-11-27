//! Shared path-based atomic file replacement and parent-directory durability helpers.
//!
//! Callers that write non-workspace application files (materials, command rule
//! files, downloaded tool binaries) share this primitive. Workspace file writes
//! intentionally stay on `workspace/write.rs` so they remain pinned-fd relative
//! (`openat` / `statat` / `renameat`) and symlink-safe against concurrent tree
//! mutations.

use std::io::{self, Write};
use std::path::Path;

/// How an atomic write handles an already-present destination path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplaceMode {
    /// Atomically replace an existing file (or create if absent).
    Overwrite,
    /// Fail with [`io::ErrorKind::AlreadyExists`] without modifying an existing file.
    CreateNew,
}

/// Write `bytes` to a sibling temporary file, flush and `sync_all`, persist to
/// `path` according to `replace`, and perform a best-effort parent-directory sync.
///
/// Returns the exact number of bytes written. When `temp_prefix` is `Some`,
/// the temporary file uses that prefix via [`tempfile::Builder`].
pub(crate) fn atomic_write_bytes(
    path: &Path,
    temp_prefix: Option<&str>,
    bytes: &[u8],
    replace: ReplaceMode,
) -> io::Result<u64> {
    atomic_write_bytes_with_mode(path, temp_prefix, bytes, replace, None)
}

/// Write `bytes` to a sibling temporary file, flush and `sync_all`, optionally
/// apply a Unix permission mode before persisting to `path`, and perform a
/// best-effort parent-directory sync.
///
/// On non-Unix platforms, `mode` is accepted unconditionally and ignored.
pub(crate) fn atomic_write_bytes_with_mode(
    path: &Path,
    temp_prefix: Option<&str>,
    bytes: &[u8],
    replace: ReplaceMode,
    mode: Option<u32>,
) -> io::Result<u64> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = match temp_prefix {
        Some(prefix) => tempfile::Builder::new()
            .prefix(prefix)
            .tempfile_in(parent)?,
        None => tempfile::NamedTempFile::new_in(parent)?,
    };
    pending.write_all(bytes)?;
    pending.as_file().sync_all()?;
    #[cfg(unix)]
    if let Some(raw_mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(raw_mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    match replace {
        ReplaceMode::Overwrite => {
            pending.persist(path).map_err(|error| error.error)?;
        }
        ReplaceMode::CreateNew => {
            pending
                .persist_noclobber(path)
                .map_err(|error| error.error)?;
        }
    }
    sync_parent_dir(parent);
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

/// Best-effort parent-directory synchronization after an atomic rename on Unix.
///
/// Directory fsync is unsupported or restricted on some filesystems and non-Unix
/// platforms, so failures are intentionally ignored after the durable rename.
#[cfg(unix)]
pub(crate) fn sync_parent_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// No-op on non-Unix platforms where directory handles cannot be `fsync`ed.
#[cfg(not(unix))]
pub(crate) fn sync_parent_dir(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn overwrite_replaces_existing_content_without_leftover_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data.json");

        let written_first =
            atomic_write_bytes(&target, None, b"initial payload", ReplaceMode::Overwrite).unwrap();
        assert_eq!(written_first, 15);
        assert_eq!(std::fs::read(&target).unwrap(), b"initial payload");

        let written_second = atomic_write_bytes(
            &target,
            Some(".data.pending-"),
            b"updated",
            ReplaceMode::Overwrite,
        )
        .unwrap();
        assert_eq!(written_second, 7);
        assert_eq!(std::fs::read(&target).unwrap(), b"updated");
        assert_eq!(dir_entries(dir.path()), vec!["data.json"]);
    }

    #[test]
    fn create_new_fails_with_already_exists_without_clobbering_or_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("recovery.json");

        let written = atomic_write_bytes(
            &target,
            Some(".recovery-"),
            b"original bytes",
            ReplaceMode::CreateNew,
        )
        .unwrap();
        assert_eq!(written, 14);
        assert_eq!(dir_entries(dir.path()), vec!["recovery.json"]);

        let err = atomic_write_bytes(
            &target,
            Some(".recovery-"),
            b"replacement bytes",
            ReplaceMode::CreateNew,
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"original bytes");
        assert_eq!(dir_entries(dir.path()), vec!["recovery.json"]);
    }

    #[test]
    fn missing_parent_directory_returns_error_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("missing").join("file.txt");

        let err =
            atomic_write_bytes(&target, None, b"payload", ReplaceMode::Overwrite).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(dir_entries(dir.path()).is_empty());
    }

    #[test]
    fn atomic_write_bytes_with_mode_writes_and_sets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.toml");

        let written = atomic_write_bytes_with_mode(
            &target,
            Some(".secret.pending-"),
            b"secret payload",
            ReplaceMode::Overwrite,
            Some(0o600),
        )
        .unwrap();
        assert_eq!(written, 14);
        assert_eq!(std::fs::read(&target).unwrap(), b"secret payload");
        assert_eq!(dir_entries(dir.path()), vec!["secret.toml"]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);

            atomic_write_bytes_with_mode(
                &target,
                Some(".secret.pending-"),
                b"updated",
                ReplaceMode::Overwrite,
                Some(0o640),
            )
            .unwrap();
            let updated_mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(updated_mode, 0o640);
        }
    }
}
