use causa::kernel::{BlockContent, ContentPart, Context};
use std::process::{Command, Output};

fn run(arguments: &[&str], directory: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args(arguments)
        .current_dir(directory)
        .env("TMPDIR", directory)
        .output()
        .expect("run built Tarz executable")
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
        assert!(text(&output.stdout).contains("tarz demo [--save <file>]"));
        assert!(text(&output.stdout).contains("offline single-session"));
    }
    let output = run(&["--version"], directory.path());
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        format!("tarz {}\n", env!("CARGO_PKG_VERSION"))
    );
    for arguments in [
        vec!["unknown"],
        vec!["demo", "--save"],
        vec!["demo", "--save", ""],
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
fn offline_demo_runs_real_work_loop_and_cleans_temporary_materials() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(&["demo"], directory.path());
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(output.stderr.is_empty());
    let report = text(&output.stdout);
    assert!(report.contains("Offline demo — scripted model, local tool, no network."));
    assert!(report.contains("Exact-key replay reused the first receipt."));
    assert!(report.contains("4 model calls; 1 tool call."));
    assert!(report.contains("Temporary material round-trip verified."));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn demo_save_contains_materials_only_and_roundtrips_public_context() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(&["demo", "--save", "material.json"], directory.path());
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert!(text(&output.stdout).contains("4 model calls; 1 tool call."));
    let path = directory.path().join("material.json");
    let wire: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let object = wire.as_object().unwrap();
    assert_eq!(object.len(), 1);
    assert!(object.contains_key("blocks"));
    let context: Context = serde_json::from_value(wire).unwrap();
    let loaded = tarz::material::load(&path).unwrap();
    assert_eq!(loaded.blocks(), context.blocks());
    assert!(
        context
            .blocks()
            .iter()
            .any(|block| matches!(block.content(), BlockContent::ToolCall(_)))
    );
    assert!(
        context
            .blocks()
            .iter()
            .any(|block| matches!(block.content(), BlockContent::ToolResult(_)))
    );
    let texts = context
        .blocks()
        .iter()
        .flat_map(|block| match block.content() {
            BlockContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(text.0.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => vec![],
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"Summary of A and B"));
    assert!(!texts.contains(&"Material A"));
    assert!(!texts.contains(&"Material B"));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn failed_save_reports_failure_and_leaves_no_temporary_files() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(
        &["demo", "--save", "absent/material.json"],
        directory.path(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).starts_with("Tarz: "));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}
