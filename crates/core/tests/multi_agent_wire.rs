//! End-to-end 3-protocol local HTTP wire integration tests for P3 multi-agent collaboration:
//! - `<available_agents>` dynamic prelude & workspace trust gating
//! - `spawn_agent` with `model` profile, `reasoning_effort`, `temperature`, `max_tokens` wire fields
//! - Sync parallel fan-out (`background=false`) across all 3 protocols
//! - Background event mid-turn injection (`ToolResult` immediately followed by `Role::User` `<task-notification>`)
//! - `read_agent` UTF-8 safe pagination
//! - `cancel_agent` cooperative cancellation without zombie wakeups
//! - SQLite reopen without auto-wakeup and read-only `read_agent` recovery

#![cfg(any(target_os = "linux", target_os = "macos"))]

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tars_core::agents::ModelProfile;
use tars_core::application::{ClosePolicy, PreparedCreate, SessionHost, text_input};
use tars_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};
use tars_core::events::OperationStatus;
use tars_core::session::{SubmitRequest, WorkState};

const PROTOCOLS: [(&str, Provider); 3] = [
    ("openai-chat", Provider::OpenAiChat),
    ("openai-responses", Provider::OpenAiResponses),
    ("anthropic", Provider::Anthropic),
];

#[derive(Clone, Debug)]
enum WireReply {
    Text(String),
    Calls(Vec<(String, Value)>),
}

type Responder = Arc<dyn Fn(usize, &Value) -> WireReply + Send + Sync>;

struct WireServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    headers: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl WireServer {
    fn start(provider: Provider, responder: Responder) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let path = match provider {
            Provider::OpenAiChat => "/v1/chat/completions",
            Provider::OpenAiResponses => "/v1/responses",
            Provider::Anthropic => "/v1/messages",
        };
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let captured_headers = Arc::new(Mutex::new(Vec::new()));
        let headers_for_server = captured_headers.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();

        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let call_counter = AtomicUsize::new(0);
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
                        Err(err) => panic!("wire server accept: {err}"),
                    }
                };

                let captured = captured.clone();
                let captured_headers = headers_for_server.clone();
                let responder = responder.clone();
                let idx = call_counter.fetch_add(1, Ordering::SeqCst);
                let path = path.to_owned();
                std::thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let (start, length, raw_headers) = loop {
                        let mut chunk = [0; 4096];
                        let count = match socket.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        bytes.extend_from_slice(&chunk[..count]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                            assert!(headers.starts_with(&format!("POST {path} HTTP/1.1")));
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (end + 4, length, headers.to_owned());
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
                    captured_headers.lock().unwrap().push(raw_headers);
                    captured.lock().unwrap().push(req_json.clone());
                    let reply = responder(idx, &req_json);
                    let body = encode_sse(provider, reply, idx);
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
            headers: captured_headers,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for WireServer {
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

fn encode_sse(provider: Provider, reply: WireReply, step_idx: usize) -> String {
    match provider {
        Provider::OpenAiChat => match reply {
            WireReply::Text(text) => {
                data_sse(
                    json!({"id":"chat","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
                ) + &data_sse(
                    json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                ) + "data: [DONE]\n\n"
            }
            WireReply::Calls(calls) => {
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
        },
        Provider::OpenAiResponses => match reply {
            WireReply::Text(text) => {
                let item = json!({
                    "id": format!("msg-{step_idx}"),
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text}]
                });
                data_sse(
                    json!({"type":"response.created","response":{"id":"resp","status":"in_progress","output":[]}}),
                ) + &data_sse(
                    json!({"type":"response.output_item.added","output_index":0,"item":{"id":format!("msg-{step_idx}"),"type":"message","role":"assistant","content":[]}}),
                ) + &data_sse(
                    json!({"type":"response.content_part.added","output_index":0,"item_id":format!("msg-{step_idx}"),"content_index":0,"part":{"type":"output_text","text":""}}),
                ) + &data_sse(
                    json!({"type":"response.output_text.delta","output_index":0,"item_id":format!("msg-{step_idx}"),"content_index":0,"delta":text}),
                ) + &data_sse(
                    json!({"type":"response.completed","response":{"id":"resp","status":"completed","output":[item]}}),
                )
            }
            WireReply::Calls(calls) => {
                let mut out = data_sse(
                    json!({"type":"response.created","response":{"id":"resp","status":"in_progress","output":[]}}),
                );
                let mut completed_items = Vec::new();
                for (i, (name, args)) in calls.into_iter().enumerate() {
                    let id = format!("fc-{step_idx}-{i}");
                    let args_str = args.to_string();
                    out.push_str(&data_sse(json!({
                        "type": "response.output_item.added",
                        "output_index": i,
                        "item": {"type":"function_call","id":id,"call_id":id,"name":name,"arguments":""}
                    })));
                    out.push_str(&data_sse(json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": i,
                        "item_id": id,
                        "delta": args_str
                    })));
                    completed_items.push(json!({
                        "type": "function_call",
                        "id": id,
                        "call_id": id,
                        "name": name,
                        "arguments": args_str
                    }));
                }
                out.push_str(&data_sse(json!({"type":"response.completed","response":{"id":"resp","status":"completed","output":completed_items}})));
                out
            }
        },
        Provider::Anthropic => match reply {
            WireReply::Text(text) => {
                data_sse(
                    json!({"type":"message_start","message":{"id":"msg","content":[],"stop_reason":null}}),
                ) + &data_sse(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                ) + &data_sse(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                ) + &data_sse(json!({"type":"content_block_stop","index":0}))
                    + &data_sse(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
                    + &data_sse(json!({"type":"message_stop"}))
            }
            WireReply::Calls(calls) => {
                let mut out = data_sse(
                    json!({"type":"message_start","message":{"id":"msg","content":[],"stop_reason":null}}),
                );
                for (i, (name, args)) in calls.into_iter().enumerate() {
                    let id = format!("toolu-{step_idx}-{i}");
                    out.push_str(&data_sse(json!({
                        "type": "content_block_start",
                        "index": i,
                        "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
                    })));
                    out.push_str(&data_sse(json!({
                        "type": "content_block_delta",
                        "index": i,
                        "delta": {"type": "input_json_delta", "partial_json": args.to_string()}
                    })));
                    out.push_str(&data_sse(json!({"type":"content_block_stop","index":i})));
                }
                out.push_str(&data_sse(
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                ));
                out.push_str(&data_sse(json!({"type":"message_stop"})));
                out
            }
        },
    }
}

fn wire_definition(provider: Provider, endpoint: &str, ws_root: &Path) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "parent-wire-model".into(),
        endpoint: Some(endpoint.to_owned()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 16,
        max_model_rounds: 8,
        max_tool_calls: 16,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(ws_root.canonicalize().unwrap()),
        tools: vec![WorkspaceTool::ReadFile],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
        code_execution: None,
    }
}

fn is_main_request(req: &Value) -> bool {
    req.to_string()
        .contains("You are running inside the TARS agent host.")
}

fn make_child_profile(name: &str, provider: Provider, endpoint: &str) -> ModelProfile {
    ModelProfile {
        name: name.to_owned(),
        provider,
        model: "child-wire-model".into(),
        endpoint: Some(endpoint.to_owned()),
        credential: CredentialRef::Environment("PATH".into()),
        max_tokens: (provider == Provider::Anthropic).then_some(512),
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        context_window: None,
        max_model_attempts: None,
        render_options: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_p3_three_protocol_discovery_profile_override_parallel_fanout_and_reopen() {
    for (proto_name, provider) in PROTOCOLS {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let ws_agents = ws.join(".tars/agents");
        let store = dir.path().join("store");
        fs::create_dir_all(&ws_agents).unwrap();
        fs::write(ws.join("notes.txt"), "hello workspace\n").unwrap();
        fs::write(
            ws_agents.join("reviewer.md"),
            concat!(
                "---\n",
                "name: reviewer\n",
                "description: Wire reviewer preset\n",
                "tools: [read_file]\n",
                "---\n",
                "You are a wire reviewer.\n",
            ),
        )
        .unwrap();

        let child_server = WireServer::start(
            provider,
            Arc::new(|_, _| WireReply::Text("reviewer done".into())),
        );

        let main_step = Arc::new(AtomicUsize::new(0));
        let main_step_clone = main_step.clone();
        let parent_server = WireServer::start(
            provider,
            Arc::new(move |_, req| {
                if !is_main_request(req) {
                    // Built-in `explore` inherits parent's endpoint when no profile override is set.
                    return WireReply::Text("explore done".into());
                }
                match main_step_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => WireReply::Text("turn1 untrusted".into()),
                    1 => WireReply::Calls(vec![
                        (
                            "spawn_agent".into(),
                            json!({
                                "agent": "reviewer",
                                "prompt": "review code",
                                "background": false,
                                "model": {
                                    "profile": "child-prof",
                                    "reasoning_effort": "high",
                                    "temperature": 0.4,
                                    "max_tokens": 777
                                }
                            }),
                        ),
                        (
                            "spawn_agent".into(),
                            json!({
                                "agent": "explore",
                                "prompt": "explore code",
                                "background": false
                            }),
                        ),
                    ]),
                    _ => WireReply::Text("parallel fan-out finished".into()),
                }
            }),
        );

        let mut profiles = BTreeMap::new();
        profiles.insert(
            "child-prof".to_owned(),
            make_child_profile("child-prof", provider, &child_server.endpoint),
        );

        let host = SessionHost::new(Some(store.clone())).with_model_profiles(profiles.clone());
        let def = wire_definition(provider, &parent_server.endpoint, &ws);
        let session = host
            .create(PreparedCreate::prepare_persistent(def, None).unwrap())
            .await
            .unwrap();
        let conv_id = session.target().conversation_id().clone();
        assert!(!session.workspace_agents_trusted());

        // Turn 1 (untrusted): `reviewer` must NOT appear in `<available_agents>`.
        let w1 = session
            .submit(SubmitRequest {
                request_key: "t1".into(),
                blocks: vec![text_input("turn 1")],
            })
            .unwrap()
            .work;
        let obs1 = session.wait(&w1, Duration::from_secs(10)).await.unwrap();
        assert!(matches!(obs1.state, WorkState::Finished(_)), "{proto_name}");
        session.wait_saved().await.unwrap();

        {
            let reqs = parent_server.requests.lock().unwrap();
            let r0 = reqs[0].to_string();
            assert!(r0.contains("<available_agents>"), "{proto_name}: {r0}");
            assert!(r0.contains("<name>explore</name>"), "{proto_name}");
            assert!(r0.contains("child-prof"), "{proto_name}: {r0}");
            assert!(!r0.contains("<name>reviewer</name>"), "{proto_name}");
        }

        // Trust workspace agents -> Turn 2 emits `<available_agents updated="true">` & fans out 2 parallel sync children!
        assert_eq!(session.trust_workspace_agents().await.unwrap(), 1);
        let w2 = session
            .submit(SubmitRequest {
                request_key: "t2".into(),
                blocks: vec![text_input("turn 2")],
            })
            .unwrap()
            .work;
        let obs2 = session.wait(&w2, Duration::from_secs(10)).await.unwrap();
        assert!(matches!(obs2.state, WorkState::Finished(_)), "{proto_name}");
        session.wait_saved().await.unwrap();

        // Inspect child_server outbound HTTP JSON body for `reviewer`: model, temperature, reasoning_effort, max_tokens, role_prompt.
        let reviewer_agent_id = {
            let child_reqs = child_server.requests.lock().unwrap();
            assert_eq!(child_reqs.len(), 1, "{proto_name}");
            let body = &child_reqs[0];
            assert_eq!(body["model"], "child-wire-model", "{proto_name}");
            let temp = body["temperature"].as_f64().unwrap();
            assert!((temp - 0.4).abs() < 1e-4, "{proto_name}: temp={temp}");
            match provider {
                Provider::OpenAiChat => {
                    assert_eq!(body["reasoning_effort"], "high");
                    assert_eq!(body["max_completion_tokens"], 777);
                }
                Provider::OpenAiResponses => {
                    assert_eq!(body["reasoning"]["effort"], "high");
                    assert_eq!(body["max_output_tokens"], 777);
                }
                Provider::Anthropic => {
                    assert_eq!(body["output_config"]["effort"], "high");
                    assert_eq!(body["max_tokens"], 777);
                }
            }
            assert!(
                body.to_string().contains("You are a wire reviewer."),
                "{proto_name}: {body}"
            );

            // Inspect parent_server Turn 2 round 1: contains both ToolResults ("reviewer done" and "explore done").
            let parent_reqs = parent_server.requests.lock().unwrap();
            let last_main = parent_reqs.last().unwrap().to_string();
            assert!(last_main.contains("<name>reviewer</name>"), "{proto_name}");
            assert!(last_main.contains("reviewer done"), "{proto_name}");
            assert!(last_main.contains("explore done"), "{proto_name}");

            // Extract reviewer operation_id from `last_main` or read from SQLite via coordinator:
            let pos = last_main.find("operation_id").unwrap();
            let after = &last_main[pos..];
            // Also find via `read_agent` using coordinator snapshot: verify via session.read_agent below!
            let _ = after;
            session
                .list_agents()
                .into_iter()
                .find(|a| a.name == "reviewer")
                .unwrap()
                .agent_id
        };

        // Close and reopen via SessionHost::open -> zero auto-wakeup!
        let parent_req_count_before_close = parent_server.requests.lock().unwrap().len();
        let child_req_count_before_close = child_server.requests.lock().unwrap().len();
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        drop(session);

        let host2 = SessionHost::new(Some(store));
        let reopened = host2.open(conv_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            parent_server.requests.lock().unwrap().len(),
            parent_req_count_before_close
        );
        assert_eq!(
            child_server.requests.lock().unwrap().len(),
            child_req_count_before_close
        );

        let reviewer_snap = reopened.agent_snapshot(&reviewer_agent_id).unwrap();
        assert_eq!(reviewer_snap.name, "reviewer");
        host2
            .close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_p3_three_protocol_background_mid_turn_injection_pagination_and_cancel() {
    for (proto_name, provider) in PROTOCOLS {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let store = dir.path().join("store");
        fs::create_dir_all(&ws).unwrap();
        fs::write(ws.join("data.txt"), "alpha\nbeta\n").unwrap();

        let utf8_payload = "第一页🦀第二页🚀第三页!".repeat(3);
        let child_step = Arc::new(AtomicUsize::new(0));
        let child_step_clone = child_step.clone();
        let bg_done_flag = Arc::new(AtomicBool::new(false));
        let bg_done_clone = bg_done_flag.clone();
        let slow_child_entered = Arc::new(AtomicBool::new(false));
        let slow_child_entered_clone = slow_child_entered.clone();
        let slow_child_released = Arc::new(AtomicBool::new(false));
        let slow_child_released_clone = slow_child_released.clone();
        let utf8_for_child = utf8_payload.clone();

        let child_server = WireServer::start(
            provider,
            Arc::new(move |_, _| {
                if child_step_clone.fetch_add(1, Ordering::SeqCst) == 0 {
                    bg_done_clone.store(true, Ordering::SeqCst);
                    WireReply::Text(utf8_for_child.clone())
                } else {
                    slow_child_entered_clone.store(true, Ordering::SeqCst);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while Instant::now() < deadline {
                        if slow_child_released_clone.load(Ordering::SeqCst) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    WireReply::Text("never reached".into())
                }
            }),
        );

        let bg_op_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let bg_op_id_clone = bg_op_id.clone();
        let cancel_op_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cancel_op_id_clone = cancel_op_id.clone();
        let main_step = Arc::new(AtomicUsize::new(0));
        let main_step_clone = main_step.clone();
        let round2_req: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let round2_req_clone = round2_req.clone();
        let round3_req: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let round3_req_clone = round3_req.clone();
        let session_holder: Arc<Mutex<Option<tars_core::application::ManagedSession>>> =
            Arc::new(Mutex::new(None));
        let session_for_server = session_holder.clone();

        let parent_server = WireServer::start(
            provider,
            Arc::new(
                move |_, req| match main_step_clone.fetch_add(1, Ordering::SeqCst) {
                    0 => WireReply::Calls(vec![(
                        "spawn_agent".into(),
                        json!({
                            "agent": "general",
                            "prompt": "produce utf8",
                            "background": true,
                            "model": { "profile": "bg-prof" }
                        }),
                    )]),
                    1 => {
                        // Extract operation_id from ToolResult in `req` and wait until background child finishes & outbox lands in parent's inbox!
                        let op_id = extract_json_field_from_req(req, "operation_id")
                            .expect("operation_id in round 1 ToolResult");
                        *bg_op_id_clone.lock().unwrap() = Some(op_id);
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while Instant::now() < deadline {
                            let settled =
                                session_for_server
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .is_some_and(|s| {
                                        s.active_finite_background_tasks() == 0
                                            && bg_done_flag.load(Ordering::SeqCst)
                                    });
                            if settled {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        WireReply::Calls(vec![(
                            "read_file".into(),
                            json!({ "path": "data.txt", "start_line": 1, "max_lines": 2 }),
                        )])
                    }
                    2 => {
                        // Round 2 MUST have `read_file` ToolResult immediately followed by Role::User `<task-notification>`!
                        *round2_req_clone.lock().unwrap() = Some(req.clone());
                        let op_id = bg_op_id_clone.lock().unwrap().clone().unwrap();
                        WireReply::Calls(vec![(
                            "read_agent".into(),
                            json!({ "operation_id": op_id, "offset_bytes": 0, "limit_bytes": 17 }),
                        )])
                    }
                    3 => {
                        // Capture Round 3 outbound request carrying Round 2's paginated `read_agent` ToolResult!
                        *round3_req_clone.lock().unwrap() = Some(req.clone());
                        // Spawn slow background child to test `cancel_agent`
                        WireReply::Calls(vec![(
                            "spawn_agent".into(),
                            json!({
                                "agent": "general",
                                "prompt": "slow job",
                                "background": true,
                                "model": { "profile": "bg-prof" }
                            }),
                        )])
                    }
                    4 => {
                        let op_id = extract_json_field_from_req(req, "operation_id")
                            .expect("slow operation_id in round 4 ToolResult");
                        *cancel_op_id_clone.lock().unwrap() = Some(op_id.clone());
                        // Wait until slow child actually hits child_server before cancelling!
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while Instant::now() < deadline {
                            if slow_child_entered.load(Ordering::SeqCst) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        WireReply::Calls(vec![(
                            "cancel_agent".into(),
                            json!({ "operation_id": op_id }),
                        )])
                    }
                    _ => WireReply::Text("all p3 wire checks complete".into()),
                },
            ),
        );

        let mut profiles = BTreeMap::new();
        profiles.insert(
            "bg-prof".to_owned(),
            make_child_profile("bg-prof", provider, &child_server.endpoint),
        );

        let host = SessionHost::new(Some(store.clone())).with_model_profiles(profiles);
        let def = wire_definition(provider, &parent_server.endpoint, &ws);
        let session = host
            .create(PreparedCreate::prepare_persistent(def, None).unwrap())
            .await
            .unwrap();
        *session_holder.lock().unwrap() = Some(session.clone());
        let conv_id = session.target().conversation_id().clone();

        let work = session
            .submit(SubmitRequest {
                request_key: "t-bg".into(),
                blocks: vec![text_input("run background + read + cancel")],
            })
            .unwrap()
            .work;
        let obs = session.wait(&work, Duration::from_secs(15)).await.unwrap();
        assert!(
            matches!(obs.state, WorkState::Finished(_)),
            "{proto_name}: {obs:?}"
        );
        slow_child_released.store(true, Ordering::SeqCst);
        session.wait_saved().await.unwrap();
        // Clear session_holder cycle before closing!
        *session_holder.lock().unwrap() = None;

        // 1. Verify Round 2 outbound HTTP body has ToolResult immediately followed by Role::User `<task-notification`!
        let r2 = round2_req.lock().unwrap().take().expect("round 2 request");
        verify_tool_result_followed_by_task_notification(provider, &r2);

        // 2. Verify `read_agent` UTF-8 safe pagination reassembles exact original string across multiple pages
        // and that Round 3's outbound wire request contains the paginated `read_agent` output returned from Round 2!
        let bg_id = bg_op_id.lock().unwrap().clone().unwrap();
        let first_page = session.read_agent(&bg_id, 0, 17).await.unwrap();
        assert_eq!(first_page.status, OperationStatus::Completed);
        assert!(first_page.page.has_more, "{proto_name}");
        assert!(
            first_page.page.content.len() <= 17 && !first_page.page.content.is_empty(),
            "{proto_name}"
        );
        let r3 = round3_req.lock().unwrap().take().expect("round 3 request");
        assert!(
            r3.to_string().contains(&first_page.page.content),
            "{proto_name}: Round 3 outbound wire request must carry paginated read_agent output from Round 2: {r3}"
        );

        let mut offset = 0usize;
        let mut reassembled = String::new();
        let mut page_count = 0usize;
        loop {
            let page = session.read_agent(&bg_id, offset, 17).await.unwrap();
            assert_eq!(page.status, OperationStatus::Completed);
            reassembled.push_str(&page.page.content);
            page_count += 1;
            if !page.page.has_more {
                assert_eq!(page.page.next_offset_bytes, utf8_payload.len());
                break;
            }
            offset = page.page.next_offset_bytes;
        }
        assert!(page_count > 3, "{proto_name}: expected multiple pages");
        assert_eq!(reassembled, utf8_payload, "{proto_name}");

        // 3. Verify `cancel_agent` settled the slow child as Cancelled with zero zombie wakeup!
        let cancel_id = cancel_op_id.lock().unwrap().clone().unwrap();
        let cancelled_page = session.read_agent(&cancel_id, 0, 1024).await.unwrap();
        assert_eq!(
            cancelled_page.status,
            OperationStatus::Cancelled,
            "{proto_name}"
        );
        assert_eq!(session.active_finite_background_tasks(), 0);

        // 4. SQLite reopen: zero auto-wakeup and `read_agent` retrieves archived UTF-8 output without credentials!
        let reqs_before_reopen = parent_server.requests.lock().unwrap().len();
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        drop(session);

        let host2 = SessionHost::new(Some(store));
        let reopened = host2.open(conv_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            parent_server.requests.lock().unwrap().len(),
            reqs_before_reopen
        );
        let reopened_page = reopened.read_agent(&bg_id, 0, 1024).await.unwrap();
        assert_eq!(reopened_page.status, OperationStatus::Completed);
        assert_eq!(reopened_page.page.content, utf8_payload);
        host2
            .close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

fn verify_tool_result_followed_by_task_notification(provider: Provider, req: &Value) {
    match provider {
        Provider::OpenAiChat => {
            let msgs = req["messages"].as_array().unwrap();
            let pos = msgs.windows(2).position(|pair| {
                pair[0]["role"] == "tool"
                    && pair[0]["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("alpha"))
                    && pair[1]["role"] == "user"
                    && pair[1]["content"]
                        .as_str()
                        .is_some_and(|s| s.contains("<task-notification"))
            });
            assert!(
                pos.is_some(),
                "OpenAI Chat must have tool result immediately followed by user <task-notification>: {msgs:?}"
            );
        }
        Provider::OpenAiResponses => {
            let input = req["input"].as_array().unwrap();
            let pos = input.windows(2).position(|pair| {
                pair[0]["type"] == "function_call_output"
                    && pair[0]["output"]
                        .as_str()
                        .is_some_and(|s| s.contains("alpha"))
                    && pair[1]["role"] == "user"
                    && pair[1].to_string().contains("<task-notification")
            });
            assert!(
                pos.is_some(),
                "OpenAI Responses must have function_call_output immediately followed by user <task-notification>: {input:?}"
            );
        }
        Provider::Anthropic => {
            let msgs = req["messages"].as_array().unwrap();
            // In Anthropic Messages, consecutive Role::User blocks (ToolResult + Role::User notification)
            // may be merged into one `role: "user"` message's `content` array or sit in adjacent `user` messages.
            let merged_match = msgs.iter().any(|m| {
                m["role"] == "user"
                    && m["content"].as_array().is_some_and(|parts| {
                        parts.windows(2).any(|pair| {
                            pair[0]["type"] == "tool_result"
                                && pair[0].to_string().contains("alpha")
                                && pair[1]["type"] == "text"
                                && pair[1]["text"]
                                    .as_str()
                                    .is_some_and(|s| s.contains("<task-notification"))
                        })
                    })
            });
            let adjacent_match = msgs.windows(2).any(|pair| {
                pair[0]["role"] == "user"
                    && pair[0].to_string().contains("tool_result")
                    && pair[0].to_string().contains("alpha")
                    && pair[1]["role"] == "user"
                    && pair[1].to_string().contains("<task-notification")
            });
            assert!(
                merged_match || adjacent_match,
                "Anthropic must have tool_result immediately followed by <task-notification>: {msgs:?}"
            );
        }
    }
}

fn extract_json_field_from_req(req: &Value, field: &str) -> Option<String> {
    let raw = req.to_string();
    // Look for escaped or unescaped JSON key `"operation_id":"..."` in serialized tool output.
    for needle in [format!("\\\"{field}\\\":\\\""), format!("\"{field}\":\"")] {
        if let Some(pos) = raw.rfind(&needle) {
            let rest = &raw[pos + needle.len()..];
            let end = rest.find(['\\', '"'])?;
            return Some(rest[..end].to_owned());
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn store_backed_credentials_work_for_root_and_child_agents_and_update_runtime_config_preserves_open_sessions()
 {
    use tars_core::application::{ApplicationError, AssemblyError, CredentialUnavailableReason};
    use tars_core::config::CredentialsStore;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    let store = dir.path().join("store");
    let creds_path = dir.path().join("credentials.toml");
    let root_secret = "sk-root-store-secret-111";
    let child_secret = "sk-child-store-secret-222";
    CredentialsStore::set(&creds_path, "root-cred", root_secret).unwrap();
    CredentialsStore::set(&creds_path, "child-cred", child_secret).unwrap();

    // 1. Unconfigured host (`credentials_path: None`) fails closed on CredentialRef::Store.
    let isolated_host = SessionHost::new(Some(store.clone()));
    let mut blocked_def = wire_definition(Provider::OpenAiChat, "http://127.0.0.1:1/unused", &ws);
    blocked_def.credential = CredentialRef::Store("root-cred".into());
    let err = match isolated_host
        .create(PreparedCreate::prepare_transient(blocked_def, None).unwrap())
        .await
    {
        Ok(_) => panic!("unconfigured host must reject CredentialRef::Store"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        ApplicationError::Assembly(AssemblyError::CredentialUnavailable(
            CredentialUnavailableReason::StoreDisabled { ref name }
        )) if name == "root-cred"
    ));

    // 2. Host with an already-open session preserves `HostInner.entries` across `update_runtime_config`,
    //    and newly created sessions (both root and child agents) resolve store credentials via the updated config.
    let host = SessionHost::new(Some(store.clone()));
    let existing_def = wire_definition(Provider::OpenAiChat, "http://127.0.0.1:1/unused", &ws);
    let existing_session = host
        .create(PreparedCreate::prepare_persistent(existing_def, None).unwrap())
        .await
        .unwrap();
    let existing_id = existing_session.target().conversation_id().clone();

    let parent_server = WireServer::start(
        Provider::OpenAiChat,
        Arc::new(|call_idx, _req| match call_idx {
            0 => WireReply::Calls(vec![(
                "spawn_agent".into(),
                json!({
                    "agent": "explore",
                    "prompt": "do child work",
                    "background": false,
                    "model": { "profile": "child-prof" }
                }),
            )]),
            _ => WireReply::Text("root done".into()),
        }),
    );
    let child_server = WireServer::start(
        Provider::OpenAiChat,
        Arc::new(|_call_idx, _req| WireReply::Text("child done".into())),
    );

    let mut updated_profiles = BTreeMap::new();
    let mut child_profile =
        make_child_profile("child-prof", Provider::OpenAiChat, &child_server.endpoint);
    child_profile.credential = CredentialRef::Store("child-cred".into());
    updated_profiles.insert("child-prof".into(), child_profile);

    host.update_runtime_config(Some(creds_path), None, updated_profiles);

    // Existing open session is still tracked in `HostInner.entries` and saves cleanly!
    assert!(host.lookup(&existing_id).is_some());
    host.close(&existing_session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    drop(existing_session);

    // New session uses updated `credentials_path` and `model_profiles` for root + child turns.
    let mut root_def = wire_definition(Provider::OpenAiChat, &parent_server.endpoint, &ws);
    root_def.credential = CredentialRef::Store("root-cred".into());
    let session = host
        .create(PreparedCreate::prepare_persistent(root_def, None).unwrap())
        .await
        .unwrap();
    let work = session
        .submit(SubmitRequest {
            request_key: "store-turn".into(),
            blocks: vec![text_input("delegate to child")],
        })
        .unwrap()
        .work;
    let obs = session.wait(&work, Duration::from_secs(15)).await.unwrap();
    assert!(matches!(obs.state, WorkState::Finished(_)), "{obs:?}");

    let parent_headers = parent_server
        .headers
        .lock()
        .unwrap()
        .join("\n")
        .to_ascii_lowercase();
    let child_headers = child_server
        .headers
        .lock()
        .unwrap()
        .join("\n")
        .to_ascii_lowercase();
    assert!(
        parent_headers
            .contains(&format!("authorization: bearer {root_secret}").to_ascii_lowercase()),
        "root turn did not receive store credential: {parent_headers}"
    );
    assert!(
        child_headers
            .contains(&format!("authorization: bearer {child_secret}").to_ascii_lowercase()),
        "child turn did not receive store credential: {child_headers}"
    );

    // Persisted SQLite record must never contain plaintext secrets (`CR03`).
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    drop(session);
    let db_bytes = fs::read(store.join("tars.db")).unwrap();
    let db_lossy = String::from_utf8_lossy(&db_bytes);
    assert!(!db_lossy.contains(root_secret));
    assert!(!db_lossy.contains(child_secret));
}
