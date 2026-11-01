//! Runtime checks for third-party tools and consent-gated installation.
//!
//! The sandboxed search tool runs `rg` (ripgrep) as an external program
//! instead of searching inside the process. TARS does not bundle or silently
//! install that program: the release is pinned in [`RipgrepAsset`], resolved
//! from the shared configuration at startup, and only downloaded after an
//! explicit yes.
//!
//! # Sources
//!
//! `[external_tools] rg` selects where the executable comes from:
//!
//! - `managed` (the default) is the copy TARS installs under
//!   `$XDG_DATA_HOME/tars/tools/ripgrep-15.2.0-<target>/rg`, falling back to
//!   `~/.local/share/tars/...`. Its SHA-256 is checked before it is ever
//!   executed.
//! - `system` is the first `rg` found on `PATH` when TARS starts; its version
//!   is recorded and a version older than [`MIN_SYSTEM_MAJOR`] is unusable.
//! - an absolute path names the executable directly; it must exist and its
//!   version is recorded.
//!
//! # One check per process
//!
//! [`ExternalTools::resolve`] performs the check for one configuration and
//! [`resolve_process`] caches the first result for the life of the process.
//! Later session binding uses [`ToolStatus::ready_path`], the resolved
//! absolute path; the tool is never re-resolved behind the caller's back.
//! Every failure is reported as [`ToolState::Unavailable`] with a short
//! reason, never as a silent fallback.
//!
//! # Consent
//!
//! [`install_managed_rg`] is the only operation that uses the network. It is
//! never called by a check: the CLI asks the user first, `--yes` answers the
//! question, and a non-interactive invocation without `--yes` refuses. A
//! download is HTTPS-only, is restricted to `github.com` and GitHub's release
//! asset hosts (redirects included), is capped in size and time, honours proxy
//! environment variables, verifies the pinned SHA-256, extracts only the
//! single `rg` entry from the tar.gz archive with mode 0755, atomically
//! renames it into place and then runs `rg --version` once. A failure leaves
//! nothing at the final path.

mod download;
mod manifest;
mod status;

pub use download::{DownloadError, InstallError, install_managed_rg, install_managed_rg_with};
pub use manifest::{
    RIPGREP_ASSETS, RIPGREP_VERSION, RipgrepAsset, managed_dir_name, ripgrep_asset, ripgrep_target,
};
pub use status::{
    ExternalTools, MIN_SYSTEM_MAJOR, ToolEnvironment, ToolSource, ToolSourceError, ToolState,
    ToolStatus, managed_rg_path, resolve_process,
};
