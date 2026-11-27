//! End-to-end 3-protocol HTTP capture integration tests for multimodal images (`MM03`–`MM05`):
//! - `MM03`: User image wire formats across Anthropic Messages, OpenAI Chat Completions,
//!   and OpenAI Responses, including exact 5 MiB (`5_242_880` bytes) boundary without padding
//!   degradation, multi-turn reopen persistence, and compaction summary omission.
//! - `MM04`: MCP `ToolResult.media` (`artifact:<uuid>`) wire formats across all 3 protocols.
//! - `MM05`: `fork` + source deletion independence over HTTP, sidecar `export_session_materials`
//!   + `load_with_media` round-trip, and fail-closed counterexamples.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use base64::Engine as _;
use causa::kernel::BlockContent;
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tars_core::application::{ClosePolicy, PreparedCreate, SessionHost, text_input};
use tars_core::definition::{
    CredentialRef, McpApproval, McpServerConfig, Provider, SessionDefinition,
};
use tars_core::material::{load, load_with_media};
use tars_core::media::{MAX_IMAGE_BYTES, MediaError, prepare_image_from_path};
use tars_core::permission::PermissionMode;
use tars_core::prelude::PreludeDefinition;
use tars_core::session::{SubmitRequest, WorkState};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const MAX_CAPTURE_BODY_BYTES: usize = 16 * 1024 * 1024;

const PROTOCOLS: [(&str, Provider); 3] = [
    ("openai-chat", Provider::OpenAiChat),
    ("openai-responses", Provider::OpenAiResponses),
    ("anthropic", Provider::Anthropic),
];

#[derive(Clone, Debug)]
enum WireReply {
    Answer(&'static str),
    ToolCall { name: String, arguments: Value },
    Summary(&'static str),
}

struct WireFixture {
    endpoint: String,
    raw_requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl WireFixture {
    fn new(provider: Provider, replies: Vec<WireReply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let path = match provider {
            Provider::OpenAiChat => "/v1/chat/completions",
            Provider::OpenAiResponses => "/v1/responses",
            Provider::Anthropic => "/v1/messages",
        };
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let raw_requests = Arc::new(Mutex::new(Vec::new()));
        let captured = raw_requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();

        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            for (index, reply) in replies.into_iter().enumerate() {
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
                        Err(err) => panic!("wire fixture accept failed: {err}"),
                    }
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();

                let body = read_http_post_body(&mut socket, path);
                captured.lock().unwrap().push(body);

                let (content_type, response_body) = match reply {
                    WireReply::Summary(text) => ("application/json", summary_body(provider, text)),
                    WireReply::Answer(text) => {
                        ("text/event-stream", sse_answer_body(provider, text))
                    }
                    WireReply::ToolCall { name, arguments } => (
                        "text/event-stream",
                        sse_tool_call_body(provider, &name, &arguments, index),
                    ),
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
            }
        });

        Self {
            endpoint,
            raw_requests,
            stop,
            task: Some(task),
        }
    }

    fn bodies(&self) -> Vec<Value> {
        self.raw_requests
            .lock()
            .unwrap()
            .iter()
            .map(|raw| serde_json::from_slice(raw).unwrap())
            .collect()
    }

    fn request_count(&self) -> usize {
        self.raw_requests.lock().unwrap().len()
    }
}

impl Drop for WireFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
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
    let mut head = [0u8; PREFIX.len()];
    loop {
        match socket.peek(&mut head) {
            Ok(0) | Err(_) => return false,
            Ok(n) if head[..n] != PREFIX[..n] => return false,
            Ok(n) if n == PREFIX.len() => return true,
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

fn read_http_post_body(socket: &mut TcpStream, expected_path: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    let (header_end, content_length) = loop {
        let mut chunk = [0u8; 65_536];
        let count = socket.read(&mut chunk).expect("read HTTP request");
        assert!(count > 0, "unexpected EOF reading HTTP headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(
            bytes.len() <= MAX_CAPTURE_BODY_BYTES,
            "captured request exceeded {MAX_CAPTURE_BODY_BYTES} bytes"
        );
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..pos]).unwrap();
            assert!(
                headers.starts_with(&format!("POST {expected_path} HTTP/1.1")),
                "unexpected request line: {headers}"
            );
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("missing Content-Length header");
            assert!(
                length <= MAX_CAPTURE_BODY_BYTES,
                "Content-Length {length} exceeded {MAX_CAPTURE_BODY_BYTES}"
            );
            break (pos + 4, length);
        }
    };
    while bytes.len() < header_end + content_length {
        let mut chunk = [0u8; 65_536];
        let count = socket.read(&mut chunk).expect("read HTTP body");
        assert!(count > 0, "unexpected EOF reading HTTP body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    bytes[header_end..header_end + content_length].to_vec()
}

fn sse_data(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn summary_body(provider: Provider, text: &str) -> String {
    match provider {
        Provider::OpenAiChat => json!({
            "choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]
        }),
        Provider::OpenAiResponses => json!({
            "id": "summary",
            "status": "completed",
            "output": [{
                "id": "message",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }]
        }),
        Provider::Anthropic => json!({
            "id": "summary",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn"
        }),
    }
    .to_string()
}

fn sse_answer_body(provider: Provider, text: &str) -> String {
    match provider {
        Provider::OpenAiChat => {
            sse_data(
                json!({"id":"chat","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
            ) + &sse_data(
                json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + "data: [DONE]\n\n"
        }
        Provider::OpenAiResponses => {
            let item = json!({
                "id": "message",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            });
            sse_data(
                json!({"type":"response.created","response":{"id":"response","status":"in_progress","output":[]}}),
            ) + &sse_data(
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message","type":"message","role":"assistant","content":[]}}),
            ) + &sse_data(
                json!({"type":"response.content_part.added","output_index":0,"item_id":"message","content_index":0,"part":{"type":"output_text","text":""}}),
            ) + &sse_data(
                json!({"type":"response.output_text.delta","output_index":0,"item_id":"message","content_index":0,"delta":text}),
            ) + &sse_data(
                json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[item]}}),
            )
        }
        Provider::Anthropic => {
            sse_data(
                json!({"type":"message_start","message":{"id":"message","content":[],"stop_reason":null}}),
            ) + &sse_data(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ) + &sse_data(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            ) + &sse_data(json!({"type":"content_block_stop","index":0}))
                + &sse_data(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
                + &sse_data(json!({"type":"message_stop"}))
        }
    }
}

fn sse_tool_call_body(provider: Provider, name: &str, arguments: &Value, index: usize) -> String {
    let args_str = arguments.to_string();
    let call_id = format!("provider-call-{index}");
    match provider {
        Provider::OpenAiChat => {
            let delta = json!({
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": args_str}
                }]
            });
            sse_data(
                json!({"id":"chat","choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            ) + &sse_data(
                json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ) + "data: [DONE]\n\n"
        }
        Provider::OpenAiResponses => {
            let item = json!({
                "type": "function_call",
                "id": call_id,
                "call_id": call_id,
                "name": name,
                "arguments": args_str
            });
            sse_data(
                json!({"type":"response.created","response":{"id":"response","status":"in_progress","output":[]}}),
            ) + &sse_data(
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":call_id,"call_id":call_id,"name":name,"arguments":""}}),
            ) + &sse_data(
                json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":call_id,"delta":args_str}),
            ) + &sse_data(
                json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[item]}}),
            )
        }
        Provider::Anthropic => {
            sse_data(
                json!({"type":"message_start","message":{"id":"message","content":[],"stop_reason":null}}),
            ) + &sse_data(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
            ) + &sse_data(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args_str}}),
            ) + &sse_data(json!({"type":"content_block_stop","index":0}))
                + &sse_data(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}))
                + &sse_data(json!({"type":"message_stop"}))
        }
    }
}

struct McpHttpImageServer {
    url: String,
    stop: Arc<AtomicBool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl McpHttpImageServer {
    async fn start(image_b64: String) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/mcp");
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();

        let task = tokio::spawn(async move {
            loop {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let accept =
                    tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                let Ok(Ok((mut socket, _))) = accept else {
                    continue;
                };
                let b64 = image_b64.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 16_384];
                    let header_end = loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let (method, content_length) = {
                        let headers = String::from_utf8_lossy(&buf[..header_end]);
                        let first_line = headers.lines().next().unwrap_or("");
                        let method = first_line
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_owned();
                        let content_length: usize = headers
                            .lines()
                            .find_map(|line| {
                                let (name, val) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| val.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        (method, content_length)
                    };
                    while buf.len() < header_end + content_length {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }

                    let resp = match method.as_str() {
                        "GET" => "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                        "DELETE" => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                        "POST" => {
                            let req: Value = serde_json::from_slice(&buf[header_end..header_end + content_length]).unwrap();
                            let rpc_method = req["method"].as_str().unwrap_or("");
                            let id = req.get("id").cloned();
                            if let Some(id) = id
                                .filter(|_| !rpc_method.starts_with("notifications/"))
                            {
                                let result = match rpc_method {
                                    "initialize" => json!({
                                        "protocolVersion": "2025-03-26",
                                        "capabilities": { "tools": {} },
                                        "serverInfo": { "name": "mcp-image-fixture", "version": "1.0.0" }
                                    }),
                                    "tools/list" => json!({
                                        "tools": [{
                                            "name": "snap",
                                            "description": "Returns a PNG image",
                                            "inputSchema": { "type": "object" }
                                        }]
                                    }),
                                    "tools/call" => json!({
                                        "content": [
                                            { "type": "text", "text": "captured png" },
                                            { "type": "image", "data": b64, "mimeType": "image/png" }
                                        ],
                                        "isError": false
                                    }),
                                    _ => json!({}),
                                };
                                let payload = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": result
                                })
                                .to_string();
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: mcp-wire-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                                    payload.len()
                                )
                            } else {
                                "HTTP/1.1 202 Accepted\r\nMcp-Session-Id: mcp-wire-session\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                            }
                        }
                        _ => "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                    };
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        Self {
            url,
            stop,
            task: Some(task),
        }
    }
}

impl Drop for McpHttpImageServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn base_definition(provider: Provider, endpoint: &str) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "multimodal-wire-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 4,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: Some(PermissionMode::FullAccess),
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: false,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: PreludeDefinition {
            agents_enabled: false,
            ..Default::default()
        },
        code_execution: None,
    }
}

fn png_bytes(len: usize, seed: u8) -> Vec<u8> {
    assert!(len >= 8);
    let mut bytes = Vec::with_capacity(len);
    bytes.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    for idx in 8..len {
        bytes.push(seed.wrapping_add((idx % 251) as u8));
    }
    bytes
}

fn jpeg_bytes(len: usize, seed: u8) -> Vec<u8> {
    assert!(len >= 12);
    let mut bytes = vec![seed; len];
    bytes[..11].copy_from_slice(b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00");
    bytes
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn decode_b64(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .expect("valid base64 image payload")
}

/// Extract all inline base64 image payloads (decoded raw bytes) rendered for user messages
/// according to each protocol's wire contract.
fn extract_user_image_bytes(provider: Provider, body: &Value) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    match provider {
        Provider::Anthropic => {
            for msg in body["messages"].as_array().unwrap() {
                if msg["role"] != "user" {
                    continue;
                }
                if let Some(parts) = msg["content"].as_array() {
                    for part in parts {
                        if part["type"] == "image" {
                            let source = &part["source"];
                            assert_eq!(source["type"], "base64");
                            let mime = source["media_type"].as_str().unwrap().to_owned();
                            let raw = decode_b64(source["data"].as_str().unwrap());
                            found.push((mime, raw));
                        }
                    }
                }
            }
        }
        Provider::OpenAiChat => {
            for msg in body["messages"].as_array().unwrap() {
                if msg["role"] != "user" {
                    continue;
                }
                if let Some(parts) = msg["content"].as_array() {
                    for part in parts {
                        if part["type"] == "image_url" {
                            let url = part["image_url"]["url"].as_str().unwrap();
                            let rest = url.strip_prefix("data:").expect("data URL");
                            let (mime, b64) = rest.split_once(";base64,").expect("base64 data URL");
                            found.push((mime.to_owned(), decode_b64(b64)));
                        }
                    }
                }
            }
        }
        Provider::OpenAiResponses => {
            for item in body["input"].as_array().unwrap() {
                if item["role"] != "user" {
                    continue;
                }
                if let Some(parts) = item["content"].as_array() {
                    for part in parts {
                        if part["type"] == "input_image" {
                            let url = part["image_url"].as_str().unwrap();
                            let rest = url.strip_prefix("data:").expect("data URL");
                            let (mime, b64) = rest.split_once(";base64,").expect("base64 data URL");
                            found.push((mime.to_owned(), decode_b64(b64)));
                        }
                    }
                }
            }
        }
    }
    found
}

/// Extract image bytes emitted from `ToolResult.media` according to each protocol's contract.
fn extract_tool_result_image_bytes(provider: Provider, body: &Value) -> Vec<(String, Vec<u8>)> {
    let mut found = Vec::new();
    match provider {
        Provider::Anthropic => {
            // Anthropic embeds media blocks directly inside `tool_result.content`.
            for msg in body["messages"].as_array().unwrap() {
                if msg["role"] != "user" {
                    continue;
                }
                for part in msg["content"].as_array().into_iter().flatten() {
                    if part["type"] == "tool_result" {
                        for inner in part["content"].as_array().into_iter().flatten() {
                            if inner["type"] == "image" {
                                let source = &inner["source"];
                                assert_eq!(source["type"], "base64");
                                let mime = source["media_type"].as_str().unwrap().to_owned();
                                let raw = decode_b64(source["data"].as_str().unwrap());
                                found.push((mime, raw));
                            }
                        }
                    }
                }
            }
        }
        Provider::OpenAiChat => {
            // OpenAI Chat hoists tool result media to a following `role: "user"` message.
            let messages = body["messages"].as_array().unwrap();
            for pair in messages.windows(2) {
                if pair[0]["role"] == "tool" && pair[1]["role"] == "user" {
                    for part in pair[1]["content"].as_array().into_iter().flatten() {
                        if part["type"] == "image_url" {
                            let url = part["image_url"]["url"].as_str().unwrap();
                            let rest = url.strip_prefix("data:").expect("data URL");
                            let (mime, b64) = rest.split_once(";base64,").expect("base64 data URL");
                            found.push((mime.to_owned(), decode_b64(b64)));
                        }
                    }
                }
            }
        }
        Provider::OpenAiResponses => {
            // OpenAI Responses embeds `input_image` inside `function_call_output.output`.
            for item in body["input"].as_array().unwrap() {
                if item["type"] == "function_call_output" {
                    for inner in item["output"].as_array().into_iter().flatten() {
                        if inner["type"] == "input_image" {
                            let url = inner["image_url"].as_str().unwrap();
                            let rest = url.strip_prefix("data:").expect("data URL");
                            let (mime, b64) = rest.split_once(";base64,").expect("base64 data URL");
                            found.push((mime.to_owned(), decode_b64(b64)));
                        }
                    }
                }
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm03_three_protocol_user_image_exact_5mib_and_reopen() {
    assert_eq!(MAX_IMAGE_BYTES, 5_242_880);
    let temp = tempfile::tempdir().unwrap();
    let exact_5mib = png_bytes(MAX_IMAGE_BYTES, 0x3c);
    let img_path = write_file(temp.path(), "exact-5mib.png", &exact_5mib);
    let prepared = prepare_image_from_path(&img_path).unwrap();
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&exact_5mib);

    for (name, provider) in PROTOCOLS {
        let store = temp.path().join(format!("store-{name}"));
        let host = SessionHost::new(Some(store));
        let fixture = WireFixture::new(
            provider,
            vec![
                WireReply::Answer("saw exact 5mib image"),
                WireReply::Answer("follow-up after reopen"),
            ],
        );

        let def = base_definition(provider, &fixture.endpoint);
        let session = host
            .create(PreparedCreate::prepare_persistent(def, None).unwrap())
            .await
            .unwrap();
        let session_id = session.target().conversation_id().clone();

        // Turn 1: submit exact 5_242_880-byte PNG.
        let work1 = session
            .submit_with_images(
                format!("turn1-{name}"),
                Some("Inspect exact 5 MiB image"),
                std::slice::from_ref(&prepared),
            )
            .unwrap()
            .work;
        let obs1 = session.wait(&work1, Duration::from_secs(15)).await.unwrap();
        assert!(
            matches!(obs1.state, WorkState::Finished(_)),
            "{name}: {obs1:?}"
        );
        session.wait_saved().await.unwrap();

        // Close and reopen, then submit Turn 2 without re-attaching the image.
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        let reopened = host.open(session_id).await.unwrap();
        assert!(
            reopened.media_vault().contains(&prepared.reference),
            "{name}"
        );

        let work2 = reopened
            .submit(SubmitRequest {
                request_key: format!("turn2-{name}"),
                blocks: vec![text_input("Follow-up after reopen")],
            })
            .unwrap()
            .work;
        let obs2 = reopened
            .wait(&work2, Duration::from_secs(15))
            .await
            .unwrap();
        assert!(
            matches!(obs2.state, WorkState::Finished(_)),
            "{name}: {obs2:?}"
        );
        reopened.wait_saved().await.unwrap();

        let bodies = fixture.bodies();
        assert_eq!(bodies.len(), 2, "{name}: expected 2 HTTP requests");

        for (turn_idx, body) in bodies.iter().enumerate() {
            let images = extract_user_image_bytes(provider, body);
            assert_eq!(
                images.len(),
                1,
                "{name} turn {turn_idx}: exact 5 MiB image must NOT degrade to text placeholder"
            );
            assert_eq!(images[0].0, "image/png", "{name} turn {turn_idx}");
            assert_eq!(images[0].1.len(), MAX_IMAGE_BYTES, "{name} turn {turn_idx}");
            assert!(
                images[0].1 == exact_5mib,
                "{name} turn {turn_idx}: decoded bytes must match exact 5 MiB PNG"
            );
        }

        // Also assert the raw JSON wire body directly carries the exact base64 string.
        let raw_turn1 = String::from_utf8(fixture.raw_requests.lock().unwrap()[0].clone()).unwrap();
        assert!(raw_turn1.contains(&expected_b64), "{name}: raw turn 1");
        let raw_turn2 = String::from_utf8(fixture.raw_requests.lock().unwrap()[1].clone()).unwrap();
        assert!(
            raw_turn2.contains(&expected_b64),
            "{name}: raw turn 2 after reopen"
        );

        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm03_compaction_summary_gateway_omits_raw_image_bytes_and_preserves_history() {
    let temp = tempfile::tempdir().unwrap();
    let sample_png = png_bytes(256, 0x71);
    let img_path = write_file(temp.path(), "compact-pic.png", &sample_png);
    let prepared = prepare_image_from_path(&img_path).unwrap();
    let sample_b64 = base64::engine::general_purpose::STANDARD.encode(&sample_png);

    for (name, provider) in PROTOCOLS {
        let store = temp.path().join(format!("compact-store-{name}"));
        let host = SessionHost::new(Some(store));
        let fixture = WireFixture::new(
            provider,
            vec![
                WireReply::Answer("noted image and context"),
                WireReply::Answer("noted follow-up"),
                WireReply::Summary(
                    "Brief: user shared compact-pic.png and discussed earlier notes.",
                ),
                WireReply::Answer("post-compaction answer"),
            ],
        );

        let mut def = base_definition(provider, &fixture.endpoint);
        def.context_window = Some(12_000);
        let session = host
            .create(PreparedCreate::prepare_persistent(def, None).unwrap())
            .await
            .unwrap();
        let session_id = session.target().conversation_id().clone();

        // Turn 1: image + substantial text so summarization shrinks context.
        let w1 = session
            .submit_with_images(
                format!("c-turn1-{name}"),
                Some(&"earlier architecture notes ".repeat(80)),
                std::slice::from_ref(&prepared),
            )
            .unwrap()
            .work;
        let obs1 = session.wait(&w1, Duration::from_secs(15)).await.unwrap();
        assert!(matches!(obs1.state, WorkState::Finished(_)), "{name}");
        session.wait_saved().await.unwrap();

        // Turn 2: recent turn retained in tail.
        let w2 = session
            .submit(SubmitRequest {
                request_key: format!("c-turn2-{name}"),
                blocks: vec![text_input("keep this recent turn")],
            })
            .unwrap()
            .work;
        let obs2 = session.wait(&w2, Duration::from_secs(15)).await.unwrap();
        assert!(matches!(obs2.state, WorkState::Finished(_)), "{name}");
        session.wait_saved().await.unwrap();

        // Trigger manual compaction -> sends HTTP summary request (request index 2).
        let outcome = session.compact().await.unwrap();
        assert!(
            outcome.estimate_after < outcome.estimate_before,
            "{name}: {outcome:?}"
        );

        let raw_summary =
            String::from_utf8(fixture.raw_requests.lock().unwrap()[2].clone()).unwrap();
        assert!(
            !raw_summary.contains(&sample_b64),
            "{name}: compaction summary request must NOT upload raw base64 image bytes"
        );
        let summary_json: Value = serde_json::from_str(&raw_summary).unwrap();
        assert!(
            extract_user_image_bytes(provider, &summary_json).is_empty(),
            "{name}: summary request must not contain inline image blocks"
        );
        assert!(
            raw_summary.contains("compact-pic.png") || raw_summary.contains("image/png"),
            "{name}: summary request should carry lightweight text reference"
        );

        // Close and reopen: archived image in history/artifacts remains intact!
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        let reopened = host.open(session_id).await.unwrap();
        assert!(!reopened.history().is_empty(), "{name}: history preserved");
        assert!(
            reopened.media_vault().contains(&prepared.reference),
            "{name}: archived image preserved in MediaVault/artifacts on reopen"
        );

        let w3 = reopened
            .submit(SubmitRequest {
                request_key: format!("c-turn3-{name}"),
                blocks: vec![text_input("post-compaction turn")],
            })
            .unwrap()
            .work;
        let obs3 = reopened.wait(&w3, Duration::from_secs(15)).await.unwrap();
        assert!(matches!(obs3.state, WorkState::Finished(_)), "{name}");
        reopened.wait_saved().await.unwrap();
        assert_eq!(fixture.request_count(), 4, "{name}");

        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm04_mcp_tool_result_media_three_protocol_wire_payload() {
    let temp = tempfile::tempdir().unwrap();
    let mcp_png = png_bytes(128, 0x44);
    let mcp_b64 = base64::engine::general_purpose::STANDARD.encode(&mcp_png);
    let mcp_server = McpHttpImageServer::start(mcp_b64.clone()).await;

    for (name, provider) in PROTOCOLS {
        let store = temp.path().join(format!("mcp-store-{name}"));
        let host = SessionHost::new(Some(store));
        let fixture = WireFixture::new(
            provider,
            vec![
                WireReply::ToolCall {
                    name: "mcp_cam_snap".to_owned(),
                    arguments: json!({}),
                },
                WireReply::Answer("observed mcp screenshot"),
                WireReply::Answer("reopened mcp follow-up"),
            ],
        );

        let mut def = base_definition(provider, &fixture.endpoint);
        def.mcp_servers = vec![McpServerConfig {
            id: "cam".into(),
            command: None,
            args: Vec::new(),
            url: Some(mcp_server.url.clone()),
            env: Default::default(),
            headers: Default::default(),
            startup_timeout_sec: 10,
            tool_timeout_sec: 10,
            enabled_tools: None,
            disabled_tools: None,
            approval: McpApproval::Approve,
            tools: Default::default(),
            required: true,
        }];

        let session = host
            .create(PreparedCreate::prepare_persistent(def, None).unwrap())
            .await
            .unwrap();
        let session_id = session.target().conversation_id().clone();

        // Turn 1: model invokes MCP tool `mcp_cam_snap`, which stores `artifact:<uuid>` in SQLite
        // `artifacts` and attaches it to `ToolResultPayload.media`, then round 2 sends it to model.
        let w1 = session
            .submit(SubmitRequest {
                request_key: format!("mcp-turn1-{name}"),
                blocks: vec![text_input("Call mcp_cam_snap and inspect image")],
            })
            .unwrap()
            .work;
        let obs1 = session.wait(&w1, Duration::from_secs(15)).await.unwrap();
        assert!(
            matches!(obs1.state, WorkState::Finished(_)),
            "{name}: {obs1:?}"
        );
        session.wait_saved().await.unwrap();

        // Verify persisted ToolResult.media carries `artifact:<uuid>`.
        let materials = session.read_materials().unwrap();
        let tool_media: Vec<_> = materials
            .context
            .blocks()
            .iter()
            .filter_map(|b| match b.content() {
                BlockContent::ToolResult(payload) => Some(&payload.media),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(tool_media.len(), 1, "{name}");
        assert_eq!(tool_media[0].media_type, "image/png", "{name}");
        assert!(
            tool_media[0].reference.starts_with("artifact:"),
            "{name}: expected artifact:<uuid>, got {}",
            tool_media[0].reference
        );

        let bodies = fixture.bodies();
        assert_eq!(
            bodies.len(),
            2,
            "{name}: round 0 tool call + round 1 tool result"
        );
        let round1_media = extract_tool_result_image_bytes(provider, &bodies[1]);
        assert_eq!(
            round1_media.len(),
            1,
            "{name}: expected 1 tool result image in follow-up HTTP request: {}",
            bodies[1]
        );
        assert_eq!(round1_media[0].0, "image/png", "{name}");
        assert!(
            round1_media[0].1 == mcp_png,
            "{name}: decoded MCP ToolResult.media bytes must match original PNG"
        );

        // Reopen and submit Turn 2: `artifact:<uuid>` in historical ToolResult must still resolve!
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        let reopened = host.open(session_id).await.unwrap();
        let w2 = reopened
            .submit(SubmitRequest {
                request_key: format!("mcp-turn2-{name}"),
                blocks: vec![text_input("Summarize the captured MCP image")],
            })
            .unwrap()
            .work;
        let obs2 = reopened.wait(&w2, Duration::from_secs(15)).await.unwrap();
        assert!(matches!(obs2.state, WorkState::Finished(_)), "{name}");
        reopened.wait_saved().await.unwrap();

        let bodies_after = fixture.bodies();
        assert_eq!(bodies_after.len(), 3, "{name}");
        let reopened_media = extract_tool_result_image_bytes(provider, &bodies_after[2]);
        assert_eq!(reopened_media.len(), 1, "{name}");
        assert!(reopened_media[0].1 == mcp_png, "{name}");

        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm05_fork_delete_sidecar_roundtrip_and_counterexamples() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let host = SessionHost::new(Some(store.clone()));

    let png_a = png_bytes(96, 0x11);
    let jpg_b = jpeg_bytes(110, 0x22);
    let png_path = write_file(temp.path(), "a.png", &png_a);
    let jpg_path = write_file(temp.path(), "b.jpg", &jpg_b);
    let img_a = prepare_image_from_path(&png_path).unwrap();
    let img_b = prepare_image_from_path(&jpg_path).unwrap();

    // Counterexample 1: spoofed extension (`.png` containing non-image/SVG bytes) rejected before HTTP.
    let spoofed_path = write_file(
        temp.path(),
        "spoof.png",
        b"<svg xmlns='http://www.w3.org/2000/svg'/>",
    );
    assert!(matches!(
        prepare_image_from_path(&spoofed_path),
        Err(MediaError::UnsupportedFormat)
    ));

    // Counterexample 2: 5_242_881-byte image rejected before HTTP.
    let oversized_path = write_file(
        temp.path(),
        "too-big.png",
        &png_bytes(MAX_IMAGE_BYTES + 1, 0x55),
    );
    assert!(matches!(
        prepare_image_from_path(&oversized_path),
        Err(MediaError::FileTooLarge {
            size_bytes,
            max_bytes: MAX_IMAGE_BYTES
        }) if size_bytes == (MAX_IMAGE_BYTES + 1) as u64
    ));

    let fixture = WireFixture::new(
        Provider::Anthropic,
        vec![
            WireReply::Answer("source turn 1"),
            WireReply::Answer("fork turn 2"),
        ],
    );
    let def = base_definition(Provider::Anthropic, &fixture.endpoint);
    let source = host
        .create(PreparedCreate::prepare_persistent(def, None).unwrap())
        .await
        .unwrap();
    let source_id = source.target().conversation_id().clone();

    let w1 = source
        .submit_with_images("src-t1", None, std::slice::from_ref(&img_a))
        .unwrap()
        .work;
    let obs1 = source.wait(&w1, Duration::from_secs(15)).await.unwrap();
    assert!(matches!(obs1.state, WorkState::Finished(_)));
    source.wait_saved().await.unwrap();
    assert!(source.title().is_none());

    // Fork while open idle, then close and delete source session completely.
    let fork_id = host
        .fork(source_id.clone(), Some("Visual Fork".into()))
        .await
        .unwrap();
    host.close(&source.target(), ClosePolicy::Save)
        .await
        .unwrap();
    host.delete(source_id).await.unwrap();

    // Reopen fork and submit Turn 2 with img_b over HTTP.
    let forked = host.open(fork_id.clone()).await.unwrap();
    let w2 = forked
        .submit_with_images("fork-t2", Some("turn 2 jpg"), std::slice::from_ref(&img_b))
        .unwrap()
        .work;
    let obs2 = forked.wait(&w2, Duration::from_secs(15)).await.unwrap();
    assert!(matches!(obs2.state, WorkState::Finished(_)));
    forked.wait_saved().await.unwrap();

    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 2);
    let fork_turn2_imgs = extract_user_image_bytes(Provider::Anthropic, &bodies[1]);
    assert_eq!(fork_turn2_imgs.len(), 2);
    assert_eq!(fork_turn2_imgs[0], ("image/png".to_owned(), png_a.clone()));
    assert_eq!(fork_turn2_imgs[1], ("image/jpeg".to_owned(), jpg_b.clone()));

    host.close(&forked.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Sidecar export_session_materials + load_with_media round-trip (and fail-closed checks).
    let export_path = temp.path().join("exported.json");
    let exported = host
        .export_session_materials(&fork_id, &export_path)
        .await
        .unwrap();
    assert_eq!(exported.media_count, 2);
    let media_dir = exported.media_dir.expect("sidecar .media dir");
    assert!(media_dir.is_dir());

    // Default `material::load` (without sidecar) still fails closed on managed media refs.
    assert!(load(&export_path).is_err());

    // `material::load_with_media` succeeds and preserves exact bytes.
    let (_ctx, blobs) = load_with_media(&export_path).unwrap();
    assert_eq!(blobs.get(&img_a.reference).unwrap(), &png_a);
    assert_eq!(blobs.get(&img_b.reference).unwrap(), &jpg_b);

    // Tampering a sidecar file fails `load_with_media` closed.
    let hex_a = img_a
        .reference
        .strip_prefix("media:sha256:")
        .expect("sha256 ref");
    let sidecar_a = media_dir.join(format!("{hex_a}.bin"));
    let mut tampered_sidecar = fs::read(&sidecar_a).unwrap();
    *tampered_sidecar.last_mut().unwrap() ^= 0xFF;
    fs::write(&sidecar_a, &tampered_sidecar).unwrap();
    assert!(load_with_media(&export_path).is_err());

    // Counterexample 3: tampering SQLite `artifacts` BLOB fails closed on reopen (0 HTTP calls).
    let count_before_tamper = fixture.request_count();
    let db = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    let mut tampered_db_blob = png_a.clone();
    *tampered_db_blob.last_mut().unwrap() ^= 0xFF;
    db.execute(
        "UPDATE artifacts SET content = ?3
         WHERE session_id = ?1 AND agent_id = ?1 AND artifact_id = ?2",
        rusqlite::params![fork_id.as_str(), &img_a.reference, &tampered_db_blob],
    )
    .unwrap();
    assert!(host.open(fork_id.clone()).await.is_err());
    assert_eq!(fixture.request_count(), count_before_tamper);

    // Deleting SQLite `artifacts` BLOB also fails closed on reopen (0 HTTP calls).
    db.execute(
        "DELETE FROM artifacts
         WHERE session_id = ?1 AND agent_id = ?1 AND artifact_id = ?2",
        rusqlite::params![fork_id.as_str(), &img_a.reference],
    )
    .unwrap();
    assert!(host.open(fork_id).await.is_err());
    assert_eq!(fixture.request_count(), count_before_tamper);
}
