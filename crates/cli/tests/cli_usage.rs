#![cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::{Value, json};
use std::fs;
use std::process::Command;

#[allow(dead_code)]
#[path = "common/tools.rs"]
mod tools;

fn run_json(workspace: &std::path::Path, endpoint: &str, tools: &str) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_tars"))
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
            endpoint,
            "--workspace",
        ])
        .arg(workspace)
        .args(["--tools", tools, "--input", "read recovery.rs", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn workspace(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    fs::write(workspace.join("recovery.rs"), "line1\nline2\nline3\n").unwrap();
    workspace
}

#[test]
fn us09_tars_run_json_reports_the_work_of_a_two_round_tool_loop() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(&dir);
    // OpenAI Chat reports prompt tokens including cached ones; TARS gets the
    // uncached input as prompt - cached. Only the second round reports reasoning.
    let responses = vec![
        tools::with_chat_usage(
            tools::tool(
                "openai-chat",
                "read_file",
                json!({"path": "recovery.rs", "start_line": 1, "max_lines": 2}),
                "call_1",
            ),
            json!({"prompt_tokens": 55, "completion_tokens": 10, "total_tokens": 65,
                "prompt_tokens_details": {"cached_tokens": 5}}),
        ),
        tools::with_chat_usage(
            tools::answer_with_text("openai-chat", "done reading file"),
            json!({"prompt_tokens": 70, "completion_tokens": 15, "total_tokens": 85,
                "prompt_tokens_details": {"cached_tokens": 10},
                "completion_tokens_details": {"reasoning_tokens": 4}}),
        ),
    ];
    let fixture = tools::Fixture::with_responses("openai-chat", responses);
    let rep = run_json(&workspace, &fixture.endpoint, "read_file");
    assert_eq!(rep["status"], "completed");

    let usage = &rep["usage"];
    assert_eq!(
        usage["calls"],
        json!({"succeeded": 2, "failed": 0, "usage_unreported": 0})
    );
    assert_eq!(usage["model_rounds"], 2);
    assert_eq!(usage["input_tokens"], 110);
    assert_eq!(usage["cache_read_tokens"], 15);
    assert!(usage["cache_write_tokens"].is_null());
    assert_eq!(usage["output_tokens"], 25);
    assert_eq!(usage["reasoning_tokens"], 4);
    assert_eq!(usage["prompt_tokens"], 125);
    assert_eq!(usage["total_tokens"], 150);
    assert!((usage["cache_hit_rate"].as_f64().unwrap() - 15.0 / 125.0).abs() < 1e-9);

    assert_eq!(usage["conversation"]["calls"]["succeeded"], 2);
    assert_eq!(usage["conversation"]["total_tokens"], 150);
    assert!(usage["conversation"].get("model_rounds").is_none());
    assert_eq!(usage["compaction"]["calls"]["succeeded"], 0);
    assert!(usage["compaction"]["input_tokens"].is_null());

    let last = &usage["last_call"];
    assert_eq!(last["purpose"], "conversation");
    assert_eq!(last["input_tokens"], 60);
    assert_eq!(last["output_tokens"], 15);
    assert_eq!(last["reasoning_tokens"], 4);
    assert_eq!(last["prompt_tokens"], 70);
    assert!(last["at_unix_ms"].as_u64().unwrap() > 0);
}

#[test]
fn us09_tars_run_json_shows_a_call_without_usage_as_null_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(&dir);
    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![tools::answer_with_text("openai-chat", "no usage here")],
    );
    let rep = run_json(&workspace, &fixture.endpoint, "read_file");
    assert_eq!(rep["status"], "completed");
    let usage = &rep["usage"];
    assert_eq!(
        usage["calls"],
        json!({"succeeded": 1, "failed": 0, "usage_unreported": 1})
    );
    assert_eq!(usage["model_rounds"], 1);
    for field in [
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "reasoning_tokens",
        "prompt_tokens",
        "total_tokens",
        "cache_hit_rate",
    ] {
        assert!(usage[field].is_null(), "{field}: {usage}");
        assert!(
            usage["last_call"][field].is_null(),
            "last_call {field}: {usage}"
        );
    }
}

#[test]
fn tars_run_json_reports_null_usage_when_the_work_never_started() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tars"))
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
            "http://127.0.0.1:9",
            "--input",
            "hello",
            "--json",
            "--load",
        ])
        .arg(dir.path().join("missing.json"))
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let rep: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rep["status"], "not_started");
    assert!(rep.get("usage").is_some_and(Value::is_null), "{rep}");
}

#[test]
fn us09_session_show_json_and_text_and_omitted_cache() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    fs::write(workspace.join("recovery.rs"), "line1\nline2\n").unwrap();
    let store = dir.path().join("store");

    // Fixture with omitted cache fields
    let usage_no_cache = json!({
        "prompt_tokens": 100,
        "completion_tokens": 20,
        "total_tokens": 120,
    });
    let responses = vec![tools::with_chat_usage(
        tools::answer_with_text("openai-chat", "hello without cache"),
        usage_no_cache,
    )];
    let fixture = tools::Fixture::with_responses("openai-chat", responses);

    // 1. Create session
    let create_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "create", "--store"])
        .arg(&store)
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create_out.status.success(), "{create_out:?}");
    let rep: Value = serde_json::from_slice(&create_out.stdout).unwrap();
    let id = rep["conversation_id"].as_str().unwrap().to_owned();

    // 2. Run session
    let run_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "run", &id, "--store"])
        .arg(&store)
        .args(["--input", "run test", "--json"])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");
    let run_rep: Value = serde_json::from_slice(&run_out.stdout).unwrap();
    assert_eq!(run_rep["status"], "completed");
    let run_usage = &run_rep["usage"];
    assert_eq!(run_usage["calls"]["succeeded"], 1);
    assert_eq!(run_usage["calls"]["usage_unreported"], 0);
    assert_eq!(run_usage["input_tokens"], 100);
    assert!(run_usage["cache_read_tokens"].is_null());
    assert!(run_usage["cache_write_tokens"].is_null());
    assert!(run_usage["cache_hit_rate"].is_null());

    // 3. session show --json
    let show_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "show", &id, "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(show_out.status.success(), "{show_out:?}");
    let show_rep: Value = serde_json::from_slice(&show_out.stdout).unwrap();
    let show_usage = &show_rep["usage"];
    assert_eq!(show_usage["input_rounds"], 1);
    assert_eq!(show_usage["total"]["calls"]["succeeded"], 1);
    assert_eq!(show_usage["total"]["input_tokens"], 100);
    assert!(show_usage["total"]["cache_read_tokens"].is_null());
    assert!(show_usage["total"]["cache_hit_rate"].is_null());
    assert_eq!(show_usage["conversation"]["calls"]["succeeded"], 1);
    assert_eq!(show_usage["compaction"]["calls"]["succeeded"], 0);

    // 4. session show text mode
    let text_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "show", &id, "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(text_out.status.success(), "{text_out:?}");
    let stdout_text = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        stdout_text
            .contains("Usage: 1 call(s) (1 ok, 0 failed, 0 without usage) · 1 input round(s)")
    );
    assert!(stdout_text.contains("cache read not reported"));
    assert!(stdout_text.contains("cache hit not reported"));
}

#[test]
fn us10_cli_session_show_text_contains_session_usage_lines() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    let usage_fixture = json!({
        "prompt_tokens": 114,
        "completion_tokens": 20,
        "total_tokens": 134,
        "prompt_tokens_details": {"cached_tokens": 11}
    });
    let responses = vec![tools::with_chat_usage(
        tools::answer_with_text("openai-chat", "answering for US10"),
        usage_fixture,
    )];
    let fixture = tools::Fixture::with_responses("openai-chat", responses);

    let create_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "create", "--store"])
        .arg(&store)
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create_out.status.success(), "{create_out:?}");
    let rep: Value = serde_json::from_slice(&create_out.stdout).unwrap();
    let id = rep["conversation_id"].as_str().unwrap().to_owned();

    let run_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "run", &id, "--store"])
        .arg(&store)
        .args(["--input", "run input"])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");

    // Show in text mode
    let text_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "show", &id, "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(text_out.status.success(), "{text_out:?}");
    let stdout_text = String::from_utf8_lossy(&text_out.stdout);

    // Open session using tars_core directly to get SessionUsage and call session_usage_lines
    let rt = tokio::runtime::Runtime::new().unwrap();
    let expected_lines = rt.block_on(async {
        let host = tars_core::application::SessionHost::new(Some(store.clone()));
        let session = host.open(id.into()).await.unwrap();
        let usage = session.session_usage().unwrap();
        tars_core::usage::session_usage_lines(&usage)
    });

    for line in expected_lines {
        assert!(
            stdout_text.contains(&line),
            "missing expected line '{line}' in stdout:\n{stdout_text}"
        );
    }
}
