//! Pinned workspace roots and safe relative filesystem access.
//!
//! On Unix, each path component is opened relative to an already opened directory,
//! without following symbolic links. Directory handles keep referring to the selected
//! objects after renames; this is neither an atomic filesystem snapshot nor an OS sandbox.
mod access;
mod write;

#[cfg(unix)]
pub(crate) use access::map_errno;
pub use access::{Workspace, WorkspaceError};
#[cfg_attr(not(unix), allow(unused_imports))]
pub(crate) use write::AtomicWriteError;
pub use write::FileVersion;

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests;
