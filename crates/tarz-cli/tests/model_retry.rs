#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Model retries through the real CLI process against a scripted local
//! server: a rate-limited later round is repeated with the same request,
//! and an exhausted attempt limit ends as a failure that keeps the earlier
//! materials usable for new work.
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
