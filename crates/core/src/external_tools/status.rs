//! Resolution and readiness of the configured external tools.
//!
//! A tool is resolved from its configured source when the process starts; the
//! result is cached by [`resolve_process`] and later session binding reads the
//! resolved absolute path from [`ToolStatus::ready_path`]. Resolution never
//! downloads anything and never executes a file that failed its check.

use super::manifest::{RIPGREP_VERSION, ripgrep_asset};
use crate::config::Config;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Name of the ripgrep executable, including the platform suffix.
pub(super) const RG_BINARY: &str = if cfg!(windows) { "rg.exe" } else { "rg" };

/// The lowest ripgrep major version accepted from `PATH`.
///
/// The search tool relies on flags and behavior that older releases do not
/// provide; a configured absolute path is trusted as given, but a `PATH`
/// lookup must satisfy this floor.
pub const MIN_SYSTEM_MAJOR: u64 = 14;

/// Refuse to hash a managed file larger than this; the pinned executable is a
/// few MiB, so an oversized file is not the release TARS installed.
const MAX_MANAGED_BINARY_BYTES: u64 = 64 * 1024 * 1024;

/// Process environment read by the external tool checks.
///
/// Entry points capture it once with [`ToolEnvironment::from_process`]. Tests
/// and embedders can build one explicitly instead of mutating the process
/// environment, so a check never depends on hidden global state.
#[derive(Clone, Debug, Default)]
pub struct ToolEnvironment {
    /// `$XDG_DATA_HOME`, used only when it is an absolute path.
    pub(crate) data_home: Option<PathBuf>,
    /// `$HOME`, the fallback data directory root.
    pub(crate) home: Option<PathBuf>,
    /// `$PATH` for the `system` source.
    pub(crate) path: Option<OsString>,
    /// Proxy URL from `$HTTPS_PROXY` / `$https_proxy` / `$ALL_PROXY` /
    /// `$all_proxy`, first one set.
    pub(crate) proxy: Option<String>,
    /// `$NO_PROXY` / `$no_proxy` bypass list for the proxy.
    pub(crate) no_proxy: Option<String>,
}

impl ToolEnvironment {
    /// Capture the process environment once.
    pub fn from_process() -> Self {
        fn variable(name: &str) -> Option<OsString> {
            std::env::var_os(name)
        }
        let proxy = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty());
        let no_proxy = ["NO_PROXY", "no_proxy"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty());
        Self {
            data_home: variable("XDG_DATA_HOME").map(PathBuf::from),
            home: variable("HOME").map(PathBuf::from),
            path: variable("PATH"),
            proxy,
            no_proxy,
        }
    }

    /// The XDG data root: `$XDG_DATA_HOME`, else `$HOME/.local/share`.
    ///
    /// A relative `XDG_DATA_HOME` is ignored, matching the XDG convention and
    /// the configuration-directory rule.
    pub(crate) fn data_root(&self) -> Option<PathBuf> {
        match self.data_home.as_deref().filter(|path| path.is_absolute()) {
            Some(root) => Some(root.to_owned()),
            None => self
                .home
                .as_ref()
                .map(|home| home.join(".local").join("share")),
        }
    }
}

/// Where the configured `rg` executable comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ToolSource {
    /// The copy TARS installs under the user data directory, pinned by SHA-256.
    #[default]
    Managed,
    /// The first `rg` found on `PATH` when TARS starts.
    System,
    /// An absolute path written in `[external_tools]`.
    Path(PathBuf),
}

impl ToolSource {
    /// Parse one `[external_tools] rg` value.
    pub fn parse(value: &str) -> Result<Self, ToolSourceError> {
        let value = value.trim();
        match value {
            "managed" => Ok(Self::Managed),
            "system" => Ok(Self::System),
            path if !path.is_empty() && Path::new(path).is_absolute() => {
                Ok(Self::Path(PathBuf::from(path)))
            }
            _ => Err(ToolSourceError),
        }
    }

    /// Stable source name used by status output and `--json`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::System => "system",
            Self::Path(_) => "path",
        }
    }
}

impl std::fmt::Display for ToolSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Managed => formatter.write_str("managed"),
            Self::System => formatter.write_str("system"),
            Self::Path(path) => write!(formatter, "{}", path.display()),
        }
    }
}

/// Why an `[external_tools] rg` value is not a usable source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("must be \"managed\", \"system\", or an absolute path")]
pub struct ToolSourceError;

/// Readiness of one checked tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    /// The tool passed every check for its source.
    Ready,
    /// The tool cannot be used; the text is a short user-facing reason.
    Unavailable(String),
}

impl ToolState {
    /// Whether the tool is ready to run.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// The checked state of one external tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStatus {
    /// Stable tool name: `rg`.
    pub name: &'static str,
    /// Configured source this check ran for.
    pub source: ToolSource,
    /// Absolute executable path when the source resolved to one. A managed
    /// path is reported even when the file is missing, because installation
    /// targets exactly that location.
    pub path: Option<PathBuf>,
    /// Version recorded from `rg --version`, when it could be read.
    pub version: Option<String>,
    /// Readiness and, when unavailable, the reason.
    pub state: ToolState,
}

impl ToolStatus {
    /// Whether the tool passed every check.
    pub fn is_ready(&self) -> bool {
        self.state.is_ready()
    }

    /// Why the tool is unusable, or `None` when it is ready.
    pub fn unavailable_reason(&self) -> Option<&str> {
        match &self.state {
            ToolState::Ready => None,
            ToolState::Unavailable(reason) => Some(reason),
        }
    }

    /// The absolute path a caller may execute, only when the tool is ready.
    ///
    /// Session binding uses this value; an unavailable tool never exposes a
    /// path, so a caller cannot accidentally run an archive whose hash failed
    /// or a `PATH` entry that was rejected.
    pub fn ready_path(&self) -> Option<&Path> {
        if self.is_ready() {
            self.path.as_deref()
        } else {
            None
        }
    }

    pub(super) fn ready(
        source: ToolSource,
        path: Option<PathBuf>,
        version: Option<String>,
    ) -> Self {
        Self {
            name: "rg",
            source,
            path,
            version,
            state: ToolState::Ready,
        }
    }

    pub(super) fn unavailable(
        source: ToolSource,
        path: Option<PathBuf>,
        version: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            name: "rg",
            source,
            path,
            version,
            state: ToolState::Unavailable(reason.into()),
        }
    }
}

/// The managed ripgrep location for this build and data directory.
///
/// Returns `None` when TARS has no managed ripgrep for the platform or the
/// user data directory cannot be determined. The file is not checked for
/// existence here.
pub fn managed_rg_path(environment: &ToolEnvironment) -> Option<PathBuf> {
    let asset = ripgrep_asset()?;
    managed_path(environment, asset.target)
}

fn managed_path(environment: &ToolEnvironment, target: &str) -> Option<PathBuf> {
    let root = environment.data_root()?;
    Some(
        root.join("tars")
            .join("tools")
            .join(format!("ripgrep-{RIPGREP_VERSION}-{target}"))
            .join(RG_BINARY),
    )
}

/// The external tools checked for one process.
#[derive(Clone, Debug)]
pub struct ExternalTools {
    rg: ToolStatus,
}

impl ExternalTools {
    /// Check every configured tool, capturing the process environment.
    pub fn resolve(config: &Config) -> Self {
        Self::resolve_with(config, &ToolEnvironment::from_process())
    }

    /// Check every configured tool against an explicit environment.
    pub fn resolve_with(config: &Config, environment: &ToolEnvironment) -> Self {
        Self {
            rg: resolve_source(config.ripgrep_source(), environment),
        }
    }

    /// The ripgrep check.
    pub fn rg(&self) -> &ToolStatus {
        &self.rg
    }
}

/// Resolve the external tools once for the process and reuse the result.
///
/// The first call performs the checks; later calls return the recorded result
/// even if the environment or configuration changed. A process that replaces a
/// tool on purpose (such as `tars tools install`) resolves again explicitly
/// instead of reading this cache.
pub fn resolve_process(config: &Config) -> &'static ExternalTools {
    static RESOLVED: OnceLock<ExternalTools> = OnceLock::new();
    RESOLVED.get_or_init(|| ExternalTools::resolve(config))
}

/// The pinned archive expectations used by a check.
///
/// Production builds this from [`ripgrep_asset`]; tests inject the hash of a
/// fixture executable so every source can be exercised without the real
/// release. The target participates only in the managed directory name.
pub(super) struct ExpectedAsset {
    pub(super) target: String,
    pub(super) binary_sha256: String,
}

impl ExpectedAsset {
    pub(super) fn embedded() -> Option<Self> {
        ripgrep_asset().map(|asset| Self {
            target: asset.target.to_owned(),
            binary_sha256: asset.binary_sha256.to_owned(),
        })
    }
}

fn resolve_source(source: &ToolSource, environment: &ToolEnvironment) -> ToolStatus {
    match source {
        ToolSource::Managed => resolve_managed(environment, ExpectedAsset::embedded().as_ref()),
        ToolSource::System => resolve_system(environment),
        ToolSource::Path(path) => resolve_configured(path),
    }
}

fn resolve_managed(environment: &ToolEnvironment, expected: Option<&ExpectedAsset>) -> ToolStatus {
    let source = ToolSource::Managed;
    let Some(expected) = expected else {
        return ToolStatus::unavailable(
            source,
            None,
            None,
            format!("no managed ripgrep {RIPGREP_VERSION} exists for this platform"),
        );
    };
    let Some(path) = managed_path(environment, &expected.target) else {
        return ToolStatus::unavailable(
            source,
            None,
            None,
            "cannot locate the user data directory; set XDG_DATA_HOME or HOME",
        );
    };
    match std::fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > MAX_MANAGED_BINARY_BYTES {
                return ToolStatus::unavailable(
                    source,
                    Some(path),
                    None,
                    "managed rg has an unexpected size; run `tars tools install rg`",
                );
            }
            match super::download::sha256_file(&path) {
                Ok(actual) if actual == expected.binary_sha256 => match probe_version(&path) {
                    Ok(version) => ToolStatus::ready(source, Some(path), Some(version)),
                    Err(reason) => ToolStatus::unavailable(source, Some(path), None, reason),
                },
                Ok(_) => ToolStatus::unavailable(
                    source,
                    Some(path),
                    None,
                    "managed rg failed its SHA-256 check; run `tars tools install rg`",
                ),
                Err(_) => ToolStatus::unavailable(
                    source,
                    Some(path),
                    None,
                    "managed rg could not be read for verification; run `tars tools install rg`",
                ),
            }
        }
        _ => ToolStatus::unavailable(
            source,
            Some(path),
            None,
            "managed rg is not installed; run `tars tools install rg`",
        ),
    }
}

fn resolve_system(environment: &ToolEnvironment) -> ToolStatus {
    let source = ToolSource::System;
    let Some(path_variable) = environment.path.as_deref() else {
        return ToolStatus::unavailable(source, None, None, "PATH is not set");
    };
    let Some(path) = find_on_path(path_variable) else {
        return ToolStatus::unavailable(
            source,
            None,
            None,
            "rg was not found on PATH; run `tars tools install rg`",
        );
    };
    match probe_version(&path) {
        Ok(version) if major_version(&version) >= MIN_SYSTEM_MAJOR => {
            ToolStatus::ready(source, Some(path), Some(version))
        }
        Ok(version) => ToolStatus::unavailable(
            source,
            Some(path),
            Some(version),
            format!(
                "rg on PATH is older than the required version {MIN_SYSTEM_MAJOR}; run `tars tools install rg`"
            ),
        ),
        Err(reason) => ToolStatus::unavailable(source, Some(path), None, reason),
    }
}

fn resolve_configured(path: &Path) -> ToolStatus {
    let source = ToolSource::Path(path.to_owned());
    if !path.is_absolute() {
        return ToolStatus::unavailable(
            source,
            Some(path.to_owned()),
            None,
            "the configured rg path must be absolute",
        );
    }
    if !path.is_file() {
        return ToolStatus::unavailable(
            source,
            Some(path.to_owned()),
            None,
            "the configured rg path does not exist or is not a file",
        );
    }
    match probe_version(path) {
        Ok(version) => ToolStatus::ready(source, Some(path.to_owned()), Some(version)),
        Err(reason) => ToolStatus::unavailable(source, Some(path.to_owned()), None, reason),
    }
}

/// Run `rg --version` once and return the reported version.
///
/// The environment is cleared and nothing from the tool's output is echoed in
/// the failure reason, so a suspicious executable can neither read process
/// secrets nor inject text into status output. A managed binary is only run
/// after its SHA-256 matched; this function does not verify hashes itself.
pub(super) fn probe_version(path: &Path) -> Result<String, String> {
    let output = std::process::Command::new(path)
        .arg("--version")
        .env_clear()
        .output()
        .map_err(|error| format!("rg could not be executed: {}", error.kind()))?;
    if !output.status.success() {
        return Err(match output.status.code() {
            Some(code) => format!("rg --version exited with status {code}"),
            None => "rg --version did not complete".to_owned(),
        });
    }
    parse_version(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "rg did not report a recognizable version".to_owned())
}

fn parse_version(text: &str) -> Option<String> {
    let version = text
        .lines()
        .next()?
        .strip_prefix("ripgrep ")?
        .split_whitespace()
        .next()?;
    if !version.starts_with(|character: char| character.is_ascii_digit())
        || !version
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
    {
        return None;
    }
    Some(version.to_owned())
}

fn major_version(version: &str) -> u64 {
    version
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(0)
}

/// Find the first executable `rg` on `PATH`, returned as an absolute path.
fn find_on_path(path: &OsStr) -> Option<PathBuf> {
    let current_dir = std::env::current_dir().ok();
    for directory in std::env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            directory
        };
        let candidate = directory.join(RG_BINARY);
        if !is_executable(&candidate) {
            continue;
        }
        let absolute = if candidate.is_absolute() {
            candidate
        } else {
            current_dir.as_ref()?.join(candidate)
        };
        return Some(absolute);
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests;
