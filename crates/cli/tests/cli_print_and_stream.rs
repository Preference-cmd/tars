//! Wire-level tests for `tars -p`, `--stream`, `--stream-json`, `--output-format`,
//! precise CLI errors, and per-subcommand `--help`.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[allow(dead_code)]
#[path = "common/tools.rs"]
mod tools;

enum MockReply {
    Sse(String),
    TruncatedSse(String),
}

fn serve(provider: &str, replies: Vec<MockReply>) -> (String, Arc<Mutex<Vec<Value>>>) {
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
        for reply in replies {
            let Ok(mut socket) = tools::accept_post(&listener) else {
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
            match reply {
                MockReply::Sse(body) => {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
                MockReply::TruncatedSse(body) => {
                    // Omit Content-Length and drop connection before `[DONE]` so
                    // StreamRetryGateway triggers a mid-stream retry.
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
                    );
                }
            }
        }
    });
    (endpoint, requests)
}

fn chat_chunked_sse(chunks: &[&str]) -> String {
    let mut sse = String::new();
    for chunk in chunks {
        sse.push_str(&format!(
            "data: {}\n\n",
            json!({"id":"chat1","choices":[{"index":0,"delta":{"content":chunk},"finish_reason":null}]})
        ));
    }
    sse.push_str(&format!(
        "data: {}\n\n",
        json!({"id":"chat1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    ));
    sse.push_str("data: [DONE]\n\n");
    sse
}

fn chat_text_then_tool_sse(text: &str, tool_name: &str, args: Value, id: &str) -> String {
    let arguments = args.to_string();
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id":"chat1","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"id":"chat1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":tool_name,"arguments":arguments}}]},"finish_reason":null}]}),
        json!({"id":"chat1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    )
}

fn tars_cmd(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env("TARS_HOME", home.join("tars-home"))
        .env("TARS_TEST_KEY", "fixture-secret");
    command
}

fn minimal_png() -> Vec<u8> {
    let mut bytes = vec![0x2a; 33];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes
}

fn run_with_stdin(mut cmd: Command, stdin_bytes: &[u8]) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin_bytes).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn ce01_print_positional_profile_double_dash_image_and_piped_stdin_approval_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    // 1. `tars -p "hello"` transient execution.
    let (endpoint, requests) = serve(
        "openai-chat",
        vec![MockReply::Sse(tools::answer_with_text(
            "openai-chat",
            "transient ok",
        ))],
    );
    let mut cmd = tars_cmd(dir.path());
    cmd.args([
        "-p",
        "hello",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        &endpoint,
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "transient ok\n");
    assert!(requests.lock().unwrap()[0].to_string().contains("hello"));

    // 2. `tars --profile fast -p "via profile"` + `tars -p -- -leading-dash`.
    let (endpoint2, requests2) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(tools::answer_with_text("openai-chat", "profile ok")),
            MockReply::Sse(tools::answer_with_text("openai-chat", "dash ok")),
        ],
    );
    let cfg = dir.path().join("tars.toml");
    std::fs::write(
        &cfg,
        format!(
            "version = 1\n[profiles.fast]\nprovider = \"openai-chat\"\nmodel = \"fast-model\"\napi_key_env = \"TARS_TEST_KEY\"\nendpoint = \"{endpoint2}\"\n"
        ),
    )
    .unwrap();

    let mut cmd = tars_cmd(dir.path());
    cmd.args(["--config"])
        .arg(&cfg)
        .args(["--profile", "fast", "-p", "via profile"]);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "profile ok\n");

    let mut cmd = tars_cmd(dir.path());
    cmd.args(["--config"])
        .arg(&cfg)
        .args(["--profile", "fast", "-p", "--", "-leading-dash"]);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "dash ok\n");
    let captured2 = requests2.lock().unwrap();
    assert!(captured2[0].to_string().contains("via profile"));
    assert!(captured2[1].to_string().contains("-leading-dash"));

    // 3. `tars -p --image <png>` without text prompt.
    let img_path = dir.path().join("shot.png");
    std::fs::write(&img_path, minimal_png()).unwrap();
    let (endpoint_img, _) = serve(
        "openai-chat",
        vec![MockReply::Sse(tools::answer_with_text(
            "openai-chat",
            "image ok",
        ))],
    );
    let mut cmd = tars_cmd(dir.path());
    cmd.args([
        "-p",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        &endpoint_img,
        "--image",
    ])
    .arg(&img_path);
    let out = run_with_stdin(cmd, b"");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "image ok\n");

    // 4. Piped stdin alone (`echo "hello" | tars -p`) & piped stdin + positional (`printf "diff" | tars -p "review"`).
    let (endpoint3, requests3) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(tools::answer_with_text("openai-chat", "stdin only ok")),
            MockReply::Sse(tools::answer_with_text("openai-chat", "combined ok")),
        ],
    );
    let mut cmd = tars_cmd(dir.path());
    cmd.args([
        "-p",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        &endpoint3,
    ]);
    let out = run_with_stdin(cmd, b"hello from pipe\n");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "stdin only ok\n");

    let mut cmd = tars_cmd(dir.path());
    cmd.args([
        "-p",
        "review",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        &endpoint3,
    ]);
    let out = run_with_stdin(cmd, b"diff");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "combined ok\n");
    let captured3 = requests3.lock().unwrap();
    assert!(captured3[0].to_string().contains("hello from pipe"));
    assert_eq!(
        captured3[1]["messages"].as_array().unwrap().last().unwrap()["content"],
        "review\n\ndiff"
    );

    // 5. Piped stdin + write_file in `ask` mode MUST NOT hang on approval; rejects non-interactively.
    let (endpoint4, requests4) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(tools::tool(
                "openai-chat",
                "write_file",
                json!({"path":"note.txt","content":"hi"}),
                "w1",
            )),
            MockReply::Sse(tools::answer_with_text(
                "openai-chat",
                "approval rejected safely",
            )),
        ],
    );
    let mut cmd = tars_cmd(dir.path());
    cmd.args([
        "-p",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        &endpoint4,
        "--workspace",
    ])
    .arg(&workspace)
    .args(["--tools", "write_file", "--permission-mode", "ask"]);
    let out = run_with_stdin(cmd, b"write a file");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "approval rejected safely\n"
    );
    assert!(!workspace.join("note.txt").exists());
    assert_eq!(requests4.lock().unwrap().len(), 2);
}

#[test]
fn ce01_counterexamples_exit_2_and_bare_non_tty_still_prints_help() {
    let dir = tempfile::tempdir().unwrap();

    // Bare non-TTY `tars` still prints help and exits 0.
    let bare = tars_cmd(dir.path()).output().unwrap();
    assert!(bare.status.success(), "{bare:?}");
    assert!(String::from_utf8_lossy(&bare.stdout).contains("Usage: tars [interactive options]"));

    // `tars demo --save -p` must NOT trigger print mode.
    let demo_save_p = tars_cmd(dir.path())
        .args(["demo", "--save", "-p"])
        .output()
        .unwrap();
    assert_eq!(demo_save_p.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&demo_save_p.stderr).contains("Unsupported arguments"));

    // Empty piped stdin with no positional prompt and no `--image` exits 2.
    let mut empty_pipe = tars_cmd(dir.path());
    empty_pipe.args([
        "-p",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "m",
        "--api-key-env",
        "TARS_TEST_KEY",
    ]);
    let out = run_with_stdin(empty_pipe, b"   \n");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("input must not be blank"));

    for (args, expected) in [
        (vec!["-p", "one", "two"], "unexpected positional argument"),
        (
            vec!["-p", "one", "--input", "two"],
            "positional prompt and --input are mutually exclusive",
        ),
        (
            vec!["-p", "one", "--stdin"],
            "positional prompt and --stdin are mutually exclusive",
        ),
    ] {
        let out = tars_cmd(dir.path()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }
}

#[test]
fn ce02_print_continue_and_resume_persist_turns_and_enforce_mode_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // Mode isolation counterexamples (exit 2):
    for (args, expected) in [
        (
            vec!["-p", "-c", "--resume", "abc"],
            "-c and --resume are mutually exclusive",
        ),
        (
            vec!["-p", "--no-config", "-c", "hi"],
            "--store is required with --no-config",
        ),
        (
            vec!["-p", "--no-config", "--resume", "abc", "hi"],
            "--store is required with --no-config",
        ),
        (
            vec![
                "-p",
                "-c",
                "--store",
                store.to_str().unwrap(),
                "--provider",
                "openai-chat",
                "hi",
            ],
            "option --provider cannot be used with --continue/--resume",
        ),
        (
            vec![
                "-p",
                "--resume",
                "abc",
                "--store",
                store.to_str().unwrap(),
                "--model",
                "m",
                "hi",
            ],
            "option --model cannot be used with --continue/--resume",
        ),
        (
            vec![
                "-p",
                "--resume",
                "abc",
                "--store",
                store.to_str().unwrap(),
                "--tools",
                "read_file",
                "hi",
            ],
            "option --tools cannot be used with --continue/--resume",
        ),
        (
            vec![
                "-p",
                "--resume",
                "abc",
                "--store",
                store.to_str().unwrap(),
                "--workspace",
                ".",
                "hi",
            ],
            "option --workspace cannot be used with --continue/--resume",
        ),
        (
            vec![
                "-p",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--store",
                store.to_str().unwrap(),
                "hi",
            ],
            "option --store can only be used with --continue or --resume",
        ),
        (
            vec![
                "-p",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--trust-workspace-rules",
                "hi",
            ],
            "option --trust-workspace-rules can only be used with --continue or --resume",
        ),
    ] {
        let out = tars_cmd(dir.path()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }

    // Create a persistent session in default $TARS_HOME/data and test `-p -c` and `-p --resume <prefix>`.
    let (endpoint, requests) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(tools::answer_with_text("openai-chat", "turn 1 reply")),
            MockReply::Sse(tools::answer_with_text("openai-chat", "turn 2 reply")),
        ],
    );
    let default_store = dir.path().join("tars-home").join("data");
    let created = tars_cmd(dir.path())
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            default_store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint,
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let conv_id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    let prefix = &conv_id[..8];

    // `-p -c` resolves default $TARS_HOME/data without `--store` when config is enabled.
    let cont = tars_cmd(dir.path())
        .args(["-p", "-c", "first follow up"])
        .output()
        .unwrap();
    assert!(cont.status.success(), "{cont:?}");
    assert_eq!(String::from_utf8_lossy(&cont.stdout), "turn 1 reply\n");

    // `-p --resume <8-char-prefix>` continues the same session with explicit `--store`.
    let resumed = tars_cmd(dir.path())
        .args([
            "-p",
            "--no-config",
            "--store",
            default_store.to_str().unwrap(),
            "--resume",
            prefix,
            "second follow up",
        ])
        .output()
        .unwrap();
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(String::from_utf8_lossy(&resumed.stdout), "turn 2 reply\n");

    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[1].to_string().contains("first follow up"));
    assert!(reqs[1].to_string().contains("second follow up"));

    // Verify both turns persisted in SQLite.
    let hist = tars_cmd(dir.path())
        .args([
            "session",
            "history",
            &conv_id,
            "--no-config",
            "--store",
            default_store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(hist.status.success(), "{hist:?}");
    let hist_text = String::from_utf8_lossy(&hist.stdout);
    assert!(hist_text.contains("first follow up"));
    assert!(hist_text.contains("turn 1 reply"));
    assert!(hist_text.contains("second follow up"));
    assert!(hist_text.contains("turn 2 reply"));

    // Missing session with `--stream-json` emits `not_started` NDJSON result and exits 1.
    let missing = tars_cmd(dir.path())
        .args([
            "-p",
            "--no-config",
            "--store",
            default_store.to_str().unwrap(),
            "--resume",
            "ffffffff",
            "--stream-json",
            "hi",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    let line: Value =
        serde_json::from_str(String::from_utf8_lossy(&missing.stdout).trim()).unwrap();
    assert_eq!(line["type"], "result");
    assert_eq!(line["status"], "not_started");
    assert!(line["error"].as_str().unwrap().contains("not found"));
}

#[test]
fn ce03_stream_text_multi_round_retry_and_zero_delta_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("note.txt"), "hello world\n").unwrap();

    // 1. Multi-chunk + multi-round tool loop on `tars -p --stream`.
    let (endpoint, _) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(chat_text_then_tool_sse(
                "Checking file.",
                "read_file",
                json!({"path":"note.txt","start_line":1,"max_lines":1}),
                "r1",
            )),
            MockReply::Sse(chat_chunked_sse(&["Done", " reading."])),
        ],
    );
    let out = tars_cmd(dir.path())
        .args([
            "-p",
            "read note",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint,
            "--workspace",
        ])
        .arg(&workspace)
        .args(["--tools", "read_file", "--stream"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Checking file.\n\nDone reading.\n"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TARS: tool read_file: running"), "{stderr}");
    assert!(
        stderr.contains("TARS: tool read_file: committed"),
        "{stderr}"
    );

    // 2. Pre-Done stream retry on `tars run --stream --max-model-attempts 2`.
    let truncated_sse = format!(
        "data: {}\n\n",
        json!({"id":"chat1","choices":[{"index":0,"delta":{"content":"partial"},"finish_reason":null}]})
    );
    let (endpoint_retry, _) = serve(
        "openai-chat",
        vec![
            MockReply::TruncatedSse(truncated_sse),
            MockReply::Sse(chat_chunked_sse(&["recovered"])),
        ],
    );
    let out_retry = tars_cmd(dir.path())
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
            &endpoint_retry,
            "--max-model-attempts",
            "2",
            "--input",
            "retry test",
            "--stream",
        ])
        .output()
        .unwrap();
    assert!(out_retry.status.success(), "{out_retry:?}");
    assert_eq!(
        String::from_utf8_lossy(&out_retry.stdout),
        "partial\n\nrecovered\n"
    );
    let retry_err = String::from_utf8_lossy(&out_retry.stderr);
    assert!(
        retry_err.contains("TARS: model stream retrying; resetting current round preview"),
        "{retry_err}"
    );

    // 3. Zero-delta fallback on `tars session run --stream` (openai-responses without text delta events).
    let zero_delta_sse = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"m1","type":"message","role":"assistant","content":[]}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback text"}]}}),
        json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[{"id":"m1","type":"message","role":"assistant","content":[{"type":"output_text","text":"fallback text"}]}]}}),
    );
    let (endpoint_zd, _) = serve("openai-responses", vec![MockReply::Sse(zero_delta_sse)]);
    let store = dir.path().join("store-zd");
    let created = tars_cmd(dir.path())
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-responses",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint_zd,
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let conv_id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    let out_zd = tars_cmd(dir.path())
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "hi",
            "--stream",
        ])
        .output()
        .unwrap();
    assert!(out_zd.status.success(), "{out_zd:?}");
    assert_eq!(String::from_utf8_lossy(&out_zd.stdout), "fallback text\n");
}

#[test]
fn ce04_stream_json_ndjson_format_conflicts_and_json_backward_compat() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.txt"), "content\n").unwrap();

    // 1. Format conflicts exit 2 naming offending flags across `-p`, `run`, `session run`.
    for (args, expected_parts) in [
        (
            vec!["-p", "hi", "--json", "--stream"],
            vec!["--json", "--stream"],
        ),
        (
            vec!["-p", "hi", "--stream", "--stream-json"],
            vec!["--stream", "--stream-json"],
        ),
        (
            vec!["-p", "hi", "--output-format", "json", "--stream"],
            vec!["--output-format json", "--stream"],
        ),
        (
            vec!["-p", "hi", "--output-format", "xml"],
            vec!["invalid --output-format 'xml'"],
        ),
        (
            vec![
                "run",
                "--no-config",
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--input",
                "hi",
                "--json",
                "--stream-json",
            ],
            vec!["--json", "--stream-json"],
        ),
        (
            vec![
                "session",
                "run",
                "00000000-0000-0000-0000-000000000000",
                "--no-config",
                "--store",
                "/tmp/s",
                "--input",
                "hi",
                "--stream",
                "--output-format",
                "text",
            ],
            vec!["--stream", "--output-format text"],
        ),
    ] {
        let out = tars_cmd(dir.path()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        for part in expected_parts {
            assert!(
                stderr.contains(part),
                "{args:?}: '{part}' not in '{stderr}'"
            );
        }
    }

    // 2. `--stream-json` on `tars -p` with tool call + `--output-format stream-json` on `tars run` + `tars session run`.
    let (endpoint, _) = serve(
        "openai-chat",
        vec![
            MockReply::Sse(chat_text_then_tool_sse(
                "Reading.",
                "read_file",
                json!({"path":"a.txt","start_line":1,"max_lines":1}),
                "r1",
            )),
            MockReply::Sse(chat_chunked_sse(&["Done."])),
            MockReply::Sse(chat_chunked_sse(&["Run ok."])),
            MockReply::Sse(chat_chunked_sse(&["Session ok."])),
            MockReply::Sse(chat_chunked_sse(&["Legacy json ok."])),
        ],
    );

    let out_p = tars_cmd(dir.path())
        .args([
            "-p",
            "inspect",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint,
            "--workspace",
        ])
        .arg(&workspace)
        .args(["--tools", "read_file", "--stream-json"])
        .output()
        .unwrap();
    assert!(out_p.status.success(), "{out_p:?}");
    let p_events: Vec<Value> = String::from_utf8(out_p.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(p_events.iter().any(|e| e["type"] == "model_started"));
    assert!(p_events.iter().any(|e| e["type"] == "text_delta"));
    assert!(p_events.iter().any(|e| e["type"] == "tool_progress"));
    let terminal = p_events.last().unwrap();
    assert_eq!(terminal["type"], "result");
    assert_eq!(terminal["status"], "completed");
    assert_eq!(terminal["kind"], "completed");
    assert_eq!(terminal["text"], "Done.");
    assert!(terminal["conversation_id"].is_string());
    assert!(terminal["work_id"].is_string());

    // `tars run --output-format stream-json`
    let out_run = tars_cmd(dir.path())
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
            &endpoint,
            "--input",
            "hi",
            "--output-format",
            "stream-json",
        ])
        .output()
        .unwrap();
    assert!(out_run.status.success(), "{out_run:?}");
    let run_events: Vec<Value> = String::from_utf8(out_run.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(run_events.last().unwrap()["type"], "result");
    assert_eq!(run_events.last().unwrap()["text"], "Run ok.");

    // `tars session run --stream-json`
    let store = dir.path().join("store-sj");
    let created = tars_cmd(dir.path())
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint,
        ])
        .output()
        .unwrap();
    let conv_id = String::from_utf8(created.stdout).unwrap().trim().to_owned();
    let out_srun = tars_cmd(dir.path())
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "hi",
            "--stream-json",
        ])
        .output()
        .unwrap();
    assert!(out_srun.status.success(), "{out_srun:?}");
    let srun_events: Vec<Value> = String::from_utf8(out_srun.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let s_term = srun_events.last().unwrap();
    assert_eq!(s_term["type"], "result");
    assert_eq!(s_term["text"], "Session ok.");
    assert_eq!(s_term["saved"], true);
    assert!(s_term["revision"].is_number());

    // 3. Existing `--json` single-object output has NO `"type"` field.
    let out_json = tars_cmd(dir.path())
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
            &endpoint,
            "--input",
            "hi",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out_json.status.success(), "{out_json:?}");
    let single: Value = serde_json::from_slice(&out_json.stdout).unwrap();
    assert!(single.get("type").is_none(), "{single}");
    assert_eq!(single["status"], "completed");
    assert_eq!(single["text"], "Legacy json ok.");

    // 4. Early failure in `--stream-json` mode emits NDJSON `result` line.
    let early = tars_cmd(dir.path())
        .args([
            "-p",
            "hi",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--image",
            "/nonexistent/path/image.png",
            "--stream-json",
        ])
        .output()
        .unwrap();
    assert_eq!(early.status.code(), Some(1), "{early:?}");
    let early_json: Value =
        serde_json::from_str(String::from_utf8_lossy(&early.stdout).trim()).unwrap();
    assert_eq!(early_json["type"], "result");
    assert_eq!(early_json["status"], "not_started");
    assert!(
        early_json["error"]
            .as_str()
            .unwrap()
            .contains("invalid image")
    );
}

#[test]
fn ce09_precise_error_messages_and_per_subcommand_help() {
    let dir = tempfile::tempdir().unwrap();

    // Precise error messages across run, -p, session, tools:
    for (args, expected) in [
        (vec!["run", "--bogus"], "unknown option: --bogus"),
        (
            vec!["run", "--stdin", "--stdin"],
            "duplicate option: --stdin",
        ),
        (
            vec!["run", "--provider"],
            "option --provider requires a value",
        ),
        (
            vec!["run", "--provider", ""],
            "option --provider must not be empty",
        ),
        (vec!["-p", "--model"], "option --model requires a value"),
        (
            vec!["-p", "hi", "--model", "a", "--model", "b"],
            "duplicate option: --model",
        ),
        (
            vec!["session", "list", "--mcp", "git"],
            "option --mcp is not applicable to session list",
        ),
        (
            vec!["session", "list", "--bogus"],
            "unknown session option: --bogus",
        ),
        (
            vec!["session", "list", "--store"],
            "option --store requires a value",
        ),
        (
            vec!["session", "list", "--store", ""],
            "option --store must not be empty",
        ),
        (
            vec!["session", "list", "--json", "--json"],
            "duplicate option: --json",
        ),
        (
            vec!["tools", "status", "--yes"],
            "option --yes is not applicable to tools status",
        ),
        (
            vec!["tools", "install", "rg", "--json"],
            "option --json is not applicable to tools install",
        ),
        (
            vec!["tools", "status", "--bogus"],
            "unknown option: --bogus",
        ),
    ] {
        let out = tars_cmd(dir.path()).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected),
            "{args:?}: expected '{expected}' in '{stderr}'"
        );
    }

    // Per-subcommand `--help` outputs are concise and specific:
    let p_help = tars_cmd(dir.path())
        .args(["-p", "--help"])
        .output()
        .unwrap();
    assert!(p_help.status.success());
    let p_text = String::from_utf8(p_help.stdout).unwrap();
    assert!(p_text.contains("Usage: tars -p [prompt] [options]"));
    assert!(p_text.contains("--resume"));
    assert!(!p_text.contains("tars tools status"));

    let run_help = tars_cmd(dir.path())
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(run_help.status.success());
    let run_text = String::from_utf8(run_help.stdout).unwrap();
    assert!(run_text.contains("Usage: tars run --provider <protocol>"));
    assert!(run_text.contains("--stream-json"));
    assert!(!run_text.contains("tars session create"));

    let srun_help = tars_cmd(dir.path())
        .args(["session", "run", "--help"])
        .output()
        .unwrap();
    assert!(srun_help.status.success());
    let srun_text = String::from_utf8(srun_help.stdout).unwrap();
    assert!(srun_text.contains("Usage: tars session run <conversation-id>"));
    assert!(srun_text.contains("--stream-json"));
    assert!(!srun_text.contains("--expected-revision"));

    let sfork_help = tars_cmd(dir.path())
        .args(["session", "fork", "--help"])
        .output()
        .unwrap();
    assert!(sfork_help.status.success());
    let sfork_text = String::from_utf8(sfork_help.stdout).unwrap();
    assert!(sfork_text.contains("Usage: tars session fork <conversation-id>"));
    assert!(sfork_text.contains("--title"));
    assert!(!sfork_text.contains("--expected-revision"));

    let tstatus_help = tars_cmd(dir.path())
        .args(["tools", "status", "--help"])
        .output()
        .unwrap();
    assert!(tstatus_help.status.success());
    let tstatus_text = String::from_utf8(tstatus_help.stdout).unwrap();
    assert!(tstatus_text.contains("Usage: tars tools status"));
    assert!(!tstatus_text.contains("install rg"));
}
