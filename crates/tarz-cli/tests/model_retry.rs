#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Model retries through the real CLI against a scripted local server: a
//! rate-limited later round repeats the same request, and an exhausted attempt
//! limit keeps earlier materials usable. Generation settings are checked too.
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;
// Only the wire-message builders are used; the fixed three-round server is not.
#[allow(dead_code)]
#[path = "common/tools.rs"]
mod tools;

enum Reply {
    Stream(String),
    RateLimited,
    /// A non-success status with a provider error envelope.
    Refused(&'static str, Value),
    /// A success status whose body is not the event stream that was asked for.
    Unreadable,
}

/// Serves one scripted reply per connection and records each request body.
fn serve(provider: &str, script: Vec<Reply>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let path = match provider {
        "openai-chat" => "/v1/chat/completions",
        "openai-responses" => "/v1/responses",
        _ => "/v1/messages",
    };
    let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    std::thread::spawn(move || {
        for reply in script {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut chunk = [0; 4096];
                let Ok(count) = socket.read(&mut chunk) else {
                    return;
                };
                if count == 0 {
                    return;
                }
                bytes.extend_from_slice(&chunk[..count]);
                let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break bytes[end + 4..end + 4 + length].to_vec();
                }
            };
            captured
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            let (status, kind, body) = match reply {
                Reply::Stream(body) => ("200 OK", "text/event-stream", body),
                Reply::RateLimited => (
                    "429 Too Many Requests",
                    "application/json",
                    json!({"error":{"message":"slow down"}}).to_string(),
                ),
                Reply::Refused(status, envelope) => {
                    (status, "application/json", envelope.to_string())
                }
                Reply::Unreadable => ("200 OK", "application/json", "{}".to_owned()),
            };
            let _ = write!(
                socket,
                "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (endpoint, requests)
}

fn two_tool_rounds(provider: &str) -> Vec<Reply> {
    vec![
        Reply::Stream(tools::tool(
            provider,
            "search_text",
            json!({"path":".","query":"retry original","max_matches":10}),
            "search1",
        )),
        Reply::Stream(tools::tool(
            provider,
            "read_file",
            json!({"path":"recovery.rs","start_line":2,"max_lines":1}),
            "read1",
        )),
    ]
}

fn run(provider: &str, endpoint: &str, workspace: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    command.args([
        "run",
        "--no-config",
        "--provider",
        provider,
        "--model",
        "fixture-model",
        "--api-key-env",
        "PATH",
        "--endpoint",
        endpoint,
        "--tools",
        "list_directory,read_file,search_text",
        "--input",
        "Find recovery path",
        "--json",
        "--workspace",
    ]);
    command.arg(workspace).args(extra);
    if provider == "anthropic" {
        command.args(["--max-tokens", "1024"]);
    }
    command.output().unwrap()
}

fn workspace(dir: &Path) -> std::path::PathBuf {
    let workspace = dir.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("recovery.rs"),
        "header\nretry original material\nfooter\n",
    )
    .unwrap();
    workspace
}

fn tool_results(material: &Path) -> usize {
    tarz_core::material::load(material)
        .unwrap()
        .blocks()
        .iter()
        .filter(|b| matches!(b.content(), causa::kernel::BlockContent::ToolResult(_)))
        .count()
}

#[test]
fn a_rate_limited_third_round_is_repeated_with_the_same_request_and_completes() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        let mut script = two_tool_rounds(provider);
        script.extend([
            Reply::RateLimited,
            Reply::RateLimited,
            Reply::Stream(tools::answer(provider)),
        ]);
        let (endpoint, requests) = serve(provider, script);
        let material = dir.path().join("material.json");
        let output = run(
            provider,
            &endpoint,
            &workspace,
            &[
                "--max-model-attempts",
                "3",
                "--save",
                material.to_str().unwrap(),
            ],
        );
        assert!(output.status.success(), "{provider}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "completed");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5, "{provider}");
        // The two retries resend the third round unchanged.
        assert_eq!(requests[2], requests[3], "{provider}");
        assert_eq!(requests[3], requests[4], "{provider}");
        // Each tool ran once: the retries added no result.
        assert_eq!(tool_results(&material), 2, "{provider}");
    }
}

#[test]
fn an_exhausted_attempt_limit_fails_with_earlier_materials_kept_for_new_work() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = workspace(dir.path());
        let mut script = two_tool_rounds(provider);
        script.extend([Reply::RateLimited, Reply::RateLimited]);
        let (endpoint, requests) = serve(provider, script);
        let material = dir.path().join("material.json");
        let output = run(
            provider,
            &endpoint,
            &workspace,
            &[
                "--max-model-attempts",
                "2",
                "--save",
                material.to_str().unwrap(),
            ],
        );
        assert!(!output.status.success(), "{provider}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "failed", "{provider}");
        // The failure names its kind and carries the gateway's own message.
        assert_eq!(report["error_kind"], "transient", "{provider}");
        let detail = report["error_detail"].as_str().unwrap();
        assert!(
            detail.ends_with("error 429: slow down"),
            "{provider}: {detail}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("temporary provider failure")
                && stderr.contains(&format!("Tarz: detail: {detail}")),
            "{provider}: {stderr}"
        );
        assert_eq!(requests.lock().unwrap().len(), 4, "{provider}");
        assert_eq!(tool_results(&material), 2, "{provider}");

        // New work over the saved materials completes without redoing tools.
        let (endpoint, requests) = serve(provider, vec![Reply::Stream(tools::answer(provider))]);
        let output = run(
            provider,
            &endpoint,
            &workspace,
            &["--load", material.to_str().unwrap()],
        );
        assert!(output.status.success(), "{provider}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "completed", "{provider}");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{provider}");
        assert!(
            requests[0].to_string().contains("retry original material"),
            "{provider}"
        );
    }
}

#[test]
fn without_a_configured_limit_a_rate_limit_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    let (endpoint, requests) = serve("openai-chat", vec![Reply::RateLimited]);
    let output = run("openai-chat", &endpoint, &workspace, &[]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn a_refusal_and_an_unreadable_response_are_reported_as_different_causes() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    for (reply, kind, hint) in [
        (
            Reply::Refused(
                "401 Unauthorized",
                json!({"error":{"type":"authentication_error","message":"bad\r\nkey"}}),
            ),
            "credential_rejected",
            "check the API key",
        ),
        (
            Reply::Refused(
                "404 Not Found",
                json!({"error":{"code":"model_not_found","message":"no such model"}}),
            ),
            "not_found",
            "check the model name and the endpoint URL",
        ),
        (Reply::Unreadable, "undecodable", "could not be read"),
    ] {
        let (endpoint, requests) = serve("openai-chat", vec![reply]);
        let output = run("openai-chat", &endpoint, &workspace, &[]);
        assert_eq!(output.status.code(), Some(1), "{kind}: {output:?}");
        assert_eq!(requests.lock().unwrap().len(), 1, "{kind}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["status"], "failed", "{kind}");
        assert_eq!(report["error_kind"], kind);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(hint), "{kind}: {stderr}");
        let detail = report["error_detail"].as_str().unwrap();
        assert!(!detail.chars().any(char::is_control), "{kind}: {detail:?}");
        assert!(
            stderr.contains(&format!("Tarz: detail: {detail}")),
            "{kind}: {stderr}"
        );
    }
}

#[test]
fn effort_text_and_extra_request_fields_reach_the_provider_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    let extra = r#"{"thinking":{"type":"adaptive","display":"summarized"},"top_k":40}"#;
    for (provider, effort_at) in [
        ("openai-chat", "/reasoning_effort"),
        ("openai-responses", "/reasoning/effort"),
        ("anthropic", "/output_config/effort"),
    ] {
        let (endpoint, requests) = serve(provider, vec![Reply::Stream(tools::answer(provider))]);
        // `off` is on no fixed list of levels; it goes out unchanged.
        let output = run(
            provider,
            &endpoint,
            &workspace,
            &["--reasoning-effort", "off", "--extra-body", extra],
        );
        assert!(output.status.success(), "{provider}: {output:?}");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "{provider}");
        let body = &requests[0];
        assert_eq!(body.pointer(effort_at), Some(&json!("off")), "{provider}");
        assert_eq!(
            body["thinking"],
            json!({"type": "adaptive", "display": "summarized"}),
            "{provider}"
        );
        assert_eq!(body["top_k"], 40, "{provider}");
    }
    // Anything but a JSON object is refused before a request is made.
    let (endpoint, requests) = serve("openai-chat", Vec::new());
    let output = run(
        "openai-chat",
        &endpoint,
        &workspace,
        &["--extra-body", "[1]"],
    );
    assert!(!output.status.success());
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn render_options_reach_the_gateway_and_unfit_ones_are_refused_before_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = workspace(dir.path());
    let chat_body = |extra: &[&str]| {
        let (endpoint, requests) = serve(
            "openai-chat",
            vec![Reply::Stream(tools::answer("openai-chat"))],
        );
        let mut args = vec!["--max-tokens", "300"];
        args.extend_from_slice(extra);
        let output = run("openai-chat", &endpoint, &workspace, &args);
        assert!(output.status.success(), "{output:?}");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        requests[0].clone()
    };
    // The default writes the limit under the newer name only.
    let body = chat_body(&[]);
    assert_eq!(body["max_completion_tokens"], 300);
    assert!(body.get("max_tokens").is_none());
    // The chosen field is the only one sent.
    let body = chat_body(&["--render-options", r#"{"max_tokens_field":"max_tokens"}"#]);
    assert_eq!(body["max_tokens"], 300);
    assert!(body.get("max_completion_tokens").is_none());

    // The same table from a TOML profile.
    let config = dir.path().join("config.toml");
    let (endpoint, requests) = serve(
        "openai-chat",
        vec![Reply::Stream(tools::answer("openai-chat"))],
    );
    std::fs::write(
        &config,
        format!(
            "version = 1\ndefault_profile = \"p\"\n[profiles.p]\nprovider = \"openai-chat\"\nmodel = \"fixture-model\"\napi_key_env = \"PATH\"\nendpoint = \"{endpoint}\"\nmax_tokens = 300\n[profiles.p.render_options]\nmax_tokens_field = \"max_tokens\"\n"
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args(["run", "--config"])
        .arg(&config)
        .args(["--input", "Find recovery path", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let body = requests.lock().unwrap()[0].clone();
    assert_eq!(body["max_tokens"], 300);
    assert!(body.get("max_completion_tokens").is_none());

    // Another protocol's option takes effect too: Responses leaves out the
    // encrypted-reasoning include when told to.
    let responses_body = |extra: &[&str]| {
        let (endpoint, requests) = serve(
            "openai-responses",
            vec![Reply::Stream(tools::answer("openai-responses"))],
        );
        let output = run("openai-responses", &endpoint, &workspace, extra);
        assert!(output.status.success(), "{output:?}");
        requests.lock().unwrap()[0].clone()
    };
    assert_eq!(
        responses_body(&[])["include"],
        json!(["reasoning.encrypted_content"])
    );
    let body = responses_body(&[
        "--render-options",
        r#"{"include_encrypted_reasoning":false}"#,
    ]);
    assert!(body.get("include").is_none());

    // A Chat option on another protocol, an unknown value and a non-object
    // are refused before any request.
    for (provider, table) in [
        ("anthropic", r#"{"max_tokens_field":"max_tokens"}"#),
        ("openai-chat", r#"{"max_tokens_field":"secret_marker"}"#),
        ("openai-chat", "[1]"),
    ] {
        let (endpoint, requests) = serve(provider, Vec::new());
        let output = run(
            provider,
            &endpoint,
            &workspace,
            &["--render-options", table],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{provider} {table}: {output:?}"
        );
        assert!(requests.lock().unwrap().is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret_marker"));
    }
}
