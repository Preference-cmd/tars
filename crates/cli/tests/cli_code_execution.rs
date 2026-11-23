use serde_json::{Value, json};
use std::fs;
use std::process::{Command, Output};

#[path = "common/tools.rs"]
#[allow(dead_code)]
mod tools;

const KEY: &str = "fixture-secret";

fn tars_cmd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env("TARS_TEST_KEY", KEY);
    command
}

fn report(output: &Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("invalid json `{stdout}`: {e}"))
}

#[test]
fn ptc07_cli_flags_conflict_duplicate_and_inapplicable_check() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // 1. Mutually exclusive on `tars run`
    let run_conflict = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--input",
            "hi",
            "--code-execution",
            "--no-code-execution",
        ])
        .output()
        .unwrap();
    assert_eq!(run_conflict.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&run_conflict.stderr)
            .contains("--code-execution and --no-code-execution are mutually exclusive")
    );

    // 2. Duplicate on `tars run`
    let run_dup = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--input",
            "hi",
            "--code-execution",
            "--code-execution",
        ])
        .output()
        .unwrap();
    assert_eq!(run_dup.status.code(), Some(2));

    // 3. Mutually exclusive on `tars session create`
    let create_conflict = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--code-execution",
            "--no-code-execution",
        ])
        .output()
        .unwrap();
    assert_eq!(create_conflict.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&create_conflict.stderr)
            .contains("--code-execution and --no-code-execution are mutually exclusive")
    );

    // 4. Inapplicable on `tars session run`
    let session_run_flag = tars_cmd()
        .args([
            "session",
            "run",
            "some-id",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "hi",
            "--code-execution",
        ])
        .output()
        .unwrap();
    assert_eq!(session_run_flag.status.code(), Some(2));
}

#[cfg(unix)]
#[test]
fn ptc07_cli_run_and_session_show_history_with_code_execution() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("alpha.txt"), "ptc-cli-marker\n").unwrap();

    // 1. Transient `tars run --code-execution --json` executes `run_code` cleanly without stdout noise.
    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "run_code",
                json!({ "code": "const r = await read_file('alpha.txt'); return r.text.trim();" }),
                "call_rc_1",
            ),
            tools::answer_with_text("openai-chat", "found ptc-cli-marker"),
        ],
    );
    let run_out = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "read_file",
            "--code-execution",
            "--input",
            "read alpha",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");
    assert!(
        run_out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&run_out.stderr)
    );
    let json_report = report(&run_out);
    assert_eq!(json_report["status"], "completed");
    assert_eq!(json_report["text"], "found ptc-cli-marker");

    let reqs = fixture.requests.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    let offered_names: Vec<&str> = reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(offered_names.contains(&"run_code"), "{offered_names:?}");
    assert!(
        reqs[1].to_string().contains("ptc-cli-marker"),
        "second request should carry run_code output: {}",
        reqs[1]
    );

    // 2. TOML profile with [profiles.ptc.code_execution] enabled=true overridden by `--no-code-execution`.
    let config_path = dir.path().join("config.toml");
    let store = dir.path().join("store");
    let toml = format!(
        r#"version = 1
store = '{}'
default_profile = 'ptc'
[profiles.ptc]
provider = 'openai-chat'
model = 'profile-model'
api_key_env = 'TARS_TEST_KEY'
workspace_root = '{}'
tools = ['read_file']
[profiles.ptc.code_execution]
enabled = true
timeout_secs = 45
max_subcalls = 128
max_concurrency = 4
"#,
        store.display(),
        workspace.display(),
    );
    fs::write(&config_path, toml).unwrap();

    let fixture_disabled = tools::Fixture::with_responses(
        "openai-chat",
        vec![tools::answer_with_text("openai-chat", "no run_code")],
    );
    let disabled_out = tars_cmd()
        .args([
            "run",
            "--config",
            config_path.to_str().unwrap(),
            "--endpoint",
            &fixture_disabled.endpoint,
            "--no-code-execution",
            "--input",
            "hello",
        ])
        .output()
        .unwrap();
    assert!(disabled_out.status.success(), "{disabled_out:?}");
    assert_eq!(
        String::from_utf8_lossy(&disabled_out.stdout),
        "no run_code\n"
    );
    let disabled_reqs = fixture_disabled.requests.lock().unwrap().clone();
    let disabled_tools: Vec<&str> = disabled_reqs[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        !disabled_tools.contains(&"run_code"),
        "run_code must be omitted with --no-code-execution: {disabled_tools:?}"
    );

    // 3. Persistent `session create` from TOML preset -> `session show` -> `session run` -> `session history`.
    let fixture_session = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "run_code",
                json!({ "code": "const r = await read_file('alpha.txt'); return r.text.trim();" }),
                "call_rc_sess",
            ),
            tools::answer_with_text("openai-chat", "session done"),
        ],
    );
    let created = tars_cmd()
        .args([
            "session",
            "create",
            "--config",
            config_path.to_str().unwrap(),
            "--endpoint",
            &fixture_session.endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let conv_id = report(&created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let show_json = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(show_json.status.success(), "{show_json:?}");
    let show_val = report(&show_json);
    assert_eq!(show_val["code_execution"], true);
    assert_eq!(show_val["definition"]["code_execution"]["timeout_secs"], 45);
    assert_eq!(
        show_val["definition"]["code_execution"]["max_subcalls"],
        128
    );

    let show_text = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(show_text.status.success(), "{show_text:?}");
    let show_str = String::from_utf8_lossy(&show_text.stdout);
    assert!(
        show_str.contains("Code execution: on (timeout 45s, max_subcalls 128, max_concurrency 4)"),
        "{show_str}"
    );

    let sess_run = tars_cmd()
        .args([
            "session",
            "run",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
            "--input",
            "run script",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(sess_run.status.success(), "{sess_run:?}");
    assert_eq!(report(&sess_run)["status"], "completed");

    let history_out = tars_cmd()
        .args([
            "session",
            "history",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(history_out.status.success(), "{history_out:?}");
    let history_str = String::from_utf8_lossy(&history_out.stdout);
    assert!(
        history_str.contains("[tool call · original] run_code"),
        "{history_str}"
    );
    assert!(
        history_str.contains("\"subcalls\":1") && history_str.contains("ptc-cli-marker"),
        "{history_str}"
    );
}
