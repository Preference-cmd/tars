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
    // Tokens of other AI coding tools.
    ".codex",
    ".gemini",
    ".config/github-copilot",
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
    ".pgpass",
    ".my.cnf",
    ".claude.json",
    ".claude/.credentials.json",
    // Shell histories routinely contain tokens typed on a command line.
    ".zsh_history",
    ".bash_history",
    ".python_history",
    ".node_repl_history",
    ".psql_history",
    ".mysql_history",
    ".sqlite_history",
    ".local/share/fish/fish_history",
    // Shell startup files are where model API keys are usually exported,
    // because TARS reads them from environment variables.
    ".zshenv",
    ".zprofile",
    ".zshrc",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".config/fish/config.fish",
    ".config/fish/fish_variables",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_files_and_tool_tokens_are_denied() {
        let home = Path::new("/nonexistent-home");
        let denials = home_denials(home);
        for file in [
            ".zshrc",
            ".bash_profile",
            ".profile",
            ".claude.json",
            ".pgpass",
        ] {
            assert!(denials.files.contains(&home.join(file)), "{file}");
        }
        for directory in [".codex", ".gemini", ".ssh"] {
            assert!(
                denials.directories.contains(&home.join(directory)),
                "{directory}"
            );
        }
    }
}
