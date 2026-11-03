//! Real wire messages; fixture outputs never substitute for filesystem results.
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
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

#[allow(dead_code)]
pub fn answer_with_text(provider: &str, text: &str) -> String {
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

/// Adds an OpenAI Chat usage chunk to `stream` just before its end marker.
#[allow(dead_code)]
pub fn with_chat_usage(stream: String, usage: Value) -> String {
    let body = stream
        .strip_suffix("data: [DONE]\n\n")
        .expect("an openai-chat stream");
    body.to_owned() + &data(json!({"id":"chat1","choices":[],"usage":usage})) + "data: [DONE]\n\n"
}

pub fn answer(provider: &str) -> String {
    answer_with_text(provider, "recovery.rs:2 retry original material")
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
        // A CLI process cannot rely on a resolved sandboxed ripgrep, so the
        // scripted round-trip uses file reads. The search tool itself is
        // covered by tars-core's sandboxed search acceptance tests.
        let responses = vec![
            tool(
                provider,
                "read_file",
                json!({"path":"recovery.rs","start_line":1,"max_lines":2}),
                "read0",
            ),
            tool(
                provider,
                "read_file",
                json!({"path":"recovery.rs","start_line":2,"max_lines":1}),
                "read1",
            ),
            answer(provider),
        ];
        Self::with_responses_and_pause(provider, responses, paused)
    }

    #[allow(dead_code)]
    pub fn with_responses(provider: &str, responses: Vec<String>) -> Self {
        Self::with_responses_and_pause(provider, responses, false)
    }

    #[allow(dead_code)]
    pub fn with_responses_and_pause(provider: &str, responses: Vec<String>, paused: bool) -> Self {
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
        let total = responses.len();
        let task = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            for (round, body) in responses.into_iter().enumerate() {
                let mut socket = loop {
                    if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    match listener.accept() {
                        Ok((socket, _)) if is_post(&socket) => break socket,
                        Ok(_) => continue,
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
                if paused && round == total.saturating_sub(1) {
                    while !gate.load(Ordering::SeqCst) {
                        if stopped.load(Ordering::SeqCst) || Instant::now() > deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
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

/// Whether an accepted connection comes from the client under test. Other
/// local programs, such as an editor that scans new ports, may connect to a
/// fixture as well; only the client's requests start with `POST `.
#[allow(dead_code)]
pub fn is_post(socket: &TcpStream) -> bool {
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

/// Accepts the next connection from the client under test, dropping any
/// other local connection instead of letting it take a scripted reply.
#[allow(dead_code)]
pub fn accept_post(listener: &TcpListener) -> std::io::Result<TcpStream> {
    loop {
        let (socket, _) = listener.accept()?;
        if is_post(&socket) {
            return Ok(socket);
        }
    }
}
