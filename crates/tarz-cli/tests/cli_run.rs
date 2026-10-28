use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[path = "common/sse.rs"]
mod sse;

fn server(status: u16, body: Value, delay: Duration) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = vec![];
        let split = loop {
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .unwrap()
            .parse()
            .unwrap();
        while bytes.len() < split + length {
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        tx.send(String::from_utf8(bytes).unwrap()).unwrap();
        std::thread::sleep(delay);
        let (content_type, body) = sse::reply(status, body);
        let _ = write!(
            socket,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    (endpoint, rx)
}

fn command(provider: &str, endpoint: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    command
        .args([
            "run",
            "--provider",
            provider,
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARZ_TEST_KEY",
            "--endpoint",
            endpoint,
        ])
        .env("TARZ_TEST_KEY", "fixture-secret");
    if provider == "anthropic" {
        command.args(["--max-tokens", "1024"]);
    }
    command
}
fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}
fn chat() -> Value {
    json!({"choices":[{"message":{"role":"assistant","content":"fixture answer"},"finish_reason":"stop"}]})
}

#[test]
fn actual_provider_protocols_receive_selected_model_input_and_credentials() {
    for (provider, body, header) in [
        (
            "openai-chat",
            chat(),
            "authorization: Bearer fixture-secret",
        ),
        (
            "openai-responses",
            json!({"status":"completed", "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text","text":"fixture answer"}]}]}),
            "authorization: Bearer fixture-secret",
        ),
        (
            "anthropic",
            json!({"content":[{"type":"text","text":"fixture answer"}],"stop_reason":"end_turn"}),
            "x-api-key: fixture-secret",
        ),
    ] {
        let (endpoint, requests) = server(200, body, Duration::ZERO);
        let output = command(provider, &endpoint)
            .args(["--input", "user question", "--json"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let report = report(&output);
        assert_eq!(report["status"], "completed");
        assert_eq!(report["text"], "fixture answer");
        assert_eq!(report["saved"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-secret"));
        let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(request.starts_with("POST /fixture HTTP/1.1"));
        assert!(request.contains(header), "{request}");
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["stream"], true);
        assert!(body.to_string().contains("user question"));
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
    }
}

#[test]
fn stdin_save_reload_preserves_materials_and_creates_fresh_work() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("materials.json");
    let (endpoint, requests) = server(200, chat(), Duration::ZERO);
    let mut child = command("openai-chat", &endpoint)
        .args(["--stdin", "--json", "--save"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"original input\n")
        .unwrap();
    let first = child.wait_with_output().unwrap();
    assert!(first.status.success(), "{first:?}");
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let original = tarz_core::material::load(&path).unwrap();
    assert_eq!(original.blocks().len(), 2);
    let bytes = std::fs::read_to_string(&path).unwrap();
    assert!(!bytes.contains("fixture-secret"));
    assert!(!bytes.contains("request_key"));
    let (endpoint, requests) = server(200, chat(), Duration::ZERO);
    let second = command("openai-chat", &endpoint)
        .args(["--input", "continue", "--json", "--load"])
        .arg(&path)
        .arg("--save")
        .arg(&path)
        .output()
        .unwrap();
    assert!(second.status.success(), "{second:?}");
    assert_ne!(report(&first)["work_id"], report(&second)["work_id"]);
    assert_ne!(
        report(&first)["conversation_id"],
        report(&second)["conversation_id"]
    );
    let restored = tarz_core::material::load(&path).unwrap();
    assert_eq!(&restored.blocks()[..2], original.blocks());
    assert_eq!(restored.blocks().len(), 4);
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(request.contains("original input") && request.contains("continue"));
}

#[test]
fn provider_failure_and_deadline_save_actual_input_with_distinct_exit_codes() {
    for (status, body, delay, exit, state) in [
        (
            401,
            json!({"error":"fixture-secret"}),
            Duration::ZERO,
            1,
            "failed",
        ),
        (200, chat(), Duration::from_secs(2), 124, "timed_out"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("material.json");
        let (endpoint, requests) = server(status, body, delay);
        let output = command("openai-chat", &endpoint)
            .args([
                "--input",
                "keep me",
                "--json",
                "--timeout-secs",
                "1",
                "--save",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(exit), "{output:?}");
        let report = report(&output);
        assert_eq!(report["status"], state);
        assert_eq!(report["saved"], true);
        assert_eq!(tarz_core::material::load(&path).unwrap().blocks().len(), 1);
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-secret"));
        requests.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

#[test]
fn save_failure_keeps_execution_result_and_reports_nonzero_exit() {
    let dir = tempfile::tempdir().unwrap();
    let (endpoint, requests) = server(200, chat(), Duration::ZERO);
    let output = command("openai-chat", &endpoint)
        .args(["--input", "hello", "--json", "--save"])
        .arg(dir.path().join("absent/material.json"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report = report(&output);
    assert_eq!(report["status"], "completed");
    assert_eq!(report["text"], "fixture answer");
    assert_eq!(report["saved"], false);
    assert_eq!(report["error"], "material save failed");
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
}

#[test]
fn malformed_options_and_missing_configuration_do_not_start_execution() {
    for args in [
        vec![],
        vec!["--input", "a", "--stdin"],
        vec!["--input", "a", "--json", "--json"],
        vec!["--input", "a", "--timeout-secs", "0"],
        vec!["--input", "a", "--timeout-secs", "86401"],
    ] {
        let output = command("openai-chat", "http://127.0.0.1:1")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
    }
    let output = command("openai-chat", "http://127.0.0.1:1")
        .env_remove("TARZ_TEST_KEY")
        .args(["--input", "a", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report(&output)["status"], "not_started");
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_http_work_and_saves_returned_materials() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancelled.json");
    let (endpoint, requests) = server(200, chat(), Duration::from_secs(2));
    let child = command("openai-chat", &endpoint)
        .args(["--input", "cancel me", "--json", "--save"])
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        Command::new("kill")
            .args(["-s", "INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert_eq!(report(&output)["status"], "cancelled");
    assert_eq!(report(&output)["saved"], true);
    assert_eq!(tarz_core::material::load(&path).unwrap().blocks().len(), 1);
}

#[test]
fn text_mode_prints_only_answer_and_invalid_material_never_starts_work() {
    let (endpoint, requests) = server(200, chat(), Duration::ZERO);
    let output = command("openai-chat", &endpoint)
        .args(["--input", "hello"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"fixture answer\n");
    assert!(output.stderr.is_empty());
    requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.json");
    std::fs::write(&path, b"not JSON").unwrap();
    let output = command("openai-chat", "http://127.0.0.1:1")
        .args(["--input", "hello", "--json", "--load"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(report(&output)["status"], "not_started");
    assert!(report(&output)["work_id"].is_null());
}

#[test]
fn anthropic_without_an_output_limit_is_rejected_before_any_request() {
    let (endpoint, requests) = server(200, json!({}), Duration::ZERO);
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    let output = command
        .args([
            "run",
            "--no-config",
            "--provider",
            "anthropic",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARZ_TEST_KEY",
            "--endpoint",
            &endpoint,
            "--input",
            "user question",
            "--json",
        ])
        .env("TARZ_TEST_KEY", "fixture-secret")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    let report = report(&output);
    assert_eq!(report["status"], "failed");
    assert!(
        report["error"].as_str().unwrap().contains("max_tokens"),
        "{report}"
    );
    assert!(requests.recv_timeout(Duration::from_millis(300)).is_err());
}

#[test]
fn the_configured_output_limit_and_effort_reach_the_request() {
    let (endpoint, requests) = server(
        200,
        json!({"content":[{"type":"text","text":"fixture answer"}],"stop_reason":"end_turn"}),
        Duration::ZERO,
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args([
            "run",
            "--no-config",
            "--provider",
            "anthropic",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARZ_TEST_KEY",
            "--endpoint",
            &endpoint,
            "--max-tokens",
            "2048",
            "--reasoning-effort",
            "low",
            "--input",
            "user question",
            "--json",
        ])
        .env("TARZ_TEST_KEY", "fixture-secret")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let request = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["max_tokens"], 2048);
    assert_eq!(body["output_config"]["effort"], "low");
}

#[cfg(target_os = "macos")]
#[test]
fn command_sandbox_status_is_reported_on_stderr_but_not_in_json() {
    let workspace = tempfile::tempdir().unwrap();
    let (endpoint, _requests) = server(200, chat(), Duration::ZERO);
    let output = command("openai-chat", &endpoint)
        .args([
            "--tools",
            "run_command",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--input",
            "user question",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Tarz: command sandbox: seatbelt: descendant lifecycle partial ("),
        "{stderr}"
    );
    assert!(!stderr.contains("file read full"), "{stderr}");
    let run_report = report(&output);
    assert_eq!(run_report["status"], "completed");
    let keys: Vec<&str> = run_report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        keys.iter()
            .all(|key| !key.contains("command") && !key.contains("sandbox")),
        "{keys:?}"
    );
}

#[test]
fn mcp_servers_that_fail_to_start_are_reported_on_stderr_but_not_in_json() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config.toml");
    std::fs::write(
        &config,
        "version = 1\nstore = 'store'\nrecovery_dir = '.'\n",
    )
    .unwrap();
    let mcp = directory.path().join("mcp.json");
    std::fs::write(
        &mcp,
        r#"{"mcpServers": {"absent": {"command": "/nonexistent/tarz-mcp-absent"}, "needed": {"command": "/nonexistent/tarz-mcp-needed", "required": true}}}"#,
    )
    .unwrap();
    let (endpoint, _requests) = server(200, chat(), Duration::ZERO);
    let output = command("openai-chat", &endpoint)
        .env_remove("TARZ_CONFIG")
        .env("XDG_CONFIG_HOME", directory.path())
        .arg("--config")
        .arg(&config)
        .args(["--mcp", "absent", "--input", "user question", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Tarz: MCP server 'absent' is unavailable: "),
        "{stderr}"
    );
    let run_report = report(&output);
    assert_eq!(run_report["status"], "completed");
    let keys: Vec<&str> = run_report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(keys.iter().all(|key| !key.contains("mcp")), "{keys:?}");

    // Creating saves the definition without starting servers, so even a
    // required server that cannot start does not block it.
    let created = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .env_remove("TARZ_CONFIG")
        .env("XDG_CONFIG_HOME", directory.path())
        .env("TARZ_TEST_KEY", "fixture-secret")
        .current_dir(directory.path())
        .args(["session", "create", "--config"])
        .arg(&config)
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARZ_TEST_KEY",
            "--mcp",
            "needed",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    assert!(created.stderr.is_empty(), "{created:?}");
    assert_eq!(report(&created)["status"], "created");
}
