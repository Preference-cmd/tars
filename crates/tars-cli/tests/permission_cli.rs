#![cfg(any(target_os = "linux", target_os = "macos"))]

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
fn ask_mode_non_terminal_write_rejected_without_blocking() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    fs::write(workspace.join("sample.txt"), "sample file content\n").unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "out.txt", "content": "should not exist"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "model observed write rejection"),
            tools::tool(
                "openai-chat",
                "read_file",
                json!({"path": "sample.txt"}),
                "call_2",
            ),
            tools::answer_with_text("openai-chat", "read sample completed"),
        ],
    );

    let child = Command::new(env!("CARGO_BIN_EXE_tars"))
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
        ])
        .arg(&workspace)
        .args([
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "ask",
            "--input",
            "Write the file",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let output = wait_child_bounded(child, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_file = workspace.join("out.txt");
    assert!(!target_file.exists(), "target file must not be created");

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(&stdout_str).expect("parse stdout json");
    assert_eq!(report["status"], "completed");
    assert_eq!(report["text"], "model observed write rejection");
    assert!(report["error"].is_null());

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr_str.contains("Allow this write? [y/n] "));

    let reqs = fixture.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let second_req = reqs[1].to_string();
    assert!(
        second_req.contains("no confirmation route available"),
        "model second round should show rejected error: {second_req}"
    );
    drop(reqs);

    // The read-only call runs under the same fixture so report keys must match.
    let readonly_child = Command::new(env!("CARGO_BIN_EXE_tars"))
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
        ])
        .arg(&workspace)
        .args([
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "ask",
            "--input",
            "Read the sample file",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let readonly_output = wait_child_bounded(readonly_child, Duration::from_secs(10));
    assert!(
        readonly_output.status.success(),
        "readonly stderr: {}",
        String::from_utf8_lossy(&readonly_output.stderr)
    );
    let readonly_stdout = String::from_utf8_lossy(&readonly_output.stdout);
    let readonly_report: Value =
        serde_json::from_str(&readonly_stdout).expect("parse readonly json");

    let write_keys: std::collections::BTreeSet<_> = report
        .as_object()
        .expect("report is object")
        .keys()
        .cloned()
        .collect();
    let readonly_keys: std::collections::BTreeSet<_> = readonly_report
        .as_object()
        .expect("readonly report is object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        write_keys, readonly_keys,
        "top-level keys of rejected write report must match readonly run report"
    );
}

#[test]
fn accept_edits_mode_executes_write_without_asking() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "accepted.txt", "content": "written by accept-edits"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "write succeeded"),
        ],
    );

    let child = Command::new(env!("CARGO_BIN_EXE_tars"))
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
        ])
        .arg(&workspace)
        .args([
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "accept-edits",
            "--input",
            "Write the file",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let output = wait_child_bounded(child, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_file = workspace.join("accepted.txt");
    assert!(target_file.exists(), "target file must be created");
    assert_eq!(
        fs::read_to_string(target_file).unwrap(),
        "written by accept-edits"
    );

    let stderr_str = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr_str.contains("Allow this write? [y/n] "));
}

#[test]
fn read_only_mode_rejects_with_mode_name() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "readonly.txt", "content": "should fail"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "write was read-only rejected"),
        ],
    );

    let child = Command::new(env!("CARGO_BIN_EXE_tars"))
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
        ])
        .arg(&workspace)
        .args([
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "read-only",
            "--input",
            "Write file in read-only",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let output = wait_child_bounded(child, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_file = workspace.join("readonly.txt");
    assert!(!target_file.exists(), "readonly write must not create file");

    let reqs = fixture.requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    let second_req = reqs[1].to_string();
    assert!(
        second_req.contains("read-only"),
        "rejection should include mode name 'read-only': {second_req}"
    );
}

#[test]
fn permission_mode_invalid_values_and_inapplicable_subcommands() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    fs::create_dir_all(&store).unwrap();

    // 1. Invalid permission mode on tars run
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "run",
            "--permission-mode",
            "invalid-mode",
            "--input",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("read-only"));
    assert!(stderr.contains("ask"));
    assert!(stderr.contains("accept-edits"));
    assert!(stderr.contains("full-access"));

    // 2. Invalid permission mode on session create
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "bogus",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("read-only"));
    assert!(stderr.contains("ask"));
    assert!(stderr.contains("accept-edits"));
    assert!(stderr.contains("full-access"));

    // 3. Invalid permission mode on session run
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            "fake-id",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "bogus",
            "--input",
            "hello",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("read-only"));
    assert!(stderr.contains("ask"));
    assert!(stderr.contains("accept-edits"));
    assert!(stderr.contains("full-access"));

    // 4. session list does not accept --permission-mode
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "list",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "ask",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // 5. session repair does not accept --permission-mode
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "repair",
            "fake-id",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "accept-edits",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // 6. session export-materials does not accept --permission-mode
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "export-materials",
            "fake-id",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "ask",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn session_preset_accept_edits_unaffected_by_config_change_to_read_only() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let store = temp.path().join("store");
    fs::create_dir_all(&store).unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "preset.txt", "content": "preset content"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "preset write succeeded"),
        ],
    );

    let config_path = temp.path().join("config.toml");
    let initial_config = format!(
        "version = 1\n\
        default_profile = 'test_profile'\n\
        [profiles.test_profile]\n\
        provider = 'openai-chat'\n\
        model = 'm'\n\
        api_key_env = 'PATH'\n\
        endpoint = '{}'\n\
        workspace_root = '{}'\n\
        tools = ['read_file', 'write_file']\n\
        permission_mode = 'accept-edits'\n",
        fixture.endpoint,
        workspace.display()
    );
    fs::write(&config_path, &initial_config).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "create",
            "--config",
            config_path.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let create_rep: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = create_rep["conversation_id"].as_str().unwrap();

    let modified_config = format!(
        "version = 1\n\
        default_profile = 'test_profile'\n\
        [profiles.test_profile]\n\
        provider = 'openai-chat'\n\
        model = 'm'\n\
        api_key_env = 'PATH'\n\
        endpoint = '{}'\n\
        workspace_root = '{}'\n\
        tools = ['read_file', 'write_file']\n\
        permission_mode = 'read-only'\n",
        fixture.endpoint,
        workspace.display()
    );
    fs::write(&config_path, &modified_config).unwrap();

    // session run should still execute write because the session definition retains accept-edits
    let child = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            id,
            "--config",
            config_path.to_str().unwrap(),
            "--store",
            store.to_str().unwrap(),
            "--input",
            "Write preset file",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let output = wait_child_bounded(child, Duration::from_secs(10));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let target_file = workspace.join("preset.txt");
    assert!(
        target_file.exists(),
        "file must be written under session definition accept-edits"
    );
    assert_eq!(fs::read_to_string(target_file).unwrap(), "preset content");
}

#[test]
fn session_lifecycle_modes_create_ask_run_accept_edits_then_rerun_without_flag() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let store = temp.path().join("store");
    fs::create_dir_all(&store).unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "file1.txt", "content": "from accept-edits"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "first write done"),
            // Round 2 arrives without the flag: the saved mode must still apply.
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "file2.txt", "content": "from persistent mode"}),
                "call_2",
            ),
            tools::answer_with_text("openai-chat", "second write done"),
        ],
    );

    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
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
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "ask",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let rep: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = rep["conversation_id"].as_str().unwrap().to_owned();

    let child1 = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "accept-edits",
            "--input",
            "run 1",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out1 = wait_child_bounded(child1, Duration::from_secs(10));
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert!(workspace.join("file1.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("file1.txt")).unwrap(),
        "from accept-edits"
    );

    let child2 = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "run 2",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out2 = wait_child_bounded(child2, Duration::from_secs(10));
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(workspace.join("file2.txt").exists());
    assert_eq!(
        fs::read_to_string(workspace.join("file2.txt")).unwrap(),
        "from persistent mode"
    );
}

#[test]
fn session_lifecycle_full_access_reverts_to_ask_on_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let store = temp.path().join("store");
    fs::create_dir_all(&store).unwrap();

    let fixture = tools::Fixture::with_responses(
        "openai-chat",
        vec![
            // Run 1: write under full-access
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "fa.txt", "content": "written in full-access"}),
                "call_1",
            ),
            tools::answer_with_text("openai-chat", "fa done"),
            // Run 2: reopen without flags reverts full-access to ask, so a
            // non-terminal write is rejected.
            tools::tool(
                "openai-chat",
                "write_file",
                json!({"path": "revert.txt", "content": "should be rejected"}),
                "call_2",
            ),
            tools::answer_with_text("openai-chat", "reopen rejection handled"),
        ],
    );

    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
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
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "ask",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let rep: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = rep["conversation_id"].as_str().unwrap().to_owned();

    let child1 = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "full-access",
            "--input",
            "run full-access",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out1 = wait_child_bounded(child1, Duration::from_secs(10));
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert!(workspace.join("fa.txt").exists());

    let records: Vec<_> = fs::read_dir(store.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(records.len(), 1);
    let record_content = fs::read_to_string(records[0].path()).unwrap();
    assert!(record_content.contains("\"permission_mode\":\"full-access\""));

    // Reopen without flags: full-access reverts to ask, so the write is rejected.
    let child2 = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "reopen run",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out2 = wait_child_bounded(child2, Duration::from_secs(10));
    assert!(
        out2.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(
        !workspace.join("revert.txt").exists(),
        "write must be rejected under reverted ask mode"
    );

    let reqs = fixture.requests.lock().unwrap();
    assert_eq!(reqs.len(), 4);
    let last_req = reqs[3].to_string();
    assert!(
        last_req.contains("no confirmation route available"),
        "reverted ask mode must report no confirmation route available: {last_req}"
    );
}

#[test]
fn session_run_mode_change_save_failure_does_not_submit_and_exits_code_1() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let store = temp.path().join("store");
    fs::create_dir_all(&store).unwrap();

    let fixture = tools::Fixture::with_responses("openai-chat", vec![]);
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
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
            "PATH",
            "--endpoint",
            &fixture.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--tools",
            "read_file,write_file",
            "--permission-mode",
            "ask",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let rep: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = rep["conversation_id"].as_str().unwrap().to_owned();

    let sessions_dir = store.join("sessions");
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(sessions_dir.clone());
    std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let probe = sessions_dir.join("probe-write");
    if std::fs::write(&probe, "").is_ok() {
        let _ = std::fs::remove_file(probe);
        eprintln!("filesystem permissions are bypassed; save-failure fixture unavailable");
        return;
    }

    // The mode change needs a save; both text and JSON must exit 1 without
    // submitting the input.
    let child_text = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "accept-edits",
            "--input",
            "submit text",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out_text = wait_child_bounded(child_text, Duration::from_secs(10));
    assert_eq!(out_text.status.code(), Some(1));
    let err_msg = String::from_utf8_lossy(&out_text.stderr);
    assert!(
        err_msg.contains("session save is blocked"),
        "stderr must report 'session save is blocked', got: {err_msg}"
    );

    let child_json = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--permission-mode",
            "accept-edits",
            "--input",
            "submit json",
            "--json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let out_json = wait_child_bounded(child_json, Duration::from_secs(10));
    assert_eq!(out_json.status.code(), Some(1));
    let stdout_str = String::from_utf8_lossy(&out_json.stdout);
    let report: Value = serde_json::from_str(&stdout_str).expect("parse early_failure json");
    assert_eq!(report["status"], "not_started");
    assert_eq!(report["saved"], false);
    let json_err = report["error"].as_str().expect("error string");
    assert!(
        json_err.contains("session save is blocked"),
        "json error must report 'session save is blocked', got: {json_err}"
    );

    // Fixture should NOT have received any model requests (input was not submitted)
    assert_eq!(fixture.requests.lock().unwrap().len(), 0);

    drop(_restore);
    std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    let record_path = sessions_dir.join(format!("{id}.json"));
    let record_str = std::fs::read_to_string(&record_path).expect("read session record file");
    let record_val: Value = serde_json::from_str(&record_str).expect("parse session record json");

    assert_eq!(record_val["format"], "tars-session-v2");
    assert!(record_val["definition"].get("permission_mode").is_none());
    // DiscardLocal must drop accept-edits; Ask stays implicit in the record.
    assert!(
        record_val.get("permission_mode").is_none(),
        "record must omit permission_mode when current mode is creation mode (ask)"
    );
    assert_ne!(
        record_val.get("permission_mode").and_then(Value::as_str),
        Some("accept-edits"),
        "DiscardLocal must not leave accept-edits in record"
    );

    let out_list = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "list",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out_list.status.success());
    let list_val: Value = serde_json::from_slice(&out_list.stdout).unwrap();
    let entry = list_val["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["conversation_id"] == id)
        .expect("session found in list");
    assert_eq!(entry["active"], false);
    assert!(entry["record_error"].is_null());
}
