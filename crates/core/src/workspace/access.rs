use std::path::{Component, Path};
use std::sync::Arc;

/// A selected root resource, pinned for the lifetime of an open session.
pub struct Workspace {
    #[cfg(unix)]
    pub(crate) root: rustix::fd::OwnedFd,
    #[cfg(unix)]
    device: rustix::fs::Dev,
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
