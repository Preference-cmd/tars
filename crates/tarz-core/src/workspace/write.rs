//! Workspace write primitives, atomic replacement, and version tracking.

use super::{Workspace, WorkspaceError};

/// Monitored file metadata used to verify that a file has not changed since it was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileVersion {
    /// Device identifier of the filesystem containing the file.
    pub dev: u64,
    /// Inode number of the file.
    pub ino: u64,
    /// Size of the file in bytes.
    pub size: u64,
    /// Last modification timestamp in seconds.
    pub mtime_secs: i64,
    /// Nanosecond component of the last modification timestamp.
    pub mtime_nanos: u32,
    /// In-memory 64-bit hash of the observed file content.
    pub hash: u64,
}

impl FileVersion {
    /// Construct a file version from an open file descriptor's stat and its content bytes.
    #[cfg(unix)]
    pub(crate) fn from_stat_and_content(stat: &rustix::fs::Stat, content: &[u8]) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, content);
        let hash = std::hash::Hasher::finish(&hasher);
        Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino,
            size: stat.st_size as u64,
            mtime_secs: stat.st_mtime,
            mtime_nanos: stat.st_mtime_nsec as u32,
            hash,
        }
    }
}

/// Error returned by atomic write operations.
#[derive(Debug)]
pub(crate) enum AtomicWriteError<E> {
    /// Target file already exists when attempting to create a new file without replacement.
    AlreadyExists,
    /// Operation was cancelled or timed out.
    CancelledOrTimedOut(E),
    /// A filesystem error occurred.
    Workspace(WorkspaceError),
}

#[cfg(unix)]
struct TempFileCleanup<'a> {
    parent: &'a rustix::fd::OwnedFd,
    name: String,
    active: bool,
}

#[cfg(unix)]
impl<'a> TempFileCleanup<'a> {
    fn new(parent: &'a rustix::fd::OwnedFd, name: String) -> Self {
        Self {
            parent,
            name,
            active: true,
        }
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

#[cfg(unix)]
impl Drop for TempFileCleanup<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = rustix::fs::unlinkat(self.parent, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

#[cfg(unix)]
impl Workspace {
    /// Ensure all intermediate parent directories exist along the relative components,
    /// creating missing directories step-by-step from the root handle.
    pub(crate) fn ensure_parent_directory(
        &self,
        components: &[String],
    ) -> Result<rustix::fd::OwnedFd, WorkspaceError> {
        let mut current = self.child_directory(&self.root, ".")?;
        for name in components {
            match rustix::fs::mkdirat(&current, name, rustix::fs::Mode::from_raw_mode(0o777)) {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {}
                Err(err) => return Err(super::map_errno(err)),
            }
            current = self.child_directory(&current, name)?;
        }
        Ok(current)
    }

    /// Open existing parent directory components without creating missing directories.
    pub(crate) fn parent_directory(
        &self,
        components: &[String],
    ) -> Result<rustix::fd::OwnedFd, WorkspaceError> {
        let mut current = self.child_directory(&self.root, ".")?;
        for name in components {
            current = self.child_directory(&current, name)?;
        }
        Ok(current)
    }

    /// Atomically write content to a file in the given parent directory.
    ///
    /// Writes content to a randomly named temporary file in the same directory,
    /// syncs it to disk, checks cancellation, and replaces or creates the target file.
    /// If an existing file is replaced (`existing_mode` is `Some`), its permission bits are preserved.
    /// If creating a new file (`existing_mode` is `None`), `renameat_with` with `RenameFlags::NOREPLACE`
    /// (or fallback via `linkat`) is used to reject existing targets with `AtomicWriteError::AlreadyExists`.
    pub(crate) fn atomic_write<E, F>(
        &self,
        parent_dir: &rustix::fd::OwnedFd,
        file_name: &str,
        content: &[u8],
        existing_mode: Option<u32>,
        check_cancel: F,
    ) -> Result<FileVersion, AtomicWriteError<E>>
    where
        F: FnOnce() -> Result<(), E>,
    {
        use rustix::fs::{
            AtFlags, Mode, OFlags, RenameFlags, fchmod, fstat, linkat, openat, renameat,
            renameat_with, unlinkat,
        };
        use std::io::Write;

        let temp_name = format!(".tarz-write-{}.tmp", uuid::Uuid::now_v7());
        let initial_mode = if let Some(mode) = existing_mode {
            Mode::from_raw_mode(mode as _)
        } else {
            Mode::from_raw_mode(0o666)
        };

        let temp_fd = openat(
            parent_dir,
            &temp_name,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            initial_mode,
        )
        .map_err(|err| AtomicWriteError::Workspace(super::map_errno(err)))?;

        let mut cleanup = TempFileCleanup::new(parent_dir, temp_name);

        if let Some(mode) = existing_mode {
            fchmod(&temp_fd, Mode::from_raw_mode(mode as _))
                .map_err(|err| AtomicWriteError::Workspace(super::map_errno(err)))?;
        }

        let mut file = std::fs::File::from(temp_fd);
        file.write_all(content)
            .map_err(|err| AtomicWriteError::Workspace(WorkspaceError::Io(err)))?;
        file.sync_all()
            .map_err(|err| AtomicWriteError::Workspace(WorkspaceError::Io(err)))?;

        let stat =
            fstat(&file).map_err(|err| AtomicWriteError::Workspace(super::map_errno(err)))?;
        let version = FileVersion::from_stat_and_content(&stat, content);

        check_cancel().map_err(AtomicWriteError::CancelledOrTimedOut)?;

        if existing_mode.is_some() {
            renameat(parent_dir, &cleanup.name, parent_dir, file_name)
                .map_err(|err| AtomicWriteError::Workspace(super::map_errno(err)))?;
        } else {
            let rename_res = renameat_with(
                parent_dir,
                &cleanup.name,
                parent_dir,
                file_name,
                RenameFlags::NOREPLACE,
            );
            match rename_res {
                Ok(()) => {}
                Err(rustix::io::Errno::EXIST) => {
                    return Err(AtomicWriteError::AlreadyExists);
                }
                Err(
                    rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP,
                ) => {
                    match linkat(
                        parent_dir,
                        &cleanup.name,
                        parent_dir,
                        file_name,
                        AtFlags::empty(),
                    ) {
                        Ok(()) => {
                            let _ = unlinkat(parent_dir, &cleanup.name, AtFlags::empty());
                        }
                        Err(rustix::io::Errno::EXIST) => {
                            return Err(AtomicWriteError::AlreadyExists);
                        }
                        Err(link_err) => {
                            return Err(AtomicWriteError::Workspace(super::map_errno(link_err)));
                        }
                    }
                }
                Err(err) => return Err(AtomicWriteError::Workspace(super::map_errno(err))),
            }
        }

        cleanup.disarm();
        drop(file);
        Ok(version)
    }
}
