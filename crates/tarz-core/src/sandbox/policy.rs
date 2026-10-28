use std::path::{Path, PathBuf};

/// Network access requested by a sandbox policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetworkAccess {
    /// No outbound or inbound traffic, including loopback and DNS.
    #[default]
    None,
    /// Outbound traffic is allowed. P0 has no allowlist or loopback-only mode.
    Open,
}

/// A platform-neutral description of the filesystem and network boundary.
///
/// Every existing path is stored as a canonical absolute path. A read-only
/// subpath may have a missing leaf and is then stored as a canonical parent
/// plus that leaf. The backend translates this value into its own policy
/// language; nothing in it names a backend.
/// Signal isolation (signals only inside the sandbox) is always required in
/// P0 and is checked through the backend's capability report rather than a
/// policy field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) read_roots: Vec<PathBuf>,
    pub(crate) write_roots: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) read_only_subpaths: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) denied_paths: Vec<PathBuf>,
    network: NetworkAccess,
    system_baseline: bool,
}

impl SandboxPolicy {
    /// Build a policy from canonicalizable directory roots.
    ///
    /// The system and toolchain read baseline is enabled by default; network
    /// access is denied by default.
    pub fn new(
        read_roots: impl IntoIterator<Item = PathBuf>,
        write_roots: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Self, SandboxPolicyError> {
        let mut read_roots = read_roots
            .into_iter()
            .map(|path| canonical_directory(&path))
            .collect::<Result<Vec<_>, _>>()?;
        let mut write_roots = write_roots
            .into_iter()
            .map(|path| canonical_directory(&path))
            .collect::<Result<Vec<_>, _>>()?;
        normalize(&mut read_roots);
        normalize(&mut write_roots);
        Ok(Self {
            read_roots,
            write_roots,
            read_only_subpaths: Vec::new(),
            denied_paths: Vec::new(),
            network: NetworkAccess::None,
            system_baseline: true,
        })
    }

    /// Keep a path writable-root-relative read-only.
    ///
    /// The parent must resolve inside a writable root; the leaf need not
    /// exist, so a missing `.git` is still denied when the command creates it.
    /// An existing leaf that resolves inside a writable root is denied at the
    /// resolved path too; a symlink resolving outside is already outside the
    /// writable roots and is denied by default.
    pub fn with_read_only_subpath(
        mut self,
        path: impl AsRef<Path>,
    ) -> Result<Self, SandboxPolicyError> {
        let path = path.as_ref();
        validate_path(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| SandboxPolicyError::InvalidPath(path.to_path_buf()))?;
        let name = path
            .file_name()
            .ok_or_else(|| SandboxPolicyError::InvalidPath(path.to_path_buf()))?;
        let parent = canonical_directory(parent)?;
        let lexical = parent.join(name);
        if !self
            .write_roots
            .iter()
            .any(|root| lexical.starts_with(root))
        {
            return Err(SandboxPolicyError::OutsideWritable(lexical));
        }
        self.read_only_subpaths.push(lexical.clone());
        if let Ok(resolved) = std::fs::canonicalize(&lexical)
            && self
                .write_roots
                .iter()
                .any(|root| resolved.starts_with(root))
        {
            self.read_only_subpaths.push(resolved);
        }
        normalize(&mut self.read_only_subpaths);
        Ok(self)
    }

    /// Deny both reads and writes for an additional path.
    pub fn with_denied_path(mut self, path: impl AsRef<Path>) -> Result<Self, SandboxPolicyError> {
        self.denied_paths.push(canonical_existing(path.as_ref())?);
        normalize(&mut self.denied_paths);
        Ok(self)
    }

    /// Set whether outbound network access is available.
    pub fn with_network(mut self, network: NetworkAccess) -> Self {
        self.network = network;
        self
    }

    /// Enable or disable the platform system and toolchain read baseline.
    ///
    /// The backend resolves the platform paths, including the Tarz process's
    /// home toolchain directories, when it generates its policy.
    pub fn with_system_baseline(mut self, enabled: bool) -> Self {
        self.system_baseline = enabled;
        self
    }

    /// The requested network access.
    pub fn network(&self) -> NetworkAccess {
        self.network
    }

    /// Whether the backend should add its platform system and toolchain reads.
    pub fn system_baseline(&self) -> bool {
        self.system_baseline
    }
}

/// Why a policy path could not be accepted.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxPolicyError {
    /// A sandbox path must be absolute.
    #[error("sandbox path must be absolute: {0}")]
    AbsolutePath(PathBuf),
    /// A sandbox path has no parent or file name.
    #[error("sandbox path has no parent or file name: {0}")]
    InvalidPath(PathBuf),
    /// A sandbox path is not valid UTF-8.
    #[error("sandbox path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// A sandbox path contains a control character.
    #[error("sandbox path contains a control character: {0}")]
    ControlCharacter(PathBuf),
    /// A sandbox path could not be resolved.
    #[error("could not resolve sandbox path {path}: {source}")]
    Resolve {
        /// The path that could not be resolved.
        path: PathBuf,
        /// The filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A filesystem root is not a directory.
    #[error("sandbox root is not a directory: {0}")]
    NotDirectory(PathBuf),
    /// A read-only subpath is outside every writable root.
    #[error("read-only sandbox subpath is outside every writable root: {0}")]
    OutsideWritable(PathBuf),
}

fn canonical_directory(path: &Path) -> Result<PathBuf, SandboxPolicyError> {
    let path = canonical_existing(path)?;
    if !path.is_dir() {
        return Err(SandboxPolicyError::NotDirectory(path));
    }
    Ok(path)
}

fn canonical_existing(path: &Path) -> Result<PathBuf, SandboxPolicyError> {
    validate_path(path)?;
    let canonical = std::fs::canonicalize(path).map_err(|source| SandboxPolicyError::Resolve {
        path: path.to_path_buf(),
        source,
    })?;
    validate_path(&canonical)?;
    Ok(canonical)
}

fn validate_path(path: &Path) -> Result<(), SandboxPolicyError> {
    if !path.is_absolute() {
        return Err(SandboxPolicyError::AbsolutePath(path.to_path_buf()));
    }
    let Some(text) = path.to_str() else {
        return Err(SandboxPolicyError::NonUtf8Path(path.to_path_buf()));
    };
    if text.chars().any(char::is_control) {
        return Err(SandboxPolicyError::ControlCharacter(path.to_path_buf()));
    }
    Ok(())
}

fn normalize(paths: &mut Vec<PathBuf>) {
    paths.sort();
    paths.dedup();
}
