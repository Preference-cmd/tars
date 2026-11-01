//! Embedded release manifest for downloadable external tools.
//!
//! Hashes and URLs are compiled in on purpose: a download never asks the same
//! server it is downloading from which checksum to trust. Upgrading the tool
//! means editing this table.

/// The pinned ripgrep release that TARS checks and installs.
pub const RIPGREP_VERSION: &str = "15.2.0";

/// One platform archive of a pinned tool release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RipgrepAsset {
    /// Rust target triple that the archive was built for.
    pub target: &'static str,
    /// Stable HTTPS URL of the archive.
    pub url: &'static str,
    /// Lowercase hexadecimal SHA-256 of the downloaded archive.
    pub sha256: &'static str,
    /// Lowercase hexadecimal SHA-256 of the `rg` executable extracted from the
    /// archive. The startup check hashes the installed file against this value;
    /// the archive hash above only covers the download.
    pub binary_sha256: &'static str,
}

/// The pinned ripgrep archives, one per supported platform.
///
/// P0 lists the two macOS targets that have a sandbox backend; Linux and
/// Windows entries are added together with their sandbox backends. The hashes
/// are the official `.sha256` values of the GitHub release and were checked
/// against the downloaded archives when this table was written.
pub const RIPGREP_ASSETS: &[RipgrepAsset] = &[
    RipgrepAsset {
        target: "aarch64-apple-darwin",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-aarch64-apple-darwin.tar.gz",
        sha256: "3750b2e93f37e0c692657da574d7019a101c0084da05a790c83fd335bad973e4",
        binary_sha256: "a326a1fb48074202e9ad41e4cd1e389eeea372c8c6f7d7e80da81176d5d9430e",
    },
    RipgrepAsset {
        target: "x86_64-apple-darwin",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-x86_64-apple-darwin.tar.gz",
        sha256: "af7825fcc69a2afc7a7aea55fc9af90e26421d8f20fe59df32e233c0b8a231c1",
        binary_sha256: "0c9a0066db0d26b640777db88045b0ccdd58509a746700e43e1c4ff8707a5ed0",
    },
];

/// The pinned ripgrep archive for this build, or `None` where TARS ships no
/// managed copy.
pub fn ripgrep_asset() -> Option<&'static RipgrepAsset> {
    let target = ripgrep_target()?;
    RIPGREP_ASSETS.iter().find(|asset| asset.target == target)
}

/// The target triple of this build when a managed ripgrep exists for it.
pub fn ripgrep_target() -> Option<&'static str> {
    let target = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else {
        return None;
    };
    Some(target)
}

/// Directory name of the managed release, for example
/// `ripgrep-15.2.0-aarch64-apple-darwin`.
pub fn managed_dir_name(asset: &RipgrepAsset) -> String {
    format!("ripgrep-{RIPGREP_VERSION}-{}", asset.target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_only_the_two_macos_assets_with_full_hashes() {
        assert_eq!(RIPGREP_VERSION, "15.2.0");
        assert_eq!(RIPGREP_ASSETS.len(), 2);
        for asset in RIPGREP_ASSETS {
            assert!(
                asset
                    .url
                    .starts_with("https://github.com/BurntSushi/ripgrep/releases/")
            );
            assert!(asset.url.contains(RIPGREP_VERSION));
            assert!(asset.url.contains(asset.target));
            for hash in [asset.sha256, asset.binary_sha256] {
                assert_eq!(hash.len(), 64);
                assert!(
                    hash.bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                );
            }
            assert!(asset.url.ends_with(&format!("{}.tar.gz", asset.target)));
        }
    }

    #[test]
    fn host_lookup_matches_the_compiled_target() {
        assert_eq!(ripgrep_target(), ripgrep_asset().map(|asset| asset.target));
        if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
            assert_eq!(ripgrep_target(), Some("aarch64-apple-darwin"));
        } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
            assert_eq!(ripgrep_target(), Some("x86_64-apple-darwin"));
        } else {
            assert_eq!(ripgrep_target(), None);
        }
        if let Some(asset) = ripgrep_asset() {
            assert_eq!(
                managed_dir_name(asset),
                format!("ripgrep-{RIPGREP_VERSION}-{}", asset.target)
            );
        }
    }
}
