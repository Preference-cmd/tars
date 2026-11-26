use std::path::{Component, Path, PathBuf};

const MAX_SYMLINK_PROTECTION_DEPTH: usize = 32;

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

/// Which filesystem the read allow covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReadScope {
    /// Only the policy's read roots and the platform system and toolchain
    /// baseline. Intended for tools that only need the workspace.
    #[default]
    Roots,
    /// The whole host filesystem. Credential denials still apply and are
    /// supplied by the backend.
    Host,
}

/// A platform-neutral description of the filesystem and network boundary.
///
/// Every existing path is stored as a canonical absolute path. A read-only
/// subpath may have a missing leaf and is then stored as a canonical parent
/// plus that leaf. A protected entry covers the entry itself, not its
/// descendants. The backend translates this value into its own policy
/// language; nothing in it names a backend.
/// Signal isolation (signals only inside the sandbox) is always required in
/// P0 and is checked through the backend's capability report rather than a
/// policy field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxPolicy {
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    pub(crate) read_roots: Vec<PathBuf>,
    pub(crate) write_roots: Vec<PathBuf>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    pub(crate) read_only_subpaths: Vec<PathBuf>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    pub(crate) protected_entries: Vec<PathBuf>,
    #[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
    pub(crate) denied_paths: Vec<PathBuf>,
    read_scope: ReadScope,
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
            protected_entries: Vec::new(),
            denied_paths: Vec::new(),
            read_scope: ReadScope::Roots,
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

    /// Keep a path inside a writable root read-only and non-modifiable.
    ///
    /// `root` must resolve to an existing directory inside `write_roots`.
    /// `path` is aligned onto `root` without dereferencing intermediate or leaf
    /// symlinks inside `root`, and must fall strictly inside `root` (missing
    /// intermediate or leaf directories are allowed).
    ///
    /// Every lexical component strictly below `root` — including intermediate
    /// directories, intermediate symlinks, and the leaf entry — is added to
    /// `protected_entries` so a command cannot rename, delete, or replace an
    /// ancestor or symlink to bypass the protection, while sibling entries under
    /// those intermediate directories remain writable. The final lexical path is
    /// also added to `read_only_subpaths` so its entire subtree stays read-only.
    /// Whenever the final path or any intermediate component is a symlink whose
    /// normalized target falls inside a writable root, that target chain is
    /// recursively protected by the same rules; symlinks pointing outside
    /// `write_roots` still keep their in-root link entry protected without
    /// failing with [`SandboxPolicyError::OutsideWritable`].
    pub fn with_protected_read_only_path(
        self,
        root: impl AsRef<Path>,
        path: impl AsRef<Path>,
    ) -> Result<Self, SandboxPolicyError> {
        let root = canonical_directory(root.as_ref())?;
        if !self.write_roots.contains(&root) {
            return Err(SandboxPolicyError::OutsideWritable(root));
        }
        let path = path.as_ref();
        validate_path(path)?;
        let Some(rel) = split_under_root(&root, path) else {
            let projected = canonical_or_projected(path)?;
            return Err(SandboxPolicyError::OutsideWritable(projected));
        };
        if rel.is_empty() {
            return Err(SandboxPolicyError::OutsideWritable(root));
        }
        let mut visited = std::collections::HashSet::new();
        self.protect_read_only_within_root(&root, &rel, &mut visited, 0)
    }

    fn protect_read_only_within_root(
        mut self,
        root: &Path,
        rel_components: &[std::ffi::OsString],
        visited: &mut std::collections::HashSet<PathBuf>,
        depth: usize,
    ) -> Result<Self, SandboxPolicyError> {
        if rel_components.is_empty() || depth > MAX_SYMLINK_PROTECTION_DEPTH {
            return Ok(self);
        }
        let lexical_target = rel_components
            .iter()
            .fold(root.to_path_buf(), |path, part| path.join(part));
        validate_path(&lexical_target)?;
        if !visited.insert(lexical_target) {
            return Ok(self);
        }

        let mut current = root.to_path_buf();
        let last_index = rel_components.len() - 1;
        for (index, part) in rel_components.iter().enumerate() {
            current.push(part);
            validate_path(&current)?;
            self.protected_entries.push(current.clone());
            if index == last_index {
                self.read_only_subpaths.push(current.clone());
            }

            if let Some(target) = symlink_target(&current) {
                let target_path = rel_components[index + 1..]
                    .iter()
                    .fold(target, |path, rest| path.join(rest));
                let normalized = lexical_normalize(&target_path);
                if validate_path(&normalized).is_ok()
                    && let Some((target_root, target_rel)) =
                        find_containing_write_root(&self.write_roots, &normalized, depth + 1)
                {
                    self = self.protect_read_only_within_root(
                        &target_root,
                        &target_rel,
                        visited,
                        depth + 1,
                    )?;
                }
            }
        }
        normalize(&mut self.protected_entries);
        normalize(&mut self.read_only_subpaths);
        Ok(self)
    }

    /// Protect the parts of a workspace's `.git` that can execute outside the
    /// sandbox: hooks, config, submodule metadata and linked-worktree metadata.
    ///
    /// The `.git` entry itself becomes non-creatable, non-deletable and
    /// non-renamable, so a command cannot move it aside, write hooks under the
    /// new name and move it back. Git objects, refs and logs stay writable.
    /// When `.git` is missing, the entry rule blocks creating it; when it
    /// resolves outside the workspace, writes through it are already outside
    /// the writable roots. Nested repositories are not covered.
    pub fn with_git_metadata_protection(
        mut self,
        root: impl AsRef<Path>,
    ) -> Result<Self, SandboxPolicyError> {
        let root = canonical_directory(root.as_ref())?;
        let git = root.join(".git");
        self = self.with_protected_entry(&git)?;
        let mut bases = vec![git.clone()];
        match symlink_target(&git) {
            Some(target) => {
                let projected = canonical_or_projected(&lexical_normalize(&target))?;
                if !projected.starts_with(&root) {
                    return Ok(self);
                }
                // Protect the target entry too: otherwise the command could
                // replace the target with another symlink after this policy
                // was built.
                self = self.with_protected_entry(&projected)?;
                bases.push(projected);
            }
            None => {
                let Ok(resolved) = std::fs::canonicalize(&git) else {
                    return Ok(self);
                };
                if !resolved.is_dir() || !resolved.starts_with(&root) {
                    return Ok(self);
                }
                bases.push(resolved);
            }
        }
        for base in bases {
            for name in ["hooks", "config", "config.worktree", "modules", "worktrees"] {
                let path = base.join(name);
                validate_path(&path)?;
                self.read_only_subpaths.push(path.clone());
                // A hook or config entry may itself be a symlink to another
                // workspace path, including a target that does not exist yet;
                // that target must be read-only too.
                let target = symlink_target(&path)
                    .map(|target| canonical_or_projected(&lexical_normalize(&target)))
                    .transpose()?
                    .or_else(|| std::fs::canonicalize(&path).ok());
                if let Some(target) = target
                    && target.starts_with(&root)
                {
                    self = self.with_protected_entry(&target)?;
                    self.read_only_subpaths.push(target);
                }
            }
        }
        normalize(&mut self.read_only_subpaths);
        Ok(self)
    }

    /// Deny writes to the entry itself while leaving its descendants writable.
    ///
    /// The parent may be missing; the entry is then stored as a canonical
    /// ancestor plus the remaining lexical components.
    pub fn with_protected_entry(
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
        let parent = canonical_or_projected(parent)?;
        let entry = parent.join(name);
        validate_path(&entry)?;
        self.protected_entries.push(entry);
        normalize(&mut self.protected_entries);
        Ok(self)
    }

    /// Deny both reads and writes for an additional path.
    ///
    /// The path may be missing; it is then stored as a canonical ancestor plus
    /// the remaining lexical components, so creating it later stays denied.
    pub fn with_denied_path(mut self, path: impl AsRef<Path>) -> Result<Self, SandboxPolicyError> {
        self.denied_paths
            .push(canonical_or_projected(path.as_ref())?);
        normalize(&mut self.denied_paths);
        Ok(self)
    }

    /// Set which filesystem the read allow covers.
    pub fn with_read_scope(mut self, scope: ReadScope) -> Self {
        self.read_scope = scope;
        self
    }

    /// Set whether outbound network access is available.
    pub fn with_network(mut self, network: NetworkAccess) -> Self {
        self.network = network;
        self
    }

    /// Enable or disable the platform system and toolchain read baseline.
    ///
    /// The backend resolves the platform paths, including the TARS process's
    /// home toolchain directories, when it generates its policy.
    pub fn with_system_baseline(mut self, enabled: bool) -> Self {
        self.system_baseline = enabled;
        self
    }

    /// The requested network access.
    pub fn network(&self) -> NetworkAccess {
        self.network
    }

    /// The filesystem covered by the read allow.
    pub fn read_scope(&self) -> ReadScope {
        self.read_scope
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
    /// A sandbox path contains a `.` or `..` component.
    #[error("sandbox path contains a relative component: {0}")]
    RelativeComponent(PathBuf),
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

/// Resolve the longest existing ancestor and append the remaining components.
///
/// This keeps a rule stable for a path that the sandbox itself may later
/// create, without inventing a canonical form of the missing part.
fn canonical_or_projected(path: &Path) -> Result<PathBuf, SandboxPolicyError> {
    validate_path(path)?;
    let mut tail = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match std::fs::canonicalize(&current) {
            Ok(canonical) => {
                validate_path(&canonical)?;
                let mut projected = canonical;
                for name in tail.iter().rev() {
                    projected.push(name);
                }
                validate_path(&projected)?;
                return Ok(projected);
            }
            Err(source) => {
                let Some(name) = current.file_name() else {
                    return Err(SandboxPolicyError::Resolve {
                        path: path.to_path_buf(),
                        source,
                    });
                };
                tail.push(name.to_os_string());
                current.pop();
            }
        }
    }
}

fn validate_path(path: &Path) -> Result<(), SandboxPolicyError> {
    if !path.is_absolute() {
        return Err(SandboxPolicyError::AbsolutePath(path.to_path_buf()));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SandboxPolicyError::RelativeComponent(path.to_path_buf()));
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

fn symlink_target(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_symlink() {
        return None;
    }
    let target = std::fs::read_link(path).ok()?;
    if target.is_absolute() {
        Some(target)
    } else {
        Some(path.parent()?.join(target))
    }
}

fn split_under_root(root: &Path, path: &Path) -> Option<Vec<std::ffi::OsString>> {
    if let Ok(rel) = path.strip_prefix(root) {
        return Some(
            rel.components()
                .filter_map(|component| match component {
                    Component::Normal(name) => Some(name.to_os_string()),
                    _ => None,
                })
                .collect(),
        );
    }
    let mut prefix = PathBuf::new();
    let mut iter = path.components();
    while let Some(component) = iter.next() {
        prefix.push(component.as_os_str());
        if matches!(component, Component::Normal(_))
            && let Ok(canon) = std::fs::canonicalize(&prefix)
            && canon == root
        {
            let rel = iter
                .filter_map(|part| match part {
                    Component::Normal(name) => Some(name.to_os_string()),
                    _ => None,
                })
                .collect();
            return Some(rel);
        }
    }
    None
}

fn find_containing_write_root(
    write_roots: &[PathBuf],
    path: &Path,
    depth: usize,
) -> Option<(PathBuf, Vec<std::ffi::OsString>)> {
    if depth > MAX_SYMLINK_PROTECTION_DEPTH {
        return None;
    }
    let mut best: Option<(PathBuf, Vec<std::ffi::OsString>)> = None;
    for writable in write_roots {
        if let Some(rel) = split_under_root(writable, path)
            && !rel.is_empty()
            && best.as_ref().is_none_or(|(best_root, _)| {
                writable.components().count() > best_root.components().count()
            })
        {
            best = Some((writable.clone(), rel));
        }
    }
    if best.is_some() {
        return best;
    }

    // Handle `symlink-out-and-back`: `path` may live outside `write_roots`
    // while an intermediate component resolves back into a `write_root`
    // subdirectory (or dangles back into one).
    let mut prefix = PathBuf::new();
    let components: Vec<_> = path.components().collect();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        let tail = &components[index + 1..];
        if let Ok(canon) = std::fs::canonicalize(&prefix) {
            let mut matched: Option<(PathBuf, Vec<std::ffi::OsString>)> = None;
            for writable in write_roots {
                if let Ok(inside) = canon.strip_prefix(writable) {
                    let mut rel: Vec<std::ffi::OsString> = inside
                        .components()
                        .filter_map(|part| match part {
                            Component::Normal(name) => Some(name.to_os_string()),
                            _ => None,
                        })
                        .collect();
                    rel.extend(tail.iter().filter_map(|part| match part {
                        Component::Normal(name) => Some(name.to_os_string()),
                        _ => None,
                    }));
                    if !rel.is_empty()
                        && matched.as_ref().is_none_or(|(best_root, _)| {
                            writable.components().count() > best_root.components().count()
                        })
                    {
                        matched = Some((writable.clone(), rel));
                    }
                }
            }
            if matched.is_some() {
                return matched;
            }
        } else if let Some(target) = symlink_target(&prefix) {
            let next = tail
                .iter()
                .fold(target, |acc, part| acc.join(part.as_os_str()));
            let normalized = lexical_normalize(&next);
            if validate_path(&normalized).is_ok() {
                return find_containing_write_root(write_roots, &normalized, depth + 1);
            }
        }
    }
    None
}

/// Remove `.` and `..` without touching the filesystem.
///
/// Only used for symlink targets before projection: a missing target cannot
/// be canonicalized, but its lexical form still has to be a valid policy path.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}
