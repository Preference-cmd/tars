use std::process::{Command, Output};

fn run(arguments: &[&str], directory: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(arguments)
        .current_dir(directory)
        .env("TMPDIR", directory)
        .env("TARS_HOME", directory.join("tars-home"))
        .env_remove("TARS_CONFIG")
        .output()
        .expect("run built TARS executable")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

#[test]
fn help_version_and_argument_errors_remain_available_without_files() {
    let directory = tempfile::tempdir().unwrap();
    for arguments in [vec![], vec!["--help"], vec!["-h"]] {
        let output = run(&arguments, directory.path());
        assert!(output.status.success(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
        let stdout = text(&output.stdout);
        assert!(stdout.contains("Usage: tars [interactive options]"));
        assert!(stdout.contains("--no-config"));
        assert!(stdout.contains("--code-execution"));
        assert!(stdout.contains("/doctor"));
        assert!(stdout.contains("/init"));
        assert!(stdout.contains("/new"));
        assert!(stdout.contains("/sessions"));
        assert!(stdout.contains("/agents"));
        assert!(stdout.contains("/mode"));
        assert!(stdout.contains("/compact"));
        assert!(stdout.contains("/help"));
        assert!(stdout.contains("tars run --provider <protocol>"));
        assert!(stdout.contains("tars session <create | run | list | show | compact | history | repair | export-materials | import-legacy>"));
        assert!(stdout.contains("tars tools <status | install>"));
        assert!(!stdout.contains("tars demo"));
        assert!(!stdout.contains("offline single-session"));
        assert!(!stdout.contains("tars-tui"));
        assert!(!stdout.contains("prototype"));
    }

    for version_flag in ["-V", "--version"] {
        let output = run(&[version_flag], directory.path());
        assert!(output.status.success());
        assert_eq!(
            text(&output.stdout),
            format!("tars {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    for arguments in [
        vec!["unknown"],
        vec!["demo"],
        vec!["demo", "--save"],
        vec!["demo", "--save", "material.json"],
        vec!["demo", "--unknown"],
    ] {
        let output = run(&arguments, directory.path());
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(text(&output.stderr).contains("Unsupported arguments"));
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn interactive_flags_forward_to_tui_and_respect_non_tty_and_store_isolation() {
    let directory = tempfile::tempdir().unwrap();

    // Interactive --help and -V via TUI option parser stay fast and clean.
    let help = run(&["--no-config", "--help"], directory.path());
    assert!(help.status.success());
    let help_stdout = text(&help.stdout);
    assert!(help_stdout.contains("/doctor"));
    assert!(help_stdout.contains("/init"));
    assert!(!help_stdout.contains("prototype"));
    assert!(!help_stdout.contains("tars-tui"));

    let version = run(&["--no-config", "-V"], directory.path());
    assert!(version.status.success());
    assert_eq!(
        text(&version.stdout),
        format!("tars {}\n", env!("CARGO_PKG_VERSION"))
    );

    // --no-config without --store still fails cleanly with exit code 2.
    let isolated = run(&["--no-config"], directory.path());
    assert_eq!(isolated.status.code(), Some(2));
    assert!(text(&isolated.stderr).contains("--store or a configured store directory is required"));

    // --no-config --store <dir> in non-TTY fails with exit code 1 (interactive terminal required).
    let store_dir = directory.path().join("isolated-store");
    let non_tty = run(
        &["--no-config", "--store", store_dir.to_str().unwrap()],
        directory.path(),
    );
    assert_eq!(non_tty.status.code(), Some(1));
    assert!(text(&non_tty.stderr).contains("interactive terminal"));

    // Zero-config (config enabled, no --store) resolves default $TARS_HOME/data and hits non-TTY check (exit 1).
    let zero_cfg = run(&["--code-execution"], directory.path());
    assert_eq!(zero_cfg.status.code(), Some(1));
    assert!(text(&zero_cfg.stderr).contains("interactive terminal"));
    assert!(!directory.path().join("tars-home").exists());
}
