use std::process::{Command, Output};

fn run(arguments: &[&str], directory: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(arguments)
        .current_dir(directory)
        .env("TMPDIR", directory)
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
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let stdout = text(&output.stdout);
        assert!(stdout.contains("tars run --provider <protocol>"));
        assert!(stdout.contains("tars session <create | run | list | show | compact | history | repair | export-materials | import-legacy>"));
        assert!(stdout.contains("tars tools <status | install>"));
        assert!(!stdout.contains("tars demo"));
        assert!(!stdout.contains("offline single-session"));
    }
    let output = run(&["--version"], directory.path());
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        format!("tars {}\n", env!("CARGO_PKG_VERSION"))
    );
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
