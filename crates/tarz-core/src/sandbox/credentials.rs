//! Credential locations denied to every sandboxed read scope.
//!
//! The list is platform-neutral home-relative data. A backend adds its own
//! platform stores, such as the macOS keychains, when it generates a policy.

use std::path::{Path, PathBuf};

const HOME_DIRECTORIES: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".config/gh",
    ".password-store",
    ".zsh_sessions",
];

const HOME_FILES: &[&str] = &[
    ".netrc",
    ".git-credentials",
    ".config/git/credentials",
    ".npmrc",
    ".pypirc",
    ".gem/credentials",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".vault-token",
    ".terraform.d/credentials.tfrc.json",
    // Shell histories routinely contain tokens typed on a command line.
    ".zsh_history",
    ".bash_history",
];

/// Credential paths under one home, split by how a backend must match them.
pub(crate) struct HomeDenials {
    pub(crate) directories: Vec<PathBuf>,
    pub(crate) files: Vec<PathBuf>,
}

/// Every credential path under `home`, including resolved symlink targets.
pub(crate) fn home_denials(home: &Path) -> HomeDenials {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    collect(home, HOME_DIRECTORIES, &mut directories);
    collect(home, HOME_FILES, &mut files);
    directories.sort();
    directories.dedup();
    files.sort();
    files.dedup();
    HomeDenials { directories, files }
}

fn collect(home: &Path, relatives: &[&str], target: &mut Vec<PathBuf>) {
    for relative in relatives {
        let path = home.join(relative);
        if let Ok(resolved) = std::fs::canonicalize(&path) {
            target.push(resolved);
        }
        target.push(path);
    }
}
