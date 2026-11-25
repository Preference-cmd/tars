//! End-to-end CLI consumer tests for P4a multi-agent collaboration.

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::process::{Command, Output};
#[cfg(unix)]
use std::sync::mpsc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
enum MockReply {
    Text(String),
    Calls(Vec<(String, Value)>),
}

type Handler = Arc<dyn Fn(usize, &Value, &str) -> MockReply + Send + Sync>;

struct MockServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<(Value, String)>>>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();

        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let counter = AtomicUsize::new(0);
            loop {
                let mut socket = loop {
                    if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => {
                            if is_post(&socket) {
                                break socket;
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(err) => panic!("mock server accept: {err}"),
                    }
                };

                let captured = captured.clone();
                let handler = handler.clone();
                let idx = counter.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let (start, length, headers) = loop {
                        let mut chunk = [0; 4096];
                        let count = match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (end + 4, length, headers);
                        }
                    };
                    while bytes.len() < start + length {
                        let mut chunk = [0; 4096];
                        let count = match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let req_json: Value =
                        serde_json::from_slice(&bytes[start..start + length]).unwrap();
                    captured
                        .lock()
                        .unwrap()
                        .push((req_json.clone(), headers.clone()));
                    let reply = handler(idx, &req_json, &headers);
                    let body = encode_chat_sse(reply, idx);
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });

        Self {
            endpoint,
            requests,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn is_post(socket: &TcpStream) -> bool {
    const PREFIX: &[u8] = b"POST ";
    if socket.set_nonblocking(false).is_err()
        || socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return false;
    }
    let mut head = [0; PREFIX.len()];
    loop {
        match socket.peek(&mut head) {
            Ok(0) | Err(_) => return false,
            Ok(n) if head[..n] != PREFIX[..n] => return false,
            Ok(n) if n == PREFIX.len() => return true,
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn data_sse(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn encode_chat_sse(reply: MockReply, step_idx: usize) -> String {
    match reply {
        MockReply::Text(text) => {
            data_sse(
                json!({"id":"chat","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
            ) + &data_sse(
                json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + "data: [DONE]\n\n"
        }
        MockReply::Calls(calls) => {
            let deltas: Vec<Value> = calls
                .into_iter()
                .enumerate()
                .map(|(i, (name, args))| {
                    let id = format!("call-{step_idx}-{i}");
                    json!({
                        "index": i,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args.to_string(),
                        }
                    })
                })
                .collect();
            data_sse(
                json!({"id":"chat","choices":[{"index":0,"delta":{"tool_calls":deltas},"finish_reason":null}]}),
            ) + &data_sse(
                json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ) + "data: [DONE]\n\n"
        }
    }
}

fn tars_cmd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tars"));
    cmd.env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env_remove("TARS_TEST_KEY")
        .env_remove("TARS_SPEC_KEY");
    cmd
}

fn json_out(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "invalid JSON stdout ({err}): stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn is_child_request(req: &Value, child_prompt: &str) -> bool {
    req["messages"].as_array().is_some_and(|msgs| {
        msgs.iter()
            .any(|msg| msg["role"] == "user" && msg["content"].as_str() == Some(child_prompt))
    })
}

#[test]
fn tars_run_converges_background_agent_and_reactive_wakeup_in_text_and_json() {
    for json_mode in [false, true] {
        let server = MockServer::start(Arc::new(|_idx, req, _| {
            let serialized = req.to_string();
            if is_child_request(req, "compute 42") {
                std::thread::sleep(Duration::from_millis(60));
                return MockReply::Text("worker-payload-42".into());
            }
            if serialized.contains("<task-notification") && serialized.contains("worker-payload-42")
            {
                return MockReply::Text("final merged: worker-payload-42".into());
            }
            if serialized.contains("\"role\":\"tool\"") {
                return MockReply::Text("waiting for worker".into());
            }
            MockReply::Calls(vec![(
                "spawn_agent".into(),
                json!({
                    "agent": "general",
                    "prompt": "compute 42",
                    "background": true
                }),
            )])
        }));

        let mut cmd = tars_cmd();
        cmd.args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
            "--input",
            "run background work",
        ])
        .env("TARS_TEST_KEY", "secret");
        if json_mode {
            cmd.arg("--json");
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "json_mode={json_mode}: stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if json_mode {
            let parsed = json_out(&output);
            assert_eq!(parsed["status"], "completed");
            assert_eq!(parsed["text"], "final merged: worker-payload-42");
        } else {
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                "final merged: worker-payload-42\n"
            );
        }
    }
}

fn has_task_notification(req: &Value) -> bool {
    req["messages"].as_array().is_some_and(|msgs| {
        msgs.iter().any(|msg| {
            msg["role"] == "user"
                && msg["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("<task-notification"))
        })
    })
}

#[cfg(unix)]
#[test]
fn tars_run_sigint_during_background_task_cancels_and_exits_130() {
    let (child_entered_tx, child_entered_rx) = mpsc::channel();
    let child_entered_tx = Mutex::new(Some(child_entered_tx));
    let (root_done_tx, root_done_rx) = mpsc::channel();
    let root_done_tx = Mutex::new(Some(root_done_tx));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let wakeup_seen = Arc::new(AtomicBool::new(false));
    let wakeup_seen_clone = wakeup_seen.clone();

    let server = MockServer::start(Arc::new(move |_idx, req, _| {
        let serialized = req.to_string();
        if is_child_request(req, "block until cancelled") {
            if let Some(tx) = child_entered_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
            return MockReply::Text("should not complete".into());
        }
        if has_task_notification(req) {
            wakeup_seen_clone.store(true, Ordering::SeqCst);
            return MockReply::Text("zombie wakeup".into());
        }
        if serialized.contains("\"role\":\"tool\"") {
            if let Some(tx) = root_done_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            return MockReply::Text("initial work done while child runs".into());
        }
        MockReply::Calls(vec![(
            "spawn_agent".into(),
            json!({
                "agent": "general",
                "prompt": "block until cancelled",
                "background": true
            }),
        )])
    }));

    let mut child: Child = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--json",
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
            "--input",
            "spawn blocker",
        ])
        .env("TARS_TEST_KEY", "secret")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if let Err(err) = child_entered_rx.recv_timeout(Duration::from_secs(5)) {
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        panic!(
            "child background agent must reach mock server: {err}; requests={:?}; stdout={:?}; stderr={:?}",
            server.requests.lock().unwrap(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    }
    root_done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("initial root turn must finish before SIGINT during wait_settled");
    // Give root work a brief moment to settle into wait_settled().
    std::thread::sleep(Duration::from_millis(50));

    let pid = child.id().to_string();
    let kill_status = Command::new("kill").args(["-INT", &pid]).status().unwrap();
    assert!(kill_status.success());

    let start = Instant::now();
    let output = child.wait_with_output().unwrap();
    let _ = release_tx.send(());
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "cancellation took too long: {:?}",
        start.elapsed()
    );
    assert_eq!(
        output.status.code(),
        Some(130),
        "expected exit 130, got {:?}, stdout={:?}, stderr={:?}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = json_out(&output);
    assert_eq!(parsed["status"], "cancelled");
    assert_eq!(parsed["kind"], "cancelled");
    assert!(!wakeup_seen.load(Ordering::SeqCst));
}

#[test]
fn persistent_session_show_and_history_inspect_agents_lock_free_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    let server = MockServer::start(Arc::new(|_idx, req, _| {
        let serialized = req.to_string();
        if is_child_request(req, "inspect code") {
            std::thread::sleep(Duration::from_millis(50));
            return MockReply::Text("analyst-findings-ok".into());
        }
        if serialized.contains("<task-notification") && serialized.contains("analyst-findings-ok") {
            return MockReply::Text("root-summary: analyst-findings-ok".into());
        }
        if serialized.contains("\"role\":\"tool\"") {
            return MockReply::Text("dispatched analyst".into());
        }
        MockReply::Calls(vec![(
            "spawn_agent".into(),
            json!({
                "agent": "general",
                "prompt": "inspect code",
                "background": true
            }),
        )])
    }));

    let create_out = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(
        create_out.status.success(),
        "{}",
        String::from_utf8_lossy(&create_out.stderr)
    );
    let conv_id = json_out(&create_out)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let run_out = tars_cmd()
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "start multi-agent analysis",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(
        run_out.status.success(),
        "run failed: stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_json = json_out(&run_out);
    assert_eq!(run_json["status"], "completed");
    assert_eq!(run_json["saved"], true);
    assert_eq!(run_json["text"], "root-summary: analyst-findings-ok");

    // Note: tars_cmd() intentionally removes TARS_TEST_KEY, proving lock-free read-only
    // queries succeed without model credentials!
    let show_json_out = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(show_json_out.status.success(), "{show_json_out:?}");
    let show_json = json_out(&show_json_out);
    let agents = show_json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2, "{show_json}");
    let root_agent = agents
        .iter()
        .find(|a| a["is_entry"] == true)
        .expect("entry agent");
    let child_agent = agents
        .iter()
        .find(|a| a["is_entry"] == false)
        .expect("child agent");
    assert_eq!(root_agent["name"], "main");
    assert_eq!(child_agent["name"], "general");
    let child_id = child_agent["agent_id"].as_str().unwrap().to_owned();

    let show_text_out = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(show_text_out.status.success());
    let show_text = String::from_utf8_lossy(&show_text_out.stdout);
    assert!(show_text.contains("Agents (2):"), "{show_text}");
    assert!(show_text.contains("main"), "{show_text}");
    assert!(show_text.contains(&child_id), "{show_text}");
    assert!(show_text.contains("general"), "{show_text}");

    // `session show <id> --agent <child_id>` (JSON and text) without credentials.
    let agent_show_json_out = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--agent",
            &child_id,
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        agent_show_json_out.status.success(),
        "{agent_show_json_out:?}"
    );
    let agent_show_json = json_out(&agent_show_json_out);
    assert_eq!(agent_show_json["agent_id"], child_id);
    assert_eq!(agent_show_json["name"], "general");
    assert_eq!(agent_show_json["is_entry"], false);
    let ops = agent_show_json["operations"].as_array().unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["status"], "completed");

    let agent_show_text_out = tars_cmd()
        .args([
            "session",
            "show",
            &conv_id,
            "--agent",
            &child_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(agent_show_text_out.status.success());
    let agent_show_text = String::from_utf8_lossy(&agent_show_text_out.stdout);
    assert!(agent_show_text.contains(&child_id), "{agent_show_text}");
    assert!(agent_show_text.contains("general"), "{agent_show_text}");
    assert!(
        agent_show_text.contains("Operations (1):"),
        "{agent_show_text}"
    );

    // `session history <id> --agent <child_id>` (JSON and text) without credentials.
    let child_hist_json = tars_cmd()
        .args([
            "session",
            "history",
            &conv_id,
            "--agent",
            &child_id,
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(child_hist_json.status.success(), "{child_hist_json:?}");
    let child_hist_stdout = String::from_utf8_lossy(&child_hist_json.stdout);
    assert!(
        child_hist_stdout.contains("analyst-findings-ok"),
        "{child_hist_stdout}"
    );
    assert!(
        !child_hist_stdout.contains("start multi-agent analysis"),
        "child history must stay isolated from root prompt: {child_hist_stdout}"
    );

    let child_hist_text = tars_cmd()
        .args([
            "session",
            "history",
            &conv_id,
            "--agent",
            "general",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(child_hist_text.status.success(), "{child_hist_text:?}");
    let child_hist_text_stdout = String::from_utf8_lossy(&child_hist_text.stdout);
    assert!(
        child_hist_text_stdout.contains("analyst-findings-ok"),
        "{child_hist_text_stdout}"
    );
}

#[cfg(unix)]
#[test]
fn workspace_agents_trust_gate_blocks_until_explicit_flag_and_persists_in_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let workspace = dir.path().join("workspace");
    let agents_dir = workspace.join(".tars/agents");
    fs::create_dir_all(&agents_dir).unwrap();
    fs::write(
        agents_dir.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Workspace code reviewer\n---\nYou are a careful code reviewer.\n",
    )
    .unwrap();

    let server = MockServer::start(Arc::new(|_idx, _req, _| MockReply::Text("ok".into())));

    // 0. `tars run --trust-workspace-agents` must be rejected with exit code 2.
    let bad_run = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--trust-workspace-agents",
            "--input",
            "hi",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert_eq!(bad_run.status.code(), Some(2), "{bad_run:?}");

    // 1. Transient `tars run` warns on stderr and excludes untrusted `reviewer`.
    let transient_run = tars_cmd()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
            "--input",
            "hello transient",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(transient_run.status.success(), "{transient_run:?}");
    let transient_stderr = String::from_utf8_lossy(&transient_run.stderr);
    assert!(
        transient_stderr.contains("workspace agents are untrusted")
            && transient_stderr.contains("transient tars run cannot trust workspace agents"),
        "{transient_stderr}"
    );
    assert!(
        !server.requests.lock().unwrap()[0]
            .0
            .to_string()
            .contains("Workspace code reviewer")
    );

    // 2. `session create` without `--trust-workspace-agents` warns on stderr.
    let create_out = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--json",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "main-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &server.endpoint,
            "--workspace",
            workspace.to_str().unwrap(),
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(create_out.status.success(), "{create_out:?}");
    let create_stderr = String::from_utf8_lossy(&create_out.stderr);
    assert!(
        create_stderr.contains("workspace agents are untrusted")
            && create_stderr.contains("use --trust-workspace-agents to trust"),
        "{create_stderr}"
    );
    let conv_id = json_out(&create_out)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 3. `session run` without `--trust-workspace-agents` warns and omits `reviewer` from `<available_agents>`.
    let run_untrusted = tars_cmd()
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "turn 1",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(run_untrusted.status.success(), "{run_untrusted:?}");
    let run1_stderr = String::from_utf8_lossy(&run_untrusted.stderr);
    assert!(
        run1_stderr.contains("workspace agents are untrusted"),
        "{run1_stderr}"
    );
    let req1 = server.requests.lock().unwrap()[1].0.to_string();
    assert!(
        !req1.contains("Workspace code reviewer"),
        "untrusted workspace agent leaked into model request: {req1}"
    );

    // 4. `session run --trust-workspace-agents` trusts and persists workspace agents.
    let run_trusted = tars_cmd()
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--trust-workspace-agents",
            "--input",
            "turn 2",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(run_trusted.status.success(), "{run_trusted:?}");
    let run2_stderr = String::from_utf8_lossy(&run_trusted.stderr);
    assert!(
        !run2_stderr.contains("workspace agents are untrusted"),
        "{run2_stderr}"
    );
    let req2 = server.requests.lock().unwrap()[2].0.to_string();
    assert!(
        req2.contains("<available_agents>") && req2.contains("reviewer"),
        "trusted workspace agent missing from model request: {req2}"
    );

    // 5. Subsequent `session run` without `--trust-workspace-agents` sees persisted trust!
    let run_after = tars_cmd()
        .args([
            "session",
            "run",
            &conv_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "turn 3",
        ])
        .env("TARS_TEST_KEY", "secret")
        .output()
        .unwrap();
    assert!(run_after.status.success(), "{run_after:?}");
    let run3_stderr = String::from_utf8_lossy(&run_after.stderr);
    assert!(
        !run3_stderr.contains("workspace agents are untrusted"),
        "{run3_stderr}"
    );
    let req3 = server.requests.lock().unwrap()[3].0.to_string();
    assert!(
        req3.contains("<available_agents>") && req3.contains("reviewer"),
        "persisted workspace agent trust not active on reopen: {req3}"
    );
}

#[test]
fn config_model_profiles_are_forwarded_and_usable_by_spawn_agent() {
    let dir = tempfile::tempdir().unwrap();
    let specialist_server = MockServer::start(Arc::new(|_idx, _req, _| {
        MockReply::Text("from-specialist".into())
    }));
    let main_server = MockServer::start(Arc::new(|_idx, req, _| {
        let serialized = req.to_string();
        if serialized.contains("\"role\":\"tool\"") {
            return MockReply::Text("main got: from-specialist".into());
        }
        MockReply::Calls(vec![(
            "spawn_agent".into(),
            json!({
                "agent": "general",
                "prompt": "delegate to specialist",
                "background": false,
                "model": { "profile": "specialist" }
            }),
        )])
    }));

    let config_path = dir.path().join("config.toml");
    write_profile_config(
        &config_path,
        &main_server.endpoint,
        &specialist_server.endpoint,
    );

    let output = tars_cmd()
        .args([
            "run",
            "--config",
            config_path.to_str().unwrap(),
            "--json",
            "--input",
            "use specialist profile",
        ])
        .env("TARS_TEST_KEY", "main-secret")
        .env("TARS_SPEC_KEY", "spec-secret")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed = json_out(&output);
    assert_eq!(parsed["status"], "completed");
    assert_eq!(parsed["text"], "main got: from-specialist");

    let main_reqs = main_server.requests.lock().unwrap();
    assert!(!main_reqs.is_empty());
    let first_main_body = main_reqs[0].0.to_string();
    assert!(
        first_main_body.contains("<model_profiles>") && first_main_body.contains("specialist"),
        "expected <model_profiles> in main request: {first_main_body}"
    );

    let spec_reqs = specialist_server.requests.lock().unwrap();
    assert_eq!(spec_reqs.len(), 1);
    assert_eq!(spec_reqs[0].0["model"], "specialist-model");
    assert!(
        spec_reqs[0]
            .1
            .to_ascii_lowercase()
            .contains("authorization: bearer spec-secret"),
        "expected specialist auth header, got: {}",
        spec_reqs[0].1
    );
}

fn write_profile_config(path: &Path, main_endpoint: &str, spec_endpoint: &str) {
    let content = format!(
        r#"version = 1
default_profile = "main"

[profiles.main]
provider = "openai-chat"
model = "main-model"
endpoint = "{main_endpoint}"
api_key_env = "TARS_TEST_KEY"

[profiles.specialist]
provider = "openai-chat"
model = "specialist-model"
endpoint = "{spec_endpoint}"
api_key_env = "TARS_SPEC_KEY"
"#
    );
    fs::write(path, content).unwrap();
}
