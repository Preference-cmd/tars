//! External tool commands through actual CLI processes, without live network.
//!
//! The tool commands are the only place the CLI can download, and these tests
//! pin the refusals: a non-terminal run without `--yes`, a non-managed source,
//! and every command that is not `tools install` leave the data directory
//! untouched. The successful GitHub installation is covered by manual evidence
//! in `.agents/local/external-tools-evidence.md`; tests never download.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_MCP_CONFIG")
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_DATA_HOME", home.join("data"))
        .env("HOME", home)
        .env("APPDATA", home)
        // If a command attempted a download it would fail through this
        // unreachable proxy, so a refusal cannot hide a silent network access.
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9");
    command
}

fn config_with_source(directory: &Path, rg: &str) -> PathBuf {
    let path = directory.join("config.toml");
    std::fs::write(
        &path,
        format!("version = 1\n[external_tools]\nrg = \"{rg}\"\n"),
    )
    .unwrap();
    path
}

fn empty_bin(directory: &Path) -> PathBuf {
    let bin = directory.join("empty-bin");
    std::fs::create_dir_all(&bin).unwrap();
    bin
}

fn data_directory(home: &Path) -> PathBuf {
    home.join("data")
}

#[test]
fn status_json_reports_source_path_state_and_the_sandbox_probe_without_downloading() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path())
        .args(["tools", "status", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", text(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "rg");
    assert_eq!(tools[0]["source"], "managed");
    assert_eq!(tools[0]["status"], "unavailable");
    assert_eq!(tools[0]["version"], Value::Null);
    let reason = tools[0]["reason"].as_str().unwrap();
    assert!(!reason.is_empty());
    assert!(value["sandbox"]["available"].is_boolean());
    assert!(!value["sandbox"]["detail"].as_str().unwrap().is_empty());
    if let Some(path) = tools[0]["path"].as_str() {
        // macOS builds know where the managed copy belongs even when missing.
        assert!(path.starts_with(data_directory(home.path()).to_str().unwrap()));
        assert!(path.ends_with("/rg"));
    }
    assert!(
        !data_directory(home.path()).exists(),
        "status must not create the data directory"
    );
}

#[test]
fn status_text_names_the_source_state_and_the_install_remedy() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path()).args(["tools", "status"]).output().unwrap();
    assert!(output.status.success(), "{:?}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("rg: unavailable (managed)"), "{stdout}");
    assert!(stdout.contains("  path: "), "{stdout}");
    assert!(stdout.contains("  version: -"), "{stdout}");
    assert!(stdout.contains("  reason: "), "{stdout}");
    assert!(stdout.contains("sandbox: "), "{stdout}");
    assert!(!data_directory(home.path()).exists());
}

#[test]
fn status_reflects_a_configured_system_source() {
    let home = tempfile::tempdir().unwrap();
    let config = config_with_source(home.path(), "system");
    let output = cli(home.path())
        .args(["tools", "status", "--json", "--config"])
        .arg(&config)
        .env("PATH", empty_bin(home.path()))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", text(&output.stderr));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["tools"][0]["source"], "system");
    assert_eq!(value["tools"][0]["status"], "unavailable");
    assert_eq!(value["tools"][0]["path"], Value::Null);
    assert!(
        value["tools"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not found on PATH")
    );
    assert!(!data_directory(home.path()).exists());
}

#[test]
fn install_without_a_terminal_or_yes_refuses_and_downloads_nothing() {
    let home = tempfile::tempdir().unwrap();
    let output = cli(home.path())
        .args(["tools", "install", "rg"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(!data_directory(home.path()).exists());

    // Same refusal when stdout is piped but stdin is still not a terminal.
    let output = cli(home.path())
        .args(["tools", "install", "rg"])
        .env("PATH", empty_bin(home.path()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(!data_directory(home.path()).exists());
}

#[test]
fn install_refuses_a_configured_non_managed_source_without_downloading() {
    let home = tempfile::tempdir().unwrap();
    let config = config_with_source(home.path(), "system");
    let output = cli(home.path())
        .args(["tools", "install", "rg", "--yes", "--config"])
        .arg(&config)
        .env("PATH", empty_bin(home.path()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = text(&output.stderr);
    assert!(stderr.contains("configured as system"), "{stderr}");
    assert!(!data_directory(home.path()).exists());
}

#[test]
fn commands_that_are_not_tools_install_never_touch_the_data_directory() {
    let home = tempfile::tempdir().unwrap();
    for arguments in [
        vec!["--help"],
        vec!["tools"],
        vec!["tools", "--help"],
        vec!["tools", "status", "--json"],
        vec!["demo"],
    ] {
        let output = cli(home.path()).args(&arguments).output().unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {:?}",
            text(&output.stderr)
        );
        assert!(
            !data_directory(home.path()).exists(),
            "{arguments:?} created the data directory"
        );
    }
}

#[test]
fn tools_help_and_argument_errors_name_the_commands() {
    let home = tempfile::tempdir().unwrap();
    let help = cli(home.path()).args(["--help"]).output().unwrap();
    assert!(help.status.success());
    assert!(text(&help.stdout).contains("tars tools <status | install> [options]"));
    assert!(text(&help.stdout).contains("Check or install the external search tool"));

    let tools_help = cli(home.path()).args(["tools", "--help"]).output().unwrap();
    assert!(tools_help.status.success());
    let tools_help = text(&tools_help.stdout);
    assert!(tools_help.contains("tars tools status"));
    assert!(tools_help.contains("tars tools install rg"));
    assert!(tools_help.contains(&format!(
        "ripgrep {}",
        tars_core::external_tools::RIPGREP_VERSION
    )));
    assert!(tools_help.contains("[external_tools] rg"));

    for arguments in [
        vec!["tools", "unknown"],
        vec!["tools", "install"],
        vec!["tools", "install", "other"],
        vec!["tools", "status", "--config", "a.toml", "--no-config"],
        vec!["tools", "status", "--unknown"],
    ] {
        let output = cli(home.path()).args(&arguments).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(text(&output.stderr).contains("Use tars tools --help"));
    }
}
