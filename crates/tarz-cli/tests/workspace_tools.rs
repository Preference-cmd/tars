#![cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::Value;
use std::process::Command;
#[path = "common/tools.rs"]
mod tools;

#[test]
fn actual_cli_process_runs_search_read_and_model_continuation_for_three_protocols() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let marker = format!(
            "retry original material {}",
            causa::runtime::new_block_id().0
        );
        std::fs::write(
            workspace.join("recovery.rs"),
            format!("header\n{marker}\nfooter\n"),
        )
        .unwrap();
        let fixture = tools::Fixture::new(provider, false);
        fixture.finish();
        let material = dir.path().join("material.json");
        let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
            .args([
                "run",
                "--no-config",
                "--provider",
                provider,
                "--model",
                "fixture-model",
                "--api-key-env",
                "PATH",
                "--endpoint",
                &fixture.endpoint,
                "--workspace",
            ])
            .arg(&workspace)
            .args([
                "--tools",
                "list_directory,read_file,search_text",
                "--input",
                "Find recovery path",
                "--json",
                "--save",
            ])
            .arg(&material)
            .args(if provider == "anthropic" {
                vec!["--max-tokens", "1024"]
            } else {
                vec![]
            })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {output:?}; requests={:?}; material={:?}",
            fixture.requests.lock().unwrap(),
            std::fs::read_to_string(&material)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "completed");
        assert_eq!(report["text"], "recovery.rs:2 retry original material");
        assert!(report["saved"].as_bool().unwrap());
        assert!(output.stderr.is_empty(), "{output:?}");
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for request in &*requests {
            assert_eq!(request["stream"], true);
            assert_eq!(request["tools"].as_array().unwrap().len(), 3);
        }
        assert!(requests[1].to_string().contains(&marker));
        assert!(requests[2].to_string().contains(&marker));
        let context = tarz_core::material::load(&material).unwrap();
        let results: Vec<_> = context
            .blocks()
            .iter()
            .filter_map(|b| {
                if let causa::kernel::BlockContent::ToolResult(r) = b.content() {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_ne!(results[0].call_block_id, results[1].call_block_id);
        assert!(
            results
                .iter()
                .all(|r| r.status == causa::kernel::ToolResultStatus::Succeeded)
        );
        assert!(results[1].output.content.to_string().contains(&marker));
    }
}

#[test]
fn missing_workspace_and_invalid_tool_selection_fail_before_model_connection() {
    let dir = tempfile::tempdir().unwrap();
    for (workspace, tools, exit) in [
        (dir.path().join("missing"), "read_file", 2),
        (dir.path().to_path_buf(), "shell", 2),
        (dir.path().to_path_buf(), "read_file,read_file", 2),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
            .args([
                "run",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "fixture",
                "--api-key-env",
                "PATH",
                "--endpoint",
                "http://127.0.0.1:1/fixture",
                "--workspace",
            ])
            .arg(workspace)
            .args(["--tools", tools, "--input", "test", "--json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(exit), "{output:?}");
        if exit == 2 {
            assert!(output.stdout.is_empty());
        } else {
            let report: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report["status"], "not_started");
        }
    }
}

#[test]
fn actual_tool_text_output_remains_only_the_final_answer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("recovery.rs"),
        "header\nretry original material\n",
    )
    .unwrap();
    let fixture = tools::Fixture::new("openai-chat", false);
    let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
        ])
        .arg(dir.path())
        .args([
            "--tools",
            "search_text,read_file",
            "--input",
            "Find recovery path",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"recovery.rs:2 retry original material\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}

#[cfg(unix)]
#[test]
fn cancellation_after_actual_tools_preserves_committed_results_and_machine_stdout() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("recovery.rs"),
        "header\nretry original material\n",
    )
    .unwrap();
    let fixture = tools::Fixture::new("openai-chat", true);
    let saved = dir.path().join("cancelled.json");
    let child = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
        ])
        .arg(dir.path())
        .args([
            "--tools",
            "search_text,read_file",
            "--input",
            "Find recovery path",
            "--json",
            "--save",
        ])
        .arg(&saved)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.requests.lock().unwrap().len() < 3 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Command::new("kill")
            .args(["-s", "INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "cancelled");
    assert_eq!(report["saved"], true);
    let context = tarz_core::material::load(saved).unwrap();
    assert_eq!(
        context
            .blocks()
            .iter()
            .filter(|block| matches!(block.content(), causa::kernel::BlockContent::ToolResult(_)))
            .count(),
        2
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}
