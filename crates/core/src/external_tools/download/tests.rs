//! Download tests against a local HTTPS fixture.
//!
//! Production builds never reach these values: the fixture injects its own
//! URL, SHA-256, allowed host and trusted root certificate through the private
//! [`DownloadSource`]. The embedded manifest stays authoritative for the real
//! installation, which is covered by the manual evidence instead.

use super::*;
use crate::config::{Config, ConfigSource};
use crate::external_tools::manifest::{RIPGREP_VERSION, RipgrepAsset};
use fixture::{Entry, Handler, Reply, start, tar_gz};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod fixture;

const TEST_TARGET: &str = "test-target";

/// Executable file name the extractor looks for; `rg.exe` on Windows.
const RG_BINARY: &str = super::super::status::RG_BINARY;

/// Archive entry path of the fixture executable (platform file name).
fn rg_entry(directory: &str) -> String {
    format!("{directory}/{RG_BINARY}")
}

fn digest(bytes: &[u8]) -> String {
    super::hex_digest(&Sha256::digest(bytes))
}

fn source(fixture: &fixture::Fixture, path: &str, archive: &[u8]) -> DownloadSource {
    DownloadSource {
        url: format!("https://{}{path}", fixture.address),
        sha256: digest(archive),
        binary_sha256: digest(b"unused executable"),
        allowed_hosts: vec!["127.0.0.1".to_owned()],
        extra_roots: vec![fixture.ca_pem.clone()],
        proxy: None,
        no_proxy: None,
    }
}

fn destination(directory: &Path) -> PathBuf {
    directory
        .join("data")
        .join("tars")
        .join("tools")
        .join(format!("ripgrep-{RIPGREP_VERSION}-{TEST_TARGET}"))
        .join(RG_BINARY)
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" | ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn entries_remaining(destination: &Path) -> usize {
    match std::fs::read_dir(destination.parent().unwrap()) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

fn config_with_rg(value: &str) -> Config {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        format!("version = 1\n[external_tools]\nrg = \"{value}\"\n"),
    )
    .unwrap();
    Config::load(ConfigSource::File(path)).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn success_installs_atomically_and_runs_version() {
    use std::os::unix::fs::PermissionsExt;

    let script = b"#!/bin/sh\necho \"ripgrep 15.2.0\"\n";
    let archive = tar_gz(&[
        Entry::Dir("ripgrep-15.2.0-test-target/"),
        Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), script),
        Entry::File("ripgrep-15.2.0-test-target/README.md", b"ignored"),
    ]);
    let served = archive.clone();
    let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut install_source = source(&fixture, "/rg.tar.gz", &archive);
    install_source.binary_sha256 = digest(script);

    let version = install(&install_source, &destination).await.unwrap();
    assert_eq!(version, "15.2.0");
    assert!(destination.is_file());
    let mode = std::fs::metadata(&destination)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "the installed executable has mode 0755");
    assert_eq!(
        entries_remaining(&destination),
        1,
        "no temporary files remain beside the installed rg"
    );
    assert_eq!(fixture.requests(), 1);
    assert!(
        std::fs::read_to_string(&destination)
            .unwrap()
            .starts_with("#!/bin/sh")
    );
}

#[tokio::test]
async fn hash_mismatch_leaves_nothing_at_the_final_path() {
    let archive = tar_gz(&[Entry::File(
        &rg_entry("ripgrep-15.2.0-test-target"),
        b"not the expected executable",
    )]);
    let served = archive.clone();
    let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut source = source(&fixture, "/rg.tar.gz", &archive);
    source.sha256 = "0".repeat(64);

    let error = install(&source, &destination).await.unwrap_err();
    assert!(matches!(error, DownloadError::HashMismatch), "{error:?}");
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
}

#[tokio::test]
async fn redirect_to_a_disallowed_host_is_rejected_before_the_second_request() {
    let handler: Handler = Arc::new(|request, _| {
        if request.starts_with("GET /redirect-bad ") {
            Reply::redirect("https://not-allowed.invalid/rg.tar.gz".to_owned())
        } else {
            Reply::ok(Vec::new())
        }
    });
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let archive = tar_gz(&[Entry::File(
        &rg_entry("ripgrep-15.2.0-test-target"),
        b"unused",
    )]);

    let error = install(&source(&fixture, "/redirect-bad", &archive), &destination)
        .await
        .unwrap_err();
    let chain = error_chain(&error);
    assert!(
        chain.contains("allowed GitHub release host"),
        "unexpected error chain: {chain}"
    );
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
    assert_eq!(
        fixture.requests(),
        1,
        "the disallowed host was not contacted"
    );
}

#[tokio::test]
async fn redirect_to_an_allowed_host_is_followed() {
    let handler: Handler = Arc::new(|request, address| {
        if request.starts_with("GET /redirect-ok ") {
            Reply::redirect(format!("https://{address}/rg.tar.gz"))
        } else {
            Reply::ok(b"served after the redirect".to_vec())
        }
    });
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let archive = tar_gz(&[Entry::File(
        &rg_entry("ripgrep-15.2.0-test-target"),
        b"expected bytes",
    )]);
    let mut source = source(&fixture, "/redirect-ok", &archive);
    source.sha256 = digest(b"different bytes");

    let error = install(&source, &destination).await.unwrap_err();
    assert!(
        matches!(error, DownloadError::HashMismatch),
        "the redirected body was downloaded: {error:?}"
    );
    assert!(!destination.exists());
    assert_eq!(fixture.requests(), 2, "the allowed redirect was followed");
}

#[tokio::test]
async fn archives_with_escaping_paths_are_rejected() {
    for archive in [
        tar_gz(&[
            Entry::File("ripgrep-15.2.0-test-target/../escape", b"escape"),
            Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), b"unused"),
        ]),
        tar_gz(&[
            Entry::File("/tmp/tars-escape", b"escape"),
            Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), b"unused"),
        ]),
    ] {
        let served = archive.clone();
        let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
        let fixture = start(handler).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = destination(directory.path());
        let error = install(&source(&fixture, "/rg.tar.gz", &archive), &destination)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DownloadError::ArchivePath),
            "unexpected error: {error:?}"
        );
        assert!(!destination.exists());
        assert_eq!(entries_remaining(&destination), 0);
    }
}

#[tokio::test]
async fn archives_with_link_entries_are_rejected() {
    for archive in [
        tar_gz(&[
            Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), b"unused"),
            Entry::Symlink("ripgrep-15.2.0-test-target/rg-link", "/etc/passwd"),
        ]),
        tar_gz(&[
            Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), b"unused"),
            Entry::HardLink("ripgrep-15.2.0-test-target/rg-hard", "rg"),
        ]),
    ] {
        let served = archive.clone();
        let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
        let fixture = start(handler).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = destination(directory.path());
        let error = install(&source(&fixture, "/rg.tar.gz", &archive), &destination)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DownloadError::ArchiveLink),
            "unexpected error: {error:?}"
        );
        assert!(!destination.exists());
        assert_eq!(entries_remaining(&destination), 0);
    }
}

#[tokio::test]
async fn archives_missing_or_duplicating_the_executable_are_rejected() {
    for (archive, expected_missing) in [
        (
            tar_gz(&[Entry::File(
                "ripgrep-15.2.0-test-target/README.md",
                b"no executable",
            )]),
            true,
        ),
        (
            tar_gz(&[
                Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), b"first"),
                Entry::File(&rg_entry("ripgrep-15.2.0-test-target/second"), b"second"),
            ]),
            false,
        ),
    ] {
        let served = archive.clone();
        let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
        let fixture = start(handler).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = destination(directory.path());
        let error = install(&source(&fixture, "/rg.tar.gz", &archive), &destination)
            .await
            .unwrap_err();
        if expected_missing {
            assert!(
                matches!(error, DownloadError::ArchiveMissingRg),
                "unexpected error: {error:?}"
            );
        } else {
            assert!(
                matches!(error, DownloadError::ArchiveDuplicate),
                "unexpected error: {error:?}"
            );
        }
        assert!(!destination.exists());
        assert_eq!(entries_remaining(&destination), 0);
    }
}

#[tokio::test]
async fn oversize_content_length_is_rejected_before_reading_the_body() {
    let handler: Handler = Arc::new(|_, _| Reply {
        status: "200 OK",
        headers: Vec::new(),
        content_length: Some(MAX_ARCHIVE_BYTES + 1),
        body: Vec::new(),
    });
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut source = source(&fixture, "/rg.tar.gz", &[]);
    source.sha256 = digest(b"unreachable");
    let error = install(&source, &destination).await.unwrap_err();
    assert!(matches!(error, DownloadError::TooLarge), "{error:?}");
    assert!(!destination.exists());
}

#[tokio::test]
async fn streaming_body_over_the_cap_is_rejected() {
    let handler: Handler = Arc::new(|_, _| Reply {
        status: "200 OK",
        headers: Vec::new(),
        content_length: None,
        body: vec![b'x'; usize::try_from(MAX_ARCHIVE_BYTES + 1).unwrap()],
    });
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut source = source(&fixture, "/rg.tar.gz", &[]);
    source.sha256 = digest(b"unreachable");
    let error = install(&source, &destination).await.unwrap_err();
    assert!(matches!(error, DownloadError::TooLarge), "{error:?}");
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
}

#[tokio::test]
async fn interrupted_download_leaves_nothing_at_the_final_path() {
    let archive = tar_gz(&[Entry::File(
        &rg_entry("ripgrep-15.2.0-test-target"),
        b"truncated before the end",
    )]);
    let partial = archive[..archive.len() / 2].to_vec();
    let full_length = u64::try_from(archive.len()).unwrap();
    let handler: Handler = Arc::new(move |_, _| Reply {
        status: "200 OK",
        headers: Vec::new(),
        content_length: Some(full_length),
        body: partial.clone(),
    });
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let error = install(&source(&fixture, "/rg.tar.gz", &archive), &destination)
        .await
        .unwrap_err();
    assert!(
        matches!(error, DownloadError::Network(_)),
        "unexpected error: {error:?}"
    );
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
}

#[tokio::test]
async fn non_https_urls_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let source = DownloadSource {
        url: "http://127.0.0.1:9/rg.tar.gz".to_owned(),
        sha256: "0".repeat(64),
        binary_sha256: "0".repeat(64),
        allowed_hosts: vec!["127.0.0.1".to_owned()],
        extra_roots: Vec::new(),
        proxy: None,
        no_proxy: None,
    };
    let error = install(&source, &destination).await.unwrap_err();
    assert!(matches!(error, DownloadError::Url), "{error:?}");
    assert!(!destination.exists());
}

#[tokio::test]
async fn an_executable_that_does_not_match_its_pinned_hash_is_rejected() {
    let archive = tar_gz(&[Entry::File(
        &rg_entry("ripgrep-15.2.0-test-target"),
        b"archive is genuine but the executable pin is wrong",
    )]);
    let served = archive.clone();
    let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut install_source = source(&fixture, "/rg.tar.gz", &archive);
    install_source.binary_sha256 = "0".repeat(64);
    let error = install(&install_source, &destination).await.unwrap_err();
    assert!(
        matches!(error, DownloadError::BinaryHashMismatch),
        "unexpected error: {error:?}"
    );
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn an_installed_executable_that_cannot_run_is_removed_again() {
    let script = b"#!/bin/sh\nexit 7\n";
    let archive = tar_gz(&[Entry::File(&rg_entry("ripgrep-15.2.0-test-target"), script)]);
    let served = archive.clone();
    let handler: Handler = Arc::new(move |_, _| Reply::ok(served.clone()));
    let fixture = start(handler).await;
    let directory = tempfile::tempdir().unwrap();
    let destination = destination(directory.path());
    let mut install_source = source(&fixture, "/rg.tar.gz", &archive);
    install_source.binary_sha256 = digest(script);
    let error = install(&install_source, &destination).await.unwrap_err();
    assert!(
        matches!(error, DownloadError::Version(ref reason) if reason.contains("status 7")),
        "unexpected error: {error:?}"
    );
    assert!(!destination.exists());
    assert_eq!(entries_remaining(&destination), 0);
}

#[test]
fn download_source_uses_the_embedded_trust_anchors_and_proxy_environment() {
    let asset = RipgrepAsset {
        target: "test-target",
        url: "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/rg.tar.gz",
        sha256: "3750b2e93f37e0c692657da574d7019a101c0084da05a790c83fd335bad973e4",
        binary_sha256: "a326a1fb48074202e9ad41e4cd1e389eeea372c8c6f7d7e80da81176d5d9430e",
    };
    let environment = ToolEnvironment {
        proxy: Some("http://proxy.example:8080".to_owned()),
        no_proxy: Some("localhost,127.0.0.1".to_owned()),
        ..ToolEnvironment::default()
    };
    let source = DownloadSource::from_asset(&asset, &environment);
    assert_eq!(source.url, asset.url);
    assert_eq!(source.sha256, asset.sha256);
    assert_eq!(source.binary_sha256, asset.binary_sha256);
    assert!(source.extra_roots.is_empty());
    assert_eq!(source.proxy.as_deref(), Some("http://proxy.example:8080"));
    assert_eq!(source.no_proxy.as_deref(), Some("localhost,127.0.0.1"));
    for allowed in [
        "github.com",
        "release-assets.githubusercontent.com",
        "objects.githubusercontent.com",
    ] {
        assert!(host_allowed(allowed, &source.allowed_hosts), "{allowed}");
    }
    for rejected in [
        "example.com",
        "github.com.evil.example",
        "notgithubusercontent.com",
        "githubusercontent.com.evil.example",
    ] {
        assert!(!host_allowed(rejected, &source.allowed_hosts), "{rejected}");
    }
}

#[tokio::test]
async fn install_refuses_a_configured_non_managed_source_without_networking() {
    let config = config_with_rg("system");
    let error = install_managed_rg_with(&config, &ToolEnvironment::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, InstallError::NotManaged(ref source) if source == "system"),
        "unexpected error: {error:?}"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn install_refuses_to_guess_a_data_directory() {
    let config = Config::load(ConfigSource::Disabled).unwrap();
    let error = install_managed_rg_with(&config, &ToolEnvironment::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, InstallError::NoDataDirectory),
        "unexpected error: {error:?}"
    );
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn install_reports_a_missing_platform_archive() {
    let config = Config::load(ConfigSource::Disabled).unwrap();
    let environment = ToolEnvironment {
        data_home: Some(std::env::temp_dir()),
        ..ToolEnvironment::default()
    };
    let error = install_managed_rg_with(&config, &environment)
        .await
        .unwrap_err();
    assert!(
        matches!(error, InstallError::UnsupportedPlatform),
        "unexpected error: {error:?}"
    );
}
