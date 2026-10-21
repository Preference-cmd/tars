//! Local SSE model responses; filesystem tool output always comes from Tarz.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ScriptedToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Reply {
    Read(&'static str),
    Answer,
    Call(String, Value),
    Calls(Vec<ScriptedToolCall>),
}

fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn body(provider: &str, reply: Reply, index: usize) -> String {
    let calls_list: Vec<(String, String)> = match reply {
        Reply::Read(path) => vec![(
            "read_file".to_string(),
            json!({"path": path, "start_line": 1, "max_lines": 1}).to_string(),
        )],
        Reply::Answer => Vec::new(),
        Reply::Call(name, args) => vec![(name, args.to_string())],
        Reply::Calls(calls) => calls
            .into_iter()
            .map(|c| (c.name, c.arguments.to_string()))
            .collect(),
    };
    let text = "checked workspace";
    let is_tool = !calls_list.is_empty();
    let id = format!("provider-call-{index}");

    let (tool_name, arguments) = calls_list
        .first()
        .cloned()
        .unwrap_or_else(|| (String::new(), String::new()));
    let tool = !tool_name.is_empty();

    match provider {
        "openai-chat" => {
            let delta = if is_tool {
                let deltas: Vec<Value> = calls_list
                    .iter()
                    .enumerate()
                    .map(|(call_idx, (name, args))| {
                        let call_id = format!("provider-call-{index}-{call_idx}");
                        json!({
                            "index": call_idx,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args,
                            }
                        })
                    })
                    .collect();
                json!({"tool_calls": deltas})
            } else {
                json!({"content": text})
            };
            data(json!({"id":"chat","choices":[{"index":0,"delta":delta,"finish_reason":null}]}))
                + &data(
                    json!({"id":"chat","choices":[{"index":0,"delta":{},"finish_reason":if is_tool {"tool_calls"} else {"stop"}}]}),
                )
                + "data: [DONE]\n\n"
        }
        "openai-responses" => {
            let item = if tool {
                json!({"type":"function_call","id":id,"call_id":id,"name":tool_name,"arguments":arguments})
            } else {
                json!({"id":"message","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})
            };
            let added = if tool {
                json!({"type":"function_call","id":id,"call_id":id,"name":tool_name,"arguments":""})
            } else {
                json!({"id":"message","type":"message","role":"assistant","content":[]})
            };
            let start = data(
                json!({"type":"response.created","response":{"id":"response","status":"in_progress","output":[]}}),
            ) + &data(
                json!({"type":"response.output_item.added","output_index":0,"item":added}),
            );
            let delta = if tool {
                data(
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":id,"delta":arguments}),
                )
            } else {
                data(
                    json!({"type":"response.content_part.added","output_index":0,"item_id":"message","content_index":0,"part":{"type":"output_text","text":""}}),
                ) + &data(
                    json!({"type":"response.output_text.delta","output_index":0,"item_id":"message","content_index":0,"delta":text}),
                )
            };
            start
                + &delta
                + &data(
                    json!({"type":"response.completed","response":{"id":"response","status":"completed","output":[item]}}),
                )
        }
        "anthropic" => {
            let block = if tool {
                json!({"type":"tool_use","id":id,"name":tool_name,"input":{}})
            } else {
                json!({"type":"text","text":""})
            };
            let delta = if tool {
                json!({"type":"input_json_delta","partial_json":arguments})
            } else {
                json!({"type":"text_delta","text":text})
            };
            data(
                json!({"type":"message_start","message":{"id":"message","content":[],"stop_reason":null}}),
            ) + &data(json!({"type":"content_block_start","index":0,"content_block":block}))
                + &data(json!({"type":"content_block_delta","index":0,"delta":delta}))
                + &data(json!({"type":"content_block_stop","index":0}))
                + &data(
                    json!({"type":"message_delta","delta":{"stop_reason":if tool {"tool_use"} else {"end_turn"}}}),
                )
                + &data(json!({"type":"message_stop"}))
        }
        _ => unreachable!(),
    }
}

#[allow(dead_code)]
pub struct Fixture {
    pub endpoint: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub accepted: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
#[allow(dead_code)]
impl Fixture {
    pub fn new(provider: &str, replies: Vec<Reply>, pause_at: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let path = match provider {
            "openai-chat" => "/v1/chat/completions",
            "openai-responses" => "/v1/responses",
            _ => "/v1/messages",
        };
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_count = accepted.clone();
        let release = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let provider = provider.to_owned();
        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            for (index, reply) in replies.into_iter().enumerate() {
                let mut socket = loop {
                    if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                };
                // On macOS accepted streams inherit the listener's nonblocking
                // flag. A read timeout does not clear it, so configure the socket
                // explicitly before accepting a delayed request body.
                socket.set_nonblocking(false).unwrap();
                accepted_count.fetch_add(1, Ordering::SeqCst);
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (start, length) = loop {
                    let mut chunk = [0; 4096];
                    let count = match socket.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() <= 1024 * 1024, "fixture request bound");
                    if let Some(end) = bytes.windows(4).position(|slice| slice == b"\r\n\r\n") {
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
                        assert!(length <= 1024 * 1024);
                        break (end + 4, length);
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
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&bytes[start..start + length]).unwrap());
                if pause_at == Some(index) {
                    while !gate.load(Ordering::SeqCst) {
                        if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                let body = body(&provider, reply, index);
                // Cancelled requests may close their socket. They still consume
                // exactly this model step; they never substitute another response.
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            endpoint,
            requests,
            accepted,
            release,
            stop,
            task: Some(task),
        }
    }
    pub fn release(&self) {
        self.release.store(true, Ordering::SeqCst);
    }
    pub async fn wait_for_requests(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.requests.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {count} requests: {:?}",
                self.requests.lock().unwrap()
            )
        });
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}
