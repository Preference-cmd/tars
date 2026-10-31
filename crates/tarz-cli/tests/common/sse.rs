//! Text-only wire fixtures for actual CLI HTTP consumers.

use serde_json::{Value, json};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

pub fn reply(status: u16, body: Value) -> (&'static str, String) {
    if !(200..300).contains(&status) {
        return ("application/json", body.to_string());
    }
    let events = if body.get("choices").is_some() {
        let choice = &body["choices"][0];
        return (
            "text/event-stream",
            event(
                json!({"choices":[{"index":0,"delta":{"content":choice["message"]["content"]},"finish_reason":null}]}),
            ) + &event(
                json!({"choices":[{"index":0,"delta":{},"finish_reason":choice["finish_reason"]}]}),
            ) + "data: [DONE]\n\n",
        );
    } else if body.get("output").is_some() {
        let mut response = body;
        response["id"] = json!("fixture-response");
        response["output"][0]["id"] = json!("fixture-message");
        vec![
            json!({"type":"response.created","response":{"id":"fixture-response","status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"fixture-message","content":[]}}),
            json!({"type":"response.content_part.added","output_index":0,"item_id":"fixture-message","content_index":0,"part":{"type":"output_text","text":""}}),
            json!({"type":"response.output_text.delta","output_index":0,"item_id":"fixture-message","content_index":0,"delta":response["output"][0]["content"][0]["text"]}),
            json!({"type":"response.completed","response":response}),
        ]
    } else {
        vec![
            json!({"type":"message_start","message":{"content":[]}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":body["content"][0]["text"]}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":body["stop_reason"]}}),
            json!({"type":"message_stop"}),
        ]
    };
    ("text/event-stream", events.into_iter().map(event).collect())
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

/// Whether an accepted connection comes from the client under test. Other
/// local programs, such as an editor that scans new ports, may connect to a
/// fixture as well; only the client's requests start with `POST `.
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
pub fn accept_post(listener: &TcpListener) -> std::io::Result<TcpStream> {
    loop {
        let (socket, _) = listener.accept()?;
        if is_post(&socket) {
            return Ok(socket);
        }
    }
}
