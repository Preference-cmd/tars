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
fn tool_environment(data_home: &Path, path: &Path) -> ToolEnvironment {
    ToolEnvironment {
        data_home: Some(data_home.to_owned()),
        home: None,
        path: Some(path.as_os_str().to_owned()),
        proxy: None,
        no_proxy: None,
    }
}

#[test]
fn tool_source_parses_the_three_configured_forms() {
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
    for invalid in ["", "Managed", "rg", "./rg", "~/bin/rg"] {
        assert_eq!(
            ToolSource::parse(invalid),
            Err(ToolSourceError),
            "{invalid}"
        );
    }
    assert_eq!(ToolSource::Managed.kind(), "managed");
    assert_eq!(ToolSource::System.kind(), "system");
    assert_eq!(ToolSource::Path(absolute_path.clone()).kind(), "path");
    assert_eq!(ToolSource::Path(absolute_path).to_string(), absolute_text);
}

#[test]
fn managed_path_uses_an_absolute_xdg_data_home_or_the_home_fallback() {
    // Build the expected paths natively: `/data` is not absolute on Windows,
    // `$HOME`-style paths are not portable, and the executable name is
    // `rg.exe` there (RG_BINARY).
    let xdg = std::env::temp_dir().join("tars-managed-xdg");
    let home = std::env::temp_dir().join("tars-managed-home");
    let environment = ToolEnvironment {
        data_home: Some(xdg.clone()),
        home: Some(home.clone()),
        path: None,
        proxy: None,
        no_proxy: None,
    };
    assert_eq!(
        managed_path(&environment, "aarch64-apple-darwin").unwrap(),
        xdg.join("tars")
            .join("tools")
            .join(format!("ripgrep-{RIPGREP_VERSION}-aarch64-apple-darwin"))
            .join(RG_BINARY)
    );

    let fallback = ToolEnvironment {
        data_home: Some(PathBuf::from("relative-is-ignored")),
        home: Some(home.clone()),
        path: None,
        proxy: None,
        no_proxy: None,
    };
    assert_eq!(
        managed_path(&fallback, "x86_64-apple-darwin").unwrap(),
        home.join(".local")
            .join("share")
            .join("tars")
            .join("tools")
            .join(format!("ripgrep-{RIPGREP_VERSION}-x86_64-apple-darwin"))
            .join(RG_BINARY)
    );

    let nowhere = ToolEnvironment::default();
    assert!(managed_path(&nowhere, "aarch64-apple-darwin").is_none());
    assert!(managed_path(&nowhere, "x86_64-apple-darwin").is_none());
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
                data_home: Some(directory.path().join("empty")),
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
}

#[test]
fn configured_sources_resolve_through_the_shared_configuration() {
    // A missing platform archive still resolves to a bounded reason and never
    // panics; this holds on every platform.
    let config = config_with(None);
    let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
    assert_eq!(tools.rg().source, ToolSource::Managed);
    assert_eq!(tools.rg().name, "rg");
    if cfg!(not(any(
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64")
    ))) {
        assert!(!tools.rg().is_ready());
    }

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
    assert_eq!(first.rg().source, ToolSource::Managed);
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
