#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use std::fs;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[allow(dead_code)]
#[path = "common/tools.rs"]
mod tools;

fn wait_child_bounded(mut child: Child, timeout: Duration) -> Output {
    let start = Instant::now();
    loop {
        match child.try_wait().expect("try_wait failed") {
            Some(_) => return child.wait_with_output().expect("wait_with_output failed"),
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child process timed out after {timeout:?}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn cli_workspace_trust_flag_notices_json_isolation_and_user_rules() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    let tars_home = temp.path().join("tars-home");
    fs::create_dir_all(workspace.join(".tars")).unwrap();
    fs::create_dir_all(&tars_home).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    // User-level rules via TARS_HOME: allow `pwd`, deny `uname`.
    fs::write(
        tars_home.join("permissions.toml"),
        r#"version = 1
[[command]]
prefix = ["pwd"]
decision = "allow"

[[command]]
prefix = ["uname"]
decision = "deny"
"#,
    )
    .unwrap();

    // Workspace rules: allow `echo`, deny `id`.
    fs::write(
        workspace.join(".tars/permissions.toml"),
        r#"version = 1
[[command]]
prefix = ["echo"]
decision = "allow"

[[command]]
prefix = ["id"]
decision = "deny"
"#,
    )
    .unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            // Session create (no model call) -> session run 1 (untrusted):
            // 1. `pwd` (user allow -> succeeds)
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "pwd"}),
                "c1",
            ),
            tools::answer_with_text("openai-chat", "pwd ok"),
            // 2. `echo hi` (untrusted workspace allow -> rejected without route)
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo hi"}),
                "c2",
            ),
            tools::answer_with_text("openai-chat", "echo blocked"),
            // Session run 2 (with --trust-workspace-rules):
            // 3. `echo hi` (now trusted -> succeeds)
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo hi"}),
                "c3",
            ),
            tools::answer_with_text("openai-chat", "echo ok"),
            // Session run 3 (without --trust-workspace-rules; trust persisted in Store):
            // 4. `echo persisted` (still trusted -> succeeds)
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo persisted"}),
                "c4",
            ),
            tools::answer_with_text("openai-chat", "echo persisted ok"),
            // Transient `tars run` (user `pwd` allowed, workspace `echo` ignored + stderr notice):
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "pwd"}),
                "c5",
            ),
            tools::answer_with_text("openai-chat", "transient pwd ok"),
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo transient"}),
                "c6",
            ),
            tools::answer_with_text("openai-chat", "transient echo blocked"),
            // Transient `tars run --no-config` skips user rules (`pwd` rejected in ask mode):
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "pwd"}),
                "c7",
            ),
            tools::answer_with_text("openai-chat", "no-config pwd blocked"),
        ],
    );

    // 1. session create without --trust-workspace-rules: stderr reports ignored count, stdout JSON clean.
    let create_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .env("TARS_HOME", &tars_home)
        .env_remove("TARS_CONFIG")
        .args([
            "session",
            "create",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "test-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "run_command",
            "--permission-mode",
            "ask",
            "--json",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        create_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&create_out.stderr)
    );
    let create_json: Value = serde_json::from_slice(&create_out.stdout).expect("valid create json");
    let id = create_json["conversation_id"].as_str().unwrap().to_owned();
    let create_stderr = String::from_utf8_lossy(&create_out.stderr);
    assert!(
        create_stderr.contains("ignored 1 allow rule"),
        "stderr: {create_stderr}"
    );
    assert!(
        create_stderr.contains("--trust-workspace-rules"),
        "stderr: {create_stderr}"
    );

    // 2. session run without --trust-workspace-rules: user `pwd` runs, workspace `echo` blocked, stderr warns.
    let run1 = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "session",
                "run",
                &id,
                "--store",
                store.to_str().unwrap(),
                "--input",
                "run pwd",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(run1.status.success());
    let run1_json: Value = serde_json::from_slice(&run1.stdout).expect("valid run1 json");
    assert_eq!(run1_json["text"], "pwd ok");
    let run1_stderr = String::from_utf8_lossy(&run1.stderr);
    assert!(
        run1_stderr.contains("ignored 1 allow rule"),
        "{run1_stderr}"
    );
    assert!(
        run1_stderr.contains("allowed by user rule `pwd`"),
        "{run1_stderr}"
    );

    let run1b = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "session",
                "run",
                &id,
                "--store",
                store.to_str().unwrap(),
                "--input",
                "run echo",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(run1b.status.success());
    let run1b_json: Value = serde_json::from_slice(&run1b.stdout).unwrap();
    assert_eq!(run1b_json["text"], "echo blocked");

    // 3. session run WITH --trust-workspace-rules: no ignored warning, `echo` succeeds.
    let run2 = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "session",
                "run",
                &id,
                "--store",
                store.to_str().unwrap(),
                "--trust-workspace-rules",
                "--input",
                "run trusted echo",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(run2.status.success());
    let run2_json: Value = serde_json::from_slice(&run2.stdout).unwrap();
    assert_eq!(run2_json["text"], "echo ok");
    let run2_stderr = String::from_utf8_lossy(&run2.stderr);
    assert!(!run2_stderr.contains("ignored"));
    assert!(
        run2_stderr.contains("allowed by workspace rule `echo`"),
        "{run2_stderr}"
    );

    // 4. Subsequent session run WITHOUT --trust-workspace-rules: trust persisted, no warning, `echo` succeeds.
    let run3 = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "session",
                "run",
                &id,
                "--store",
                store.to_str().unwrap(),
                "--input",
                "run persisted echo",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(run3.status.success());
    let run3_json: Value = serde_json::from_slice(&run3.stdout).unwrap();
    assert_eq!(run3_json["text"], "echo persisted ok");
    assert!(!String::from_utf8_lossy(&run3.stderr).contains("ignored"));

    // 5. Transient `tars run` rejects `--trust-workspace-rules` as argument error (exit 2).
    let bad_flag = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "run",
            "--no-config",
            "--trust-workspace-rules",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--input",
            "hi",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_flag.status.code(), Some(2));

    // 6. Transient `tars run`: user `pwd` succeeds, workspace `echo` ignored with stderr hint, JSON stdout clean.
    let tr_pwd = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "run",
                "--provider",
                "openai-chat",
                "--model",
                "test-model",
                "--api-key-env",
                "PATH",
                "--endpoint",
                &fixture.endpoint,
                "--workspace",
                workspace.to_str().unwrap(),
                "--tools",
                "run_command",
                "--permission-mode",
                "ask",
                "--input",
                "transient pwd",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(tr_pwd.status.success());
    let tr_pwd_json: Value = serde_json::from_slice(&tr_pwd.stdout).unwrap();
    assert_eq!(tr_pwd_json["text"], "transient pwd ok");
    assert!(String::from_utf8_lossy(&tr_pwd.stderr).contains("ignored 1 allow rule"));

    let tr_echo = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "run",
                "--provider",
                "openai-chat",
                "--model",
                "test-model",
                "--api-key-env",
                "PATH",
                "--endpoint",
                &fixture.endpoint,
                "--workspace",
                workspace.to_str().unwrap(),
                "--tools",
                "run_command",
                "--permission-mode",
                "ask",
                "--input",
                "transient echo",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(tr_echo.status.success());
    let tr_echo_json: Value = serde_json::from_slice(&tr_echo.stdout).unwrap();
    assert_eq!(tr_echo_json["text"], "transient echo blocked");

    // 7. Transient `tars run --no-config`: skips user rules (`pwd` rejected without confirmation route).
    let tr_nocfg = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .env("TARS_HOME", &tars_home)
            .env_remove("TARS_CONFIG")
            .args([
                "run",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "test-model",
                "--api-key-env",
                "PATH",
                "--endpoint",
                &fixture.endpoint,
                "--workspace",
                workspace.to_str().unwrap(),
                "--tools",
                "run_command",
                "--permission-mode",
                "ask",
                "--input",
                "no-config pwd",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(tr_nocfg.status.success());
    let tr_nocfg_json: Value = serde_json::from_slice(&tr_nocfg.stdout).unwrap();
    assert_eq!(tr_nocfg_json["text"], "no-config pwd blocked");

    let reqs = fixture.requests.lock().unwrap();
    // Request #3 observed `echo hi` rejected without route; request #13 (`tr_nocfg`) observed `pwd` rejected.
    assert!(
        reqs[3]
            .to_string()
            .contains("no confirmation route available"),
        "expected untrusted echo rejection in req[3]"
    );
    assert!(
        reqs[11]
            .to_string()
            .contains("no confirmation route available"),
        "expected transient echo rejection in req[11]"
    );
    assert!(
        reqs[13]
            .to_string()
            .contains("no confirmation route available"),
        "expected --no-config pwd rejection in req[13]"
    );
}

#[test]
fn cli_session_create_with_trust_workspace_rules_persists_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join(".tars")).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\n",
    )
    .unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo created-trusted"}),
                "c1",
            ),
            tools::answer_with_text("openai-chat", "echo done"),
        ],
    );

    let create_out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--trust-workspace-rules",
            "--provider",
            "openai-chat",
            "--model",
            "test-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "run_command",
            "--permission-mode",
            "ask",
            "--json",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(create_out.status.success());
    assert!(!String::from_utf8_lossy(&create_out.stderr).contains("ignored"));
    let create_json: Value = serde_json::from_slice(&create_out.stdout).unwrap();
    let id = create_json["conversation_id"].as_str().unwrap();

    let run_out = wait_child_bounded(
        Command::new(env!("CARGO_BIN_EXE_tars"))
            .args([
                "session",
                "run",
                id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--input",
                "run echo",
                "--json",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(10),
    );
    assert!(run_out.status.success());
    let run_json: Value = serde_json::from_slice(&run_out.stdout).unwrap();
    assert_eq!(run_json["text"], "echo done");
    assert!(!String::from_utf8_lossy(&run_out.stderr).contains("ignored"));
}

#[test]
fn cli_exits_nonzero_with_location_when_rules_file_is_invalid() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join(".tars")).unwrap();
    fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"SUPER_SECRET_ARG\"]\ndecision = \"invalid_mode\"\n",
    )
    .unwrap();

    let fixture = tools::Fixture::with_responses("openai-chat", vec![]);
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "test-model",
            "--api-key-env",
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "run_command",
            "--input",
            "hi",
            "--json",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("permissions.toml"), "stderr: {stderr}");
    assert!(stderr.contains("line 4, column"), "stderr: {stderr}");
    assert!(!stderr.contains("SUPER_SECRET_ARG"), "stderr: {stderr}");
    assert!(!stderr.contains("invalid_mode"), "stderr: {stderr}");
    let report: Value = serde_json::from_slice(&out.stdout).expect("valid failure json");
    assert_eq!(report["status"], "not_started");
}
