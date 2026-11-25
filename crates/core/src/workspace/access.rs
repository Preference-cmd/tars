// Workspace tools run only on Unix; elsewhere this code is compiled but unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_variables))]
use std::path::{Component, Path};
use std::sync::Arc;

/// A selected root resource, pinned for the lifetime of an open session.
pub struct Workspace {
    #[cfg(unix)]
    pub(crate) root: rustix::fd::OwnedFd,
    #[cfg(unix)]
    device: rustix::fs::Dev,
}

/// Classification of a directory entry without following symbolic links.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    /// Regular file on the pinned workspace device.
    RegularFile,
    /// Directory (device verified when opened via [`Workspace::child_directory`]).
    Directory,
    /// Symbolic link, FIFO, socket, device node, or other non-regular entry.
    Other,
}

/// An access constraint or filesystem failure.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    /// Path syntax is outside the selected relative namespace.
    #[error("path must be a bounded relative path without parent components")]
    InvalidPath,
    /// Links, mount device changes and special files are forbidden.
    #[error("symbolic links, device boundaries and special files are not accessible")]
    Forbidden,
    /// The platform has no implemented safe workspace access mechanism.
    #[error("workspace tools require Unix handle-relative filesystem access")]
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    UnsupportedPlatform,
    /// An ordinary filesystem failure occurred.
    #[error("workspace I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

impl Workspace {
    /// Open an absolute physical root directory without following links in any
    /// component. Configuration may resolve a user-selected alias before freezing
    /// this path; reopening must use the frozen physical path.
    pub fn open(path: &Path) -> Result<Arc<Self>, WorkspaceError> {
        if !path.is_absolute() {
            return Err(WorkspaceError::InvalidPath);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{Mode, OFlags, fstat, open, openat};
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let mut root = open("/", flags, Mode::empty()).map_err(map_errno)?;
            for part in path.components() {
                match part {
                    Component::RootDir | Component::CurDir => {}
                    Component::Normal(name) => {
                        root = openat(&root, name, flags, Mode::empty()).map_err(map_errno)?
                    }
                    _ => return Err(WorkspaceError::InvalidPath),
                }
            }
            let device = fstat(&root).map_err(map_errno)?.st_dev;
            Ok(Arc::new(Self { root, device }))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(WorkspaceError::UnsupportedPlatform)
    }

    /// A detached handle for command-tool tests on a platform without
    /// workspace access.
    ///
    /// The command tool resolves its own workspace root and never uses the file
    /// tools, so a command-only test can bind the shared tool set without a
    /// real directory handle. Compiled only for tests on platforms where
    /// [`Workspace::open`] has no implementation, and never reachable from
    /// production code.
    #[cfg(all(test, not(any(target_os = "linux", target_os = "macos"))))]
    pub(crate) fn detached_for_command_tests() -> Arc<Self> {
        Arc::new(Self {})
    }

    pub(crate) fn components(path: &str) -> Result<Vec<String>, WorkspaceError> {
        if path.is_empty() || path.len() > 4096 || path.contains('\0') || path.contains('\\') {
            return Err(WorkspaceError::InvalidPath);
        }
        let mut parts = Vec::new();
        for part in Path::new(path).components() {
            match part {
                Component::CurDir => {}
                Component::Normal(name) => {
                    parts.push(name.to_str().ok_or(WorkspaceError::InvalidPath)?.to_owned())
                }
                _ => return Err(WorkspaceError::InvalidPath),
            }
        }
        Ok(parts)
    }

    /// Version-control metadata directory names that built-in tools never
    /// write or search.
    pub(crate) const VCS_METADATA: [&'static str; 4] = [".git", ".hg", ".svn", ".jj"];

    /// Whether a single path component names version-control metadata.
    ///
    /// The comparison ignores ASCII case because the default macOS filesystem
    /// does, so `.GIT` is the same directory as `.git`.
    pub(crate) fn is_vcs_metadata_name(name: &str) -> bool {
        Self::VCS_METADATA
            .iter()
            .any(|metadata| name.eq_ignore_ascii_case(metadata))
    }

    /// The first component naming version-control metadata, if any.
    ///
    /// The comparison ignores ASCII case because the default macOS filesystem
    /// does, so `.GIT/config` is the same file as `.git/config`.
    pub(crate) fn vcs_metadata_component(components: &[String]) -> Option<&str> {
        components
            .iter()
            .find(|name| Self::is_vcs_metadata_name(name))
            .map(String::as_str)
    }

    /// Classify a directory entry without following symbolic links.
    ///
    /// When the filesystem does not provide `d_type` (`FileType::Unknown`),
    /// falls back to `statat(..., SYMLINK_NOFOLLOW)` and verifies the entry
    /// remains on the pinned workspace device.
    #[cfg(unix)]
    pub(crate) fn entry_kind_nofollow(
        &self,
        parent: impl rustix::fd::AsFd,
        name: &std::ffi::CStr,
        hint: rustix::fs::FileType,
    ) -> Result<EntryKind, WorkspaceError> {
        use rustix::fs::{AtFlags, FileType, statat};
        match hint {
            FileType::RegularFile => Ok(EntryKind::RegularFile),
            FileType::Directory => Ok(EntryKind::Directory),
            FileType::Unknown => {
                let stat = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(map_errno)?;
                if stat.st_dev != self.device {
                    return Err(WorkspaceError::Forbidden);
                }
                match FileType::from_raw_mode(stat.st_mode) {
                    FileType::RegularFile => Ok(EntryKind::RegularFile),
                    FileType::Directory => Ok(EntryKind::Directory),
                    _ => Ok(EntryKind::Other),
                }
            }
            _ => Ok(EntryKind::Other),
        }
    }

    #[cfg(unix)]
    pub(crate) fn directory(&self, path: &str) -> Result<rustix::fd::OwnedFd, WorkspaceError> {
        let mut current = self.child_directory(&self.root, ".")?;
        for name in Self::components(path)? {
            current = self.child_directory(&current, &name)?;
        }
        Ok(current)
    }

    #[cfg(unix)]
    pub(crate) fn child_directory(
        &self,
        parent: &rustix::fd::OwnedFd,
        name: &str,
    ) -> Result<rustix::fd::OwnedFd, WorkspaceError> {
        use rustix::fs::{Mode, OFlags, openat};
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(map_errno)?;
        self.check_device(&fd)?;
        Ok(fd)
    }

    #[cfg(unix)]
    pub(crate) fn file(&self, path: &str) -> Result<std::fs::File, WorkspaceError> {
        let mut parts = Self::components(path)?;
        let name = parts.pop().ok_or(WorkspaceError::Forbidden)?;
        let parent = self.directory(&if parts.is_empty() {
            ".".into()
        } else {
            parts.join("/")
        })?;
        self.child_file(&parent, &name)
    }

    #[cfg(unix)]
    pub(crate) fn child_file(
        &self,
        parent: &rustix::fd::OwnedFd,
        name: &str,
    ) -> Result<std::fs::File, WorkspaceError> {
        use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
        // NONBLOCK prevents a replaced FIFO from blocking before its type can be checked.
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(map_errno)?;
        self.check_device(&fd)?;
        if FileType::from_raw_mode(fstat(&fd).map_err(map_errno)?.st_mode) != FileType::RegularFile
        {
            return Err(WorkspaceError::Forbidden);
        }
        Ok(std::fs::File::from(fd))
    }

    #[cfg(unix)]
    fn check_device(&self, fd: &rustix::fd::OwnedFd) -> Result<(), WorkspaceError> {
        if rustix::fs::fstat(fd).map_err(map_errno)?.st_dev != self.device {
            return Err(WorkspaceError::Forbidden);
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn map_errno(error: rustix::io::Errno) -> WorkspaceError {
    if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) {
        WorkspaceError::Forbidden
    } else {
        WorkspaceError::Io(error.into())
    }
}

/// One workspace-relative candidate returned by [`complete_workspace_paths`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspacePathCandidate {
    /// Workspace-relative path without a leading `./`.
    pub relative_path: String,
    /// Whether the entry is a directory on the pinned workspace device.
    pub is_dir: bool,
}

/// List bounded workspace-relative path completion candidates for `prefix` under `root`.
///
/// Uses handle-relative directory listing without following symbolic links, excludes VCS metadata
/// directories (`.git`, `.hg`, `.svn`, `.jj`, ASCII case-insensitive), rejects absolute paths,
/// parent traversal (`..`), NUL bytes, and backslashes, sorts results lexicographically, and
/// caps the output at `limit`. Returns an empty list on non-Unix platforms or invalid inputs.
pub fn complete_workspace_paths(
    root: &Path,
    prefix: &str,
    limit: usize,
) -> Vec<WorkspacePathCandidate> {
    if limit == 0 || prefix.len() > 4096 || prefix.contains('\0') || prefix.contains('\\') {
        return Vec::new();
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        complete_workspace_paths_unix(root, prefix, limit)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (root, prefix, limit);
        Vec::new()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn complete_workspace_paths_unix(
    root: &Path,
    prefix: &str,
    limit: usize,
) -> Vec<WorkspacePathCandidate> {
    use rustix::fs::{AtFlags, FileType, statat};

    let (dir_parts, name_prefix) = match prefix.rfind('/') {
        Some(slash) => {
            let dir_part = &prefix[..slash];
            if dir_part.is_empty() {
                return Vec::new();
            }
            let Ok(parts) = Workspace::components(dir_part) else {
                return Vec::new();
            };
            if Workspace::vcs_metadata_component(&parts).is_some() {
                return Vec::new();
            }
            (parts, &prefix[slash + 1..])
        }
        None => (Vec::new(), prefix),
    };
    if name_prefix == ".." {
        return Vec::new();
    }
    let Ok(ws) = Workspace::open(root) else {
        return Vec::new();
    };
    let Ok(mut dir_fd) = ws.child_directory(&ws.root, ".") else {
        return Vec::new();
    };
    for part in &dir_parts {
        let Ok(next_fd) = ws.child_directory(&dir_fd, part) else {
            return Vec::new();
        };
        dir_fd = next_fd;
    }
    let Ok(mut dir) = rustix::fs::Dir::new(dir_fd) else {
        return Vec::new();
    };
    let dir_prefix = if dir_parts.is_empty() {
        String::new()
    } else {
        format!("{}/", dir_parts.join("/"))
    };
    let mut results = Vec::new();
    while let Some(Ok(entry)) = dir.read() {
        let name_cstr = entry.file_name();
        if name_cstr.to_bytes() == b"." || name_cstr.to_bytes() == b".." {
            continue;
        }
        let Ok(name) = name_cstr.to_str() else {
            continue;
        };
        if Workspace::is_vcs_metadata_name(name) || !name.starts_with(name_prefix) {
            continue;
        }
        let Ok(fd) = dir.fd() else {
            break;
        };
        let hint = entry.file_type();
        let Ok(kind) = ws.entry_kind_nofollow(fd, name_cstr, hint) else {
            continue;
        };
        let is_dir = match kind {
            EntryKind::RegularFile => false,
            EntryKind::Directory => {
                if hint == FileType::Directory {
                    let Ok(stat) = statat(fd, name_cstr, AtFlags::SYMLINK_NOFOLLOW) else {
                        continue;
                    };
                    if stat.st_dev != ws.device
                        || FileType::from_raw_mode(stat.st_mode) != FileType::Directory
                    {
                        continue;
                    }
                }
                true
            }
            EntryKind::Other => continue,
        };
        results.push(WorkspacePathCandidate {
            relative_path: format!("{dir_prefix}{name}"),
            is_dir,
        });
    }
    results.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    results.truncate(limit);
    results
}
