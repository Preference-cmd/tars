//! Real wire messages; fixture outputs never substitute for filesystem results.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}
pub fn tool(provider: &str, name: &str, args: Value, id: &str) -> String {
    let arguments = args.to_string();
    match provider {
        "openai-chat" => {
            data(
                json!({"id":"chat1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments}}]},"finish_reason":null}]}),
            ) + &data(
                json!({"id":"chat1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            ) + "data: [DONE]\n\n"
        }
        "openai-responses" => {
            data(
                json!({"type":"response.created","response":{"id":"response1","status":"in_progress","output":[]}}),
            ) + &data(
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":id,"call_id":id,"name":name,"arguments":""}}),
            ) + &data(
                json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":id,"delta":arguments}),
            ) + &data(
                json!({"type":"response.completed","response":{"id":"response1","status":"completed","output":[{"type":"function_call","id":id,"call_id":id,"name":name,"arguments":arguments}]}}),
            )
        }
        "anthropic" => {
            data(
                json!({"type":"message_start","message":{"id":"message1","content":[],"stop_reason":null}}),
            ) + &data(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            ) + &data(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
            ) + &data(json!({"type":"content_block_stop","index":0}))
                + &data(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}))
                + &data(json!({"type":"message_stop"}))
        }
        _ => unreachable!(),
    }
}
pub fn answer(provider: &str) -> String {
    let text = "recovery.rs:2 retry original material";
    match provider {
        "openai-chat" => {
            data(
                json!({"id":"chat1","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
            ) + &data(
                json!({"id":"chat1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + "data: [DONE]\n\n"
        }
        "openai-responses" => {
            data(
                json!({"type":"response.created","response":{"id":"final","status":"in_progress","output":[]}}),
            ) + &data(
                json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","content":[]}}),
            ) + &data(
                json!({"type":"response.content_part.added","output_index":0,"item_id":"msg","content_index":0,"part":{"type":"output_text","text":""}}),
            ) + &data(
                json!({"type":"response.output_text.delta","output_index":0,"item_id":"msg","content_index":0,"delta":text}),
            ) + &data(
                json!({"type":"response.completed","response":{"id":"final","status":"completed","output":[{"id":"msg","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}]}}),
            )
        }
        "anthropic" => {
            data(json!({"type":"message_start","message":{"content":[]}}))
                + &data(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                )
                + &data(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                )
                + &data(json!({"type":"content_block_stop","index":0}))
                + &data(json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}))
                + &data(json!({"type":"message_stop"}))
        }
        _ => unreachable!(),
    }
}
pub struct Fixture {
    pub endpoint: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub release: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    pub fn new(provider: &str, paused: bool) -> Self {
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
        let release = Arc::new(AtomicBool::new(!paused));
        let gate = release.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let provider = provider.to_owned();
        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for round in 0..3 {
                let mut socket = loop {
                    if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5))
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                // macOS accepted streams inherit the nonblocking listener flag;
                // clear it before applying a blocking read timeout.
                socket.set_nonblocking(false).unwrap();
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
                        break (end + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).unwrap();
                    if count == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..count]);
                }
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::from_slice(&bytes[start..start + length]).unwrap());
                if round == 2 {
                    while !gate.load(Ordering::SeqCst) {
                        if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
                let body = match round {
                    0 => tool(
                        &provider,
                        "search_text",
                        json!({"path":".","query":"retry original","max_matches":10}),
                        "search1",
                    ),
                    1 => tool(
                        &provider,
                        "read_file",
                        json!({"path":"recovery.rs","start_line":2,"max_lines":1}),
                        "read1",
                    ),
                    _ => answer(&provider),
                };
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
            release,
            stop,
            task: Some(task),
        }
    }
    pub fn finish(&self) {
        self.release.store(true, Ordering::SeqCst);
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
