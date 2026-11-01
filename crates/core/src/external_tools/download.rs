//! Consent-gated download and installation of the managed ripgrep.
//!
//! Every network operation in TARS's external tool support lives here, and it
//! only runs after a caller has obtained the user's consent. The rules are
//! enforced in one place: HTTPS only, an allowlist of `github.com` and GitHub's
//! release asset hosts applied to the initial URL and every redirect, a size
//! cap and an overall timeout, proxy environment variables, a SHA-256 check
//! against the embedded value, extraction of only the single `rg` entry,
//! mode 0755 and an atomic rename into place.

use super::manifest::RipgrepAsset;
use super::status::{ToolEnvironment, ToolSource, ToolStatus, probe_version};
use crate::config::Config;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path};
use std::time::Duration;
use tar::Archive;

/// Largest archive accepted from the release server.
const MAX_ARCHIVE_BYTES: u64 = 10 * 1024 * 1024;

/// Largest single file accepted from the archive after decompression. The
/// pinned `rg` executable is a few MiB; the bound keeps a decompression bomb
/// from filling the disk even if the pinned archive were ever replaced.
const MAX_EXTRACTED_BYTES: u64 = 32 * 1024 * 1024;

/// Overall deadline for one download and installation.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Deadline for establishing the TLS connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum redirect hops accepted for one download.
const MAX_REDIRECTS: usize = 8;

/// Hosts a managed download may contact.
///
/// Entries are exact host names unless they start with a dot, which matches
/// that suffix and every subdomain: release downloads redirect from
/// `github.com` to a `*.githubusercontent.com` asset host.
const ALLOWED_HOST_PATTERNS: &[&str] = &["github.com", ".githubusercontent.com"];

/// Why a managed download or installation did not complete.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DownloadError {
    /// The archive URL is not an allowed HTTPS GitHub release location.
    #[error("download URL is not an allowed HTTPS GitHub release location")]
    Url,
    /// An injected root certificate or client option is invalid.
    #[error("could not prepare the HTTPS client: {0}")]
    Client(#[source] reqwest::Error),
    /// The configured proxy environment variable is not a valid proxy URL.
    #[error("invalid proxy configuration: {0}")]
    Proxy(#[source] reqwest::Error),
    /// The release server could not be reached or the transfer failed.
    #[error("download failed: {0}")]
    Network(#[source] reqwest::Error),
    /// The release server answered with an unsuccessful status.
    #[error("release server returned HTTP status {0}")]
    Status(u16),
    /// The body exceeds the accepted size.
    #[error("download exceeds the 10 MiB limit")]
    TooLarge,
    /// The archive bytes do not match the pinned SHA-256.
    #[error("archive SHA-256 does not match the pinned value")]
    HashMismatch,
    /// The extracted executable does not match its own pinned SHA-256.
    #[error("extracted rg SHA-256 does not match the pinned value")]
    BinaryHashMismatch,
    /// A local file operation failed.
    #[error("local file operation failed: {0}")]
    Io(#[source] std::io::Error),
    /// The archive could not be decoded as gzip or tar.
    #[error("archive could not be decoded: {0}")]
    Archive(#[source] std::io::Error),
    /// The archive contains an absolute or parent-relative path.
    #[error("archive contains a path that escapes the extraction directory")]
    ArchivePath,
    /// The archive contains a symbolic or hard link.
    #[error("archive contains a link entry")]
    ArchiveLink,
    /// The archive contains an entry that is neither a file nor a directory.
    #[error("archive contains an unsupported entry type")]
    ArchiveType,
    /// The archive does not contain the expected executable.
    #[error("archive does not contain the rg executable")]
    ArchiveMissingRg,
    /// The archive contains more than one `rg` executable.
    #[error("archive contains more than one rg executable")]
    ArchiveDuplicate,
    /// The installed executable did not report its version.
    #[error("installed rg could not be verified: {0}")]
    Version(String),
    /// The destination does not name a file inside a directory.
    #[error("managed tool destination is not a file path")]
    Destination,
}

/// Why a consent-gated managed installation was refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    /// The configured source is not the managed copy; nothing was downloaded.
    #[error("rg is configured as {0}; managed installation does not apply")]
    NotManaged(String),
    /// This build has no pinned archive for the host platform.
    #[error("no managed ripgrep release exists for this platform")]
    UnsupportedPlatform,
    /// The user data directory cannot be determined.
    #[error("cannot locate the user data directory; set XDG_DATA_HOME or HOME")]
    NoDataDirectory,
    /// The download or local installation failed.
    #[error(transparent)]
    Download(#[from] DownloadError),
}

/// The trust inputs for one download.
///
/// Production builds this from the embedded manifest and the process
/// environment. Unit tests inject a local HTTPS fixture's URL, hash, allowed
/// host and trusted root instead; those values never come from configuration
/// or the network.
#[derive(Clone, Debug)]
struct DownloadSource {
    url: String,
    sha256: String,
    binary_sha256: String,
    allowed_hosts: Vec<String>,
    extra_roots: Vec<Vec<u8>>,
    proxy: Option<String>,
    no_proxy: Option<String>,
}

impl DownloadSource {
    fn from_asset(asset: &RipgrepAsset, environment: &ToolEnvironment) -> Self {
        Self {
            url: asset.url.to_owned(),
            sha256: asset.sha256.to_owned(),
            binary_sha256: asset.binary_sha256.to_owned(),
            allowed_hosts: ALLOWED_HOST_PATTERNS
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            extra_roots: Vec::new(),
            proxy: environment.proxy.clone(),
            no_proxy: environment.no_proxy.clone(),
        }
    }
}

/// Download and install the managed ripgrep.
///
/// The caller must obtain explicit user consent first: this performs network
/// access. The embedded manifest supplies the URL, SHA-256 and allowed hosts;
/// the process environment supplies the data directory and proxy settings.
/// Every failure leaves nothing at the final path.
pub async fn install_managed_rg(config: &Config) -> Result<ToolStatus, InstallError> {
    install_managed_rg_with(config, &ToolEnvironment::from_process()).await
}

/// Install against an explicit environment; see [`install_managed_rg`].
///
/// The configured source must be `managed`; otherwise nothing is downloaded
/// and [`InstallError::NotManaged`] is returned.
pub async fn install_managed_rg_with(
    config: &Config,
    environment: &ToolEnvironment,
) -> Result<ToolStatus, InstallError> {
    if !matches!(config.ripgrep_source(), ToolSource::Managed) {
        return Err(InstallError::NotManaged(
            config.ripgrep_source().to_string(),
        ));
    }
    let asset = super::manifest::ripgrep_asset().ok_or(InstallError::UnsupportedPlatform)?;
    let destination =
        super::status::managed_rg_path(environment).ok_or(InstallError::NoDataDirectory)?;
    let source = DownloadSource::from_asset(asset, environment);
    let version = install(&source, &destination).await?;
    Ok(ToolStatus::ready(
        ToolSource::Managed,
        Some(destination),
        Some(version),
    ))
}

/// Run one download and installation into `destination`.
///
/// This is the whole consent-gated operation and is only reachable from the
/// public installation functions and from tests.
async fn install(source: &DownloadSource, destination: &Path) -> Result<String, DownloadError> {
    let url = reqwest::Url::parse(&source.url).map_err(|_| DownloadError::Url)?;
    let Some(host) = url.host_str() else {
        return Err(DownloadError::Url);
    };
    if url.scheme() != "https" || !host_allowed(host, &source.allowed_hosts) {
        return Err(DownloadError::Url);
    }
    let directory = destination.parent().ok_or(DownloadError::Destination)?;
    std::fs::create_dir_all(directory).map_err(DownloadError::Io)?;
    let client = build_client(source)?;

    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(DownloadError::Network)?;
    if !response.status().is_success() {
        return Err(DownloadError::Status(response.status().as_u16()));
    }
    if let Some(length) = response.content_length()
        && length > MAX_ARCHIVE_BYTES
    {
        return Err(DownloadError::TooLarge);
    }

    let mut archive = tempfile::Builder::new()
        .prefix(".rg-download-")
        .tempfile_in(directory)
        .map_err(DownloadError::Io)?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(DownloadError::Network)? {
        written = written.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if written > MAX_ARCHIVE_BYTES {
            return Err(DownloadError::TooLarge);
        }
        hasher.update(&chunk);
        std::io::Write::write_all(&mut archive, &chunk).map_err(DownloadError::Io)?;
    }
    std::io::Write::flush(&mut archive).map_err(DownloadError::Io)?;
    archive.as_file().sync_all().map_err(DownloadError::Io)?;
    let actual = hex_digest(&hasher.finalize());
    if actual != source.sha256 {
        return Err(DownloadError::HashMismatch);
    }

    let binary = extract_rg(archive.path(), directory)?;
    let extracted = sha256_file(binary.path()).map_err(DownloadError::Io)?;
    if extracted != source.binary_sha256 {
        return Err(DownloadError::BinaryHashMismatch);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(binary.path(), std::fs::Permissions::from_mode(0o755))
            .map_err(DownloadError::Io)?;
    }
    binary.as_file().sync_all().map_err(DownloadError::Io)?;
    binary
        .persist(destination)
        .map_err(|error| DownloadError::Io(error.error))?;
    match probe_version(destination) {
        Ok(version) => Ok(version),
        Err(reason) => {
            let _ = std::fs::remove_file(destination);
            Err(DownloadError::Version(reason))
        }
    }
}

fn build_client(source: &DownloadSource) -> Result<reqwest::Client, DownloadError> {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(redirect_policy(source.allowed_hosts.clone()))
        .user_agent(concat!("tars/", env!("CARGO_PKG_VERSION")));
    if let Some(proxy) = &source.proxy {
        let mut proxy = reqwest::Proxy::all(proxy).map_err(DownloadError::Proxy)?;
        if let Some(no_proxy) = &source.no_proxy {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(no_proxy));
        }
        builder = builder.proxy(proxy);
    }
    for root in &source.extra_roots {
        builder = builder.add_root_certificate(
            reqwest::Certificate::from_pem(root).map_err(DownloadError::Client)?,
        );
    }
    builder.build().map_err(DownloadError::Client)
}

fn redirect_policy(allowed_hosts: Vec<String>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        match attempt.url().host_str() {
            Some(host) if host_allowed(host, &allowed_hosts) => attempt.follow(),
            _ => attempt.error("redirect target is not an allowed GitHub release host"),
        }
    })
}

/// Check a host against exact or `.suffix` allowlist entries.
fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts
        .iter()
        .any(|pattern| match pattern.strip_prefix('.') {
            Some(suffix) => host == suffix || host.ends_with(pattern.as_str()),
            None => host == pattern,
        })
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

/// Hash a file for the managed SHA-256 check.
pub(super) fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_digest(&hasher.finalize()))
}

/// Extract the single `rg` entry from a verified archive into a temporary file
/// next to the final destination.
fn extract_rg(
    archive_path: &Path,
    directory: &Path,
) -> Result<tempfile::NamedTempFile, DownloadError> {
    let file = std::fs::File::open(archive_path).map_err(DownloadError::Io)?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let mut binary: Option<tempfile::NamedTempFile> = None;
    for entry in archive.entries().map_err(DownloadError::Archive)? {
        let mut entry = entry.map_err(DownloadError::Archive)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(DownloadError::ArchiveLink);
        }
        if !entry_type.is_file() && !entry_type.is_dir() {
            return Err(DownloadError::ArchiveType);
        }
        let path = entry.path().map_err(DownloadError::Archive)?;
        validate_entry_path(&path)?;
        if entry_type.is_dir() || path.file_name() != Some(OsStr::new(super::status::RG_BINARY)) {
            continue;
        }
        if binary.is_some() {
            return Err(DownloadError::ArchiveDuplicate);
        }
        let size = entry.header().size().map_err(DownloadError::Archive)?;
        if size > MAX_EXTRACTED_BYTES {
            return Err(DownloadError::TooLarge);
        }
        let mut file = tempfile::Builder::new()
            .prefix(".rg-install-")
            .tempfile_in(directory)
            .map_err(DownloadError::Io)?;
        let mut limited = (&mut entry).take(MAX_EXTRACTED_BYTES + 1);
        let copied = std::io::copy(&mut limited, &mut file).map_err(DownloadError::Io)?;
        if copied > MAX_EXTRACTED_BYTES {
            return Err(DownloadError::TooLarge);
        }
        binary = Some(file);
    }
    binary.ok_or(DownloadError::ArchiveMissingRg)
}

fn validate_entry_path(path: &Path) -> Result<(), DownloadError> {
    if path.as_os_str().is_empty() {
        return Err(DownloadError::ArchivePath);
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DownloadError::ArchivePath);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
