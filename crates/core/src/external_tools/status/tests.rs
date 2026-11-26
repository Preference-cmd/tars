use super::*;
use crate::config::{Config, ConfigSource};
use crate::external_tools::manifest::ripgrep_asset;
use std::path::Path;

fn config_with(ripgrep: Option<&str>) -> Config {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut text = String::from("version = 1\n");
    if let Some(value) = ripgrep {
        text.push_str("[external_tools]\n");
        text.push_str(&format!("rg = \"{value}\"\n"));
    }
    std::fs::write(&path, text).unwrap();
    Config::load(ConfigSource::File(path)).unwrap()
}

#[cfg_attr(not(unix), allow(dead_code))]
fn tool_environment(tars_home: &Path, path: &Path) -> ToolEnvironment {
    ToolEnvironment {
        tars_home: Some(tars_home.to_owned()),
        home: None,
        path: Some(path.as_os_str().to_owned()),
        proxy: None,
        no_proxy: None,
    }
}

#[test]
fn tool_source_parses_all_configured_forms() {
    assert_eq!(ToolSource::parse("auto"), Ok(ToolSource::Auto));
    assert_eq!(ToolSource::parse(" auto "), Ok(ToolSource::Auto));
    assert_eq!(ToolSource::parse("managed"), Ok(ToolSource::Managed));
    assert_eq!(ToolSource::parse(" system "), Ok(ToolSource::System));
    // A platform-absolute path; a Unix literal such as `/opt/tools/rg` is not
    // absolute on Windows. Forward slashes keep the same text on both systems.
    let absolute = std::env::temp_dir().join("tars-tool-source-rg");
    let absolute_text = absolute.to_string_lossy().replace('\\', "/");
    let absolute_path = PathBuf::from(&absolute_text);
    assert_eq!(
        ToolSource::parse(&absolute_text),
        Ok(ToolSource::Path(absolute_path.clone()))
    );
    for invalid in ["", "Auto", "Managed", "rg", "./rg", "~/bin/rg"] {
        assert_eq!(
            ToolSource::parse(invalid),
            Err(ToolSourceError),
            "{invalid}"
        );
    }
    assert_eq!(ToolSource::Auto.kind(), "auto");
    assert_eq!(ToolSource::Auto.to_string(), "auto");
    assert_eq!(ToolSource::Managed.kind(), "managed");
    assert_eq!(ToolSource::System.kind(), "system");
    assert_eq!(ToolSource::Path(absolute_path.clone()).kind(), "path");
    assert_eq!(ToolSource::Path(absolute_path).to_string(), absolute_text);
}

#[test]
fn managed_path_uses_an_absolute_tars_home_or_the_home_fallback() {
    // Build the expected paths natively: `/tars` is not absolute on Windows,
    // `$HOME`-style paths are not portable, and the executable name is
    // `rg.exe` there (RG_BINARY).
    let tars_home = std::env::temp_dir().join("tars-managed-home-override");
    let home = std::env::temp_dir().join("tars-managed-home");
    let environment = ToolEnvironment {
        tars_home: Some(tars_home.clone()),
        home: Some(home.clone()),
        path: None,
        proxy: None,
        no_proxy: None,
    };
    let expected = tars_home
        .join("tools")
        .join(format!("ripgrep-{RIPGREP_VERSION}-aarch64-apple-darwin"))
        .join(RG_BINARY);
    assert_eq!(
        managed_path(&environment, "aarch64-apple-darwin").unwrap(),
        expected
    );

    for ignored in [PathBuf::from("relative-is-ignored"), PathBuf::new()] {
        let fallback = ToolEnvironment {
            tars_home: Some(ignored),
            home: Some(home.clone()),
            path: None,
            proxy: None,
            no_proxy: None,
        };
        assert_eq!(
            managed_path(&fallback, "x86_64-apple-darwin").unwrap(),
            home.join(".tars")
                .join("tools")
                .join(format!("ripgrep-{RIPGREP_VERSION}-x86_64-apple-darwin"))
                .join(RG_BINARY)
        );
    }

    let nowhere = ToolEnvironment::default();
    assert!(managed_path(&nowhere, "aarch64-apple-darwin").is_none());
    assert!(managed_path(&nowhere, "x86_64-apple-darwin").is_none());

    let empty_home = ToolEnvironment {
        tars_home: Some(PathBuf::from("relative-is-ignored")),
        home: Some(PathBuf::new()),
        ..ToolEnvironment::default()
    };
    assert!(managed_path(&empty_home, "aarch64-apple-darwin").is_none());

    let relative_home = ToolEnvironment {
        tars_home: None,
        home: Some(PathBuf::from(".")),
        ..ToolEnvironment::default()
    };
    assert!(managed_path(&relative_home, "aarch64-apple-darwin").is_none());

    let missing_dir_status = resolve_managed(
        &nowhere,
        Some(&ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: "0".repeat(64),
        }),
    );
    assert!(!missing_dir_status.is_ready());
    assert_eq!(
        missing_dir_status.unavailable_reason(),
        Some("cannot locate the user TARS directory; set TARS_HOME or HOME")
    );
}

#[test]
fn embedded_expected_asset_matches_the_manifest_for_this_build() {
    assert_eq!(
        ExpectedAsset::embedded().map(|asset| asset.target),
        ripgrep_asset().map(|asset| asset.target.to_owned())
    );
    assert_eq!(
        ExpectedAsset::embedded().map(|asset| asset.binary_sha256),
        ripgrep_asset().map(|asset| asset.binary_sha256.to_owned())
    );
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(directory: &Path, name: &str, version: &str, marker: Option<&Path>) -> PathBuf {
        let path = directory.join(name);
        let body = match marker {
            Some(marker) => format!(
                "#!/bin/sh\nprintf ran > '{}'\necho \"ripgrep {version}\"\n",
                marker.display()
            ),
            None => format!("#!/bin/sh\necho \"ripgrep {version}\"\n"),
        };
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn hash(path: &Path) -> String {
        crate::external_tools::download::sha256_file(path).unwrap()
    }

    #[test]
    fn managed_check_hashes_before_execution_and_reports_a_fixture_version() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("executed");
        let environment =
            tool_environment(&directory.path().join("data"), Path::new("/nonexistent"));
        let path = managed_path(&environment, "test-target").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source_dir = directory.path().join("fixture");
        std::fs::create_dir(&source_dir).unwrap();
        let script = script(&source_dir, "rg", "15.2.0", Some(&marker));
        std::fs::copy(&script, &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let expected = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: hash(&path),
        };

        let wrong = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: "0".repeat(64),
        };
        let status = resolve_managed(&environment, Some(&wrong));
        assert!(!status.is_ready());
        assert!(status.unavailable_reason().unwrap().contains("SHA-256"));
        assert_eq!(status.path.as_deref(), Some(path.as_path()));
        assert!(status.version.is_none());
        assert!(status.ready_path().is_none());
        assert!(
            !marker.exists(),
            "a hash mismatch must never execute the file"
        );

        let status = resolve_managed(&environment, Some(&expected));
        assert!(status.is_ready());
        assert_eq!(status.version.as_deref(), Some("15.2.0"));
        assert_eq!(status.ready_path(), Some(path.as_path()));
        assert_eq!(status.source, ToolSource::Managed);
        assert!(marker.exists(), "the verified fixture is executed once");

        let missing = resolve_managed(
            &ToolEnvironment {
                tars_home: Some(directory.path().join("empty")),
                ..ToolEnvironment::default()
            },
            Some(&expected),
        );
        assert!(!missing.is_ready());
        assert!(
            missing
                .unavailable_reason()
                .unwrap()
                .contains("not installed")
        );
        assert!(missing.ready_path().is_none());
    }

    #[test]
    fn managed_check_without_a_platform_entry_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let status = resolve_managed(
            &tool_environment(directory.path(), Path::new("/nonexistent")),
            None,
        );
        assert!(!status.is_ready());
        assert!(status.path.is_none());
        assert!(
            status
                .unavailable_reason()
                .unwrap()
                .contains("no managed ripgrep")
        );
    }

    #[test]
    fn system_lookup_resolves_once_records_version_and_rejects_old_releases() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = directory.path().join("runs");
        script(&bin, "rg", "15.2.0", Some(&marker));
        let environment = tool_environment(directory.path(), &bin);
        let status = resolve_source(&ToolSource::System, &environment);
        assert!(status.is_ready(), "{:?}", status.state);
        assert_eq!(status.version.as_deref(), Some("15.2.0"));
        assert_eq!(status.path, Some(bin.join("rg")));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");
        // Reading a resolved status never re-runs the executable.
        assert!(status.is_ready());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ran");

        let old = directory.path().join("old");
        std::fs::create_dir(&old).unwrap();
        script(&old, "rg", "12.1.1", None);
        let status = resolve_source(
            &ToolSource::System,
            &tool_environment(directory.path(), &old),
        );
        assert!(!status.is_ready());
        assert_eq!(status.version.as_deref(), Some("12.1.1"));
        assert!(status.unavailable_reason().unwrap().contains("older"));
        assert!(status.ready_path().is_none());

        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let status = resolve_source(
            &ToolSource::System,
            &tool_environment(directory.path(), &empty),
        );
        assert!(!status.is_ready());
        assert!(status.path.is_none());
        assert!(
            status
                .unavailable_reason()
                .unwrap()
                .contains("not found on PATH")
        );

        let status = resolve_source(
            &ToolSource::System,
            &ToolEnvironment {
                path: None,
                ..ToolEnvironment::default()
            },
        );
        assert!(!status.is_ready());
        assert_eq!(status.unavailable_reason(), Some("PATH is not set"));
    }

    #[test]
    fn configured_absolute_path_must_exist_and_records_its_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = script(directory.path(), "custom-rg", "15.3.1", None);
        let status = resolve_source(&ToolSource::Path(path.clone()), &ToolEnvironment::default());
        assert!(status.is_ready());
        assert_eq!(status.version.as_deref(), Some("15.3.1"));
        assert_eq!(status.ready_path(), Some(path.as_path()));
        assert_eq!(status.source, ToolSource::Path(path.clone()));

        let missing = directory.path().join("absent");
        let status = resolve_source(&ToolSource::Path(missing), &ToolEnvironment::default());
        assert!(!status.is_ready());
        assert!(
            status
                .unavailable_reason()
                .unwrap()
                .contains("does not exist")
        );

        let relative = PathBuf::from("rg");
        let status = resolve_source(&ToolSource::Path(relative), &ToolEnvironment::default());
        assert!(!status.is_ready());
        assert!(status.unavailable_reason().unwrap().contains("absolute"));
    }

    #[test]
    fn broken_executables_are_unavailable_without_echoing_their_output() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rg");
        std::fs::write(&path, "#!/bin/sh\necho 'secret-output'\nexit 3\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let status = resolve_source(&ToolSource::Path(path), &ToolEnvironment::default());
        assert!(!status.is_ready());
        assert!(status.version.is_none());
        assert!(
            !status
                .unavailable_reason()
                .unwrap()
                .contains("secret-output")
        );
    }

    #[test]
    fn system_lookup_skips_relative_and_empty_path_entries() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = directory.path().join("ran");
        script(&bin, "rg", "15.2.0", Some(&marker));
        // The same directory written relative to the current directory.
        let current = std::env::current_dir().unwrap();
        let depth = current.components().count() - 1;
        let relative = PathBuf::from("../".repeat(depth))
            .join(bin.canonicalize().unwrap().strip_prefix("/").unwrap());
        assert!(relative.join("rg").is_file(), "{}", relative.display());
        for entries in [
            format!("{}", relative.display()),
            format!(":{}", relative.display()),
        ] {
            let status = resolve_source(
                &ToolSource::System,
                &tool_environment(directory.path(), Path::new(&entries)),
            );
            assert!(!status.is_ready(), "{entries}");
            assert!(status.path.is_none(), "{entries}");
        }
        assert!(!marker.exists(), "a relative PATH entry must never run");

        let status = resolve_source(
            &ToolSource::System,
            &tool_environment(directory.path(), &bin),
        );
        assert!(status.is_ready(), "{:?}", status.state);
        assert!(marker.exists());
    }

    #[test]
    fn auto_prefers_valid_managed_copy_over_system_path() {
        let directory = tempfile::tempdir().unwrap();
        let tars_home = directory.path().join("data");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path_marker = directory.path().join("path-ran");
        script(&bin, "rg", "14.1.0", Some(&path_marker));

        let environment = tool_environment(&tars_home, &bin);
        let managed = managed_path(&environment, "test-target").unwrap();
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let managed_marker = directory.path().join("managed-ran");
        let fixture = script(&src_dir, "rg", "15.2.0", Some(&managed_marker));
        std::fs::copy(&fixture, &managed).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let expected = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: hash(&managed),
        };

        let status = resolve_auto(&environment, Some(&expected));
        assert!(status.is_ready(), "{:?}", status.state);
        assert_eq!(status.source, ToolSource::Auto);
        assert_eq!(status.ready_path(), Some(managed.as_path()));
        assert_eq!(status.version.as_deref(), Some("15.2.0"));
        assert!(managed_marker.exists());
        assert!(!path_marker.exists(), "valid managed must win over PATH");
    }

    #[test]
    fn auto_fails_closed_when_managed_copy_is_corrupted_even_with_valid_path() {
        let directory = tempfile::tempdir().unwrap();
        let tars_home = directory.path().join("data");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let path_marker = directory.path().join("path-ran");
        script(&bin, "rg", "15.2.0", Some(&path_marker));

        let environment = tool_environment(&tars_home, &bin);
        let managed = managed_path(&environment, "test-target").unwrap();
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        let src_dir = directory.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        let managed_marker = directory.path().join("managed-ran");
        let fixture = script(&src_dir, "rg", "15.2.0", Some(&managed_marker));
        std::fs::copy(&fixture, &managed).unwrap();
        std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wrong = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: "0".repeat(64),
        };

        let status = resolve_auto(&environment, Some(&wrong));
        assert!(!status.is_ready());
        assert_eq!(status.source, ToolSource::Auto);
        assert_eq!(status.path.as_deref(), Some(managed.as_path()));
        assert!(status.ready_path().is_none());
        assert!(
            status.unavailable_reason().unwrap().contains("SHA-256"),
            "{:?}",
            status.state
        );
        assert!(!managed_marker.exists());
        assert!(
            !path_marker.exists(),
            "corrupted managed copy must fail closed without falling back to PATH"
        );

        // A broken symlink at the managed path also exists via symlink_metadata
        // and must fail closed instead of falling back to PATH.
        std::fs::remove_file(&managed).unwrap();
        std::os::unix::fs::symlink(directory.path().join("missing-target"), &managed).unwrap();
        let symlink_status = resolve_auto(&environment, Some(&wrong));
        assert!(!symlink_status.is_ready());
        assert_eq!(symlink_status.source, ToolSource::Auto);
        assert_eq!(symlink_status.path.as_deref(), Some(managed.as_path()));
        assert!(!path_marker.exists());
    }

    #[test]
    fn auto_falls_back_to_system_path_when_managed_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let tars_home = directory.path().join("data");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        script(&bin, "rg", "14.1.0", None);
        let environment = tool_environment(&tars_home, &bin);
        let expected = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: "0".repeat(64),
        };

        let status = resolve_auto(&environment, Some(&expected));
        assert!(status.is_ready(), "{:?}", status.state);
        assert_eq!(status.source, ToolSource::Auto);
        assert_eq!(status.ready_path(), Some(bin.join("rg").as_path()));
        assert_eq!(status.version.as_deref(), Some("14.1.0"));

        // Works even when no managed release exists for the platform (`expected == None`).
        let status_no_asset = resolve_auto(&environment, None);
        assert!(status_no_asset.is_ready(), "{:?}", status_no_asset.state);
        assert_eq!(status_no_asset.source, ToolSource::Auto);
        assert_eq!(status_no_asset.ready_path(), Some(bin.join("rg").as_path()));
    }

    #[test]
    fn auto_rejects_old_system_rg_and_reports_unavailable_when_missing() {
        let directory = tempfile::tempdir().unwrap();
        let tars_home = directory.path().join("data");
        let old = directory.path().join("old");
        std::fs::create_dir(&old).unwrap();
        script(&old, "rg", "13.0.0", None);
        let expected = ExpectedAsset {
            target: "test-target".into(),
            binary_sha256: "0".repeat(64),
        };

        let status = resolve_auto(&tool_environment(&tars_home, &old), Some(&expected));
        assert!(!status.is_ready());
        assert_eq!(status.source, ToolSource::Auto);
        assert_eq!(status.version.as_deref(), Some("13.0.0"));
        assert!(status.ready_path().is_none());
        let reason = status.unavailable_reason().unwrap();
        assert!(reason.contains("older than 14"), "{reason}");
        assert!(reason.contains("tars tools install rg"), "{reason}");

        let status_no_asset = resolve_auto(&tool_environment(&tars_home, &old), None);
        assert!(!status_no_asset.is_ready());
        let reason_no_asset = status_no_asset.unavailable_reason().unwrap();
        assert!(
            reason_no_asset.contains("older than 14"),
            "{reason_no_asset}"
        );
        assert!(
            !reason_no_asset.contains("tars tools install rg"),
            "{reason_no_asset}"
        );

        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let env_empty = tool_environment(&tars_home, &empty);
        let expected_managed = managed_path(&env_empty, "test-target").unwrap();
        let status_missing = resolve_auto(&env_empty, Some(&expected));
        assert!(!status_missing.is_ready());
        assert_eq!(status_missing.source, ToolSource::Auto);
        assert_eq!(
            status_missing.path.as_deref(),
            Some(expected_managed.as_path())
        );
        assert!(
            status_missing
                .unavailable_reason()
                .unwrap()
                .contains("tars tools install rg")
        );

        let status_missing_no_asset = resolve_auto(&env_empty, None);
        assert!(!status_missing_no_asset.is_ready());
        assert!(status_missing_no_asset.path.is_none());
        assert!(
            status_missing_no_asset
                .unavailable_reason()
                .unwrap()
                .contains("no managed ripgrep")
        );
    }
}

#[test]
fn configured_sources_resolve_through_the_shared_configuration() {
    // A missing platform archive still resolves to a bounded reason and never
    // panics; this holds on every platform.
    let config = config_with(None);
    let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
    assert_eq!(tools.rg().source, ToolSource::Auto);
    assert_eq!(tools.rg().name, "rg");
    assert!(!tools.rg().is_ready());

    // A configured path must be absolute; build it from the real temporary
    // directory so the value is valid on Windows too.
    let absent = std::env::temp_dir().join("tars-absent-rg");
    let absent_text = absent.to_string_lossy().replace('\\', "/");
    let config = config_with(Some(&absent_text));
    let tools = ExternalTools::resolve_with(
        &config,
        &ToolEnvironment {
            path: None,
            ..ToolEnvironment::default()
        },
    );
    assert_eq!(
        tools.rg().source,
        ToolSource::Path(PathBuf::from(&absent_text))
    );
    assert!(!tools.rg().is_ready());
}

#[test]
fn process_cache_returns_the_same_resolution() {
    let first = resolve_process(&Config::load(ConfigSource::Disabled).unwrap());
    let second = resolve_process(&Config::load(ConfigSource::Disabled).unwrap());
    assert!(std::ptr::eq(first, second));
    assert_eq!(first.rg().source, ToolSource::Auto);
}

#[test]
fn version_probe_accepts_release_and_rejects_non_ripgrep_output() {
    assert_eq!(parse_version("ripgrep 15.2.0\n").as_deref(), Some("15.2.0"));
    assert_eq!(
        parse_version("ripgrep 15.2.0 (rev abc123)\nfeatures: foo\n").as_deref(),
        Some("15.2.0")
    );
    assert_eq!(parse_version("grep 15.2.0\n"), None);
    assert_eq!(parse_version("ripgrep not-a-version\n"), None);
    assert_eq!(parse_version("ripgrep\n"), None);
    assert_eq!(
        parse_version("ripgrep 14.1.0-1\n").as_deref(),
        Some("14.1.0")
    );
    assert_eq!(
        parse_version("ripgrep 14.1.1+deb\n").as_deref(),
        Some("14.1.1")
    );
    assert_eq!(parse_version("ripgrep 15.0.0x\n"), None);
    assert_eq!(parse_version("ripgrep -1\n"), None);
    assert_eq!(major_version("15.2.0"), 15);
    assert_eq!(major_version("14"), 14);
    assert_eq!(major_version("not"), 0);
}

#[test]
fn etxtbsy_retry_recovers_from_transient_executable_file_busy() {
    let mut attempts = 0;
    let result = run_with_etxtbsy_retry(|| {
        attempts += 1;
        if attempts <= 2 {
            Err(std::io::Error::new(
                std::io::ErrorKind::ExecutableFileBusy,
                "transient busy",
            ))
        } else {
            Ok("ok")
        }
    });
    assert_eq!(result.unwrap(), "ok");
    assert_eq!(attempts, 3);

    // Succeeding on the final (5th) retry also recovers.
    let mut max_retry_attempts = 0;
    let max_retry_result = run_with_etxtbsy_retry(|| {
        max_retry_attempts += 1;
        if max_retry_attempts <= ETXTBSY_RETRY_DELAYS.len() {
            Err(std::io::Error::new(
                std::io::ErrorKind::ExecutableFileBusy,
                "transient busy",
            ))
        } else {
            Ok("recovered-on-last-retry")
        }
    });
    assert_eq!(max_retry_result.unwrap(), "recovered-on-last-retry");
    assert_eq!(max_retry_attempts, 1 + ETXTBSY_RETRY_DELAYS.len());
}

#[test]
fn etxtbsy_retry_stops_after_bounded_limit_on_persistent_busy() {
    let mut attempts = 0;
    let error = run_with_etxtbsy_retry::<(), _>(|| {
        attempts += 1;
        Err(std::io::Error::new(
            std::io::ErrorKind::ExecutableFileBusy,
            "still busy",
        ))
    })
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::ExecutableFileBusy);
    assert_eq!(attempts, 1 + ETXTBSY_RETRY_DELAYS.len());
}

#[test]
fn etxtbsy_retry_does_not_retry_other_io_errors() {
    for kind in [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied,
    ] {
        let mut attempts = 0;
        let error = run_with_etxtbsy_retry::<(), _>(|| {
            attempts += 1;
            Err(std::io::Error::new(kind, "non-transient"))
        })
        .unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(attempts, 1);
    }
}
