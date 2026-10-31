//! Real SessionHost/gateway/TUI integration with causally gated local SSE.
use super::support::*;
use crate::{app::jobs::Message, render};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tars_core::application::{ClosePolicy, SaveStatus};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn event(kind: &str, value: Value) -> String {
    format!("event: {kind}\ndata: {value}\n\n")
}
fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn sequence(provider: &str, preview: &str, final_text: &str) -> (String, String) {
    match provider {
        "openai-chat" => (
            data(
                json!({"id":"chat1","choices":[{"index":0,"delta":{"content":preview},"finish_reason":null}]}),
            ),
            data(json!({"id":"chat1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
                + "data: [DONE]\n\n",
        ),
        "openai-responses" => (
            event(
                "response.created",
                json!({"type":"response.created","response":{"id":"r1","status":"in_progress","output":[]}}),
            ) + &event(
                "response.output_item.added",
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1","content":[]}}),
            ) + &event(
                "response.content_part.added",
                json!({"type":"response.content_part.added","output_index":0,"item_id":"m1","content_index":0,"part":{"type":"output_text","text":""}}),
            ) + &event(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","output_index":0,"item_id":"m1","content_index":0,"delta":preview}),
            ),
            event(
                "response.completed",
                json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[{"type":"message","id":"m1","content":[{"type":"output_text","text":final_text}]}]}}),
            ),
        ),
        "anthropic" => (
            event(
                "message_start",
                json!({"type":"message_start","message":{"id":"m1","content":[],"stop_reason":null}}),
            ) + &event(
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            ) + &event(
                "content_block_delta",
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":preview}}),
            ),
            event(
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ) + &event(
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            ) + &event("message_stop", json!({"type":"message_stop"})),
        ),
        _ => unreachable!(),
    }
}

struct Fixture {
    endpoint: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    terminal: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(provider: &str, prefix: String, suffix: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let path = match provider {
            "openai-chat" => "/v1/chat/completions",
            "openai-responses" => "/v1/responses",
            "anthropic" => "/v1/messages",
            _ => unreachable!(),
        };
        let endpoint = format!("http://{}{path}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let (release, gate) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut socket = loop {
                let (socket, _) = listener.accept().await.unwrap();
                if is_post(&socket).await {
                    break socket;
                }
            };
            let request = read_request(&mut socket).await;
            captured.lock().unwrap().push(request);
            if socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await.is_err() { return; }
            if !chunk(&mut socket, &prefix).await {
                return;
            }
            // Terminal delivery requires an explicit signal after UI observation.
            if gate.await.is_err() {
                return;
            }
            if !chunk(&mut socket, &suffix).await {
                return;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        Self {
            endpoint,
            requests,
            terminal: Some(release),
            task,
        }
    }

    fn release(&mut self) {
        let _ = self.terminal.take().unwrap().send(());
    }

    fn assert_request(&self, provider: &str) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "view events cannot replay a model call");
        let expected = match provider {
            "openai-chat" => "/v1/chat/completions",
            "openai-responses" => "/v1/responses",
            _ => "/v1/messages",
        };
        assert_eq!(requests[0].0, expected);
        assert_eq!(requests[0].1["stream"], true);
        assert_eq!(requests[0].1["model"], "fixture-model");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let (start, length, path) = loop {
        let mut chunk = [0u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end]).unwrap();
            let path = headers
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (end + 4, length, path);
        }
    };
    while bytes.len() < start + length {
        let mut chunk = [0u8; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        path,
        serde_json::from_slice(&bytes[start..start + length]).unwrap(),
    )
}

async fn chunk(socket: &mut TcpStream, text: &str) -> bool {
    text.is_empty()
        || socket
            .write_all(format!("{:x}\r\n{text}\r\n", text.len()).as_bytes())
            .await
            .is_ok()
}

fn record(app: &crate::app::App, index: usize) -> std::path::PathBuf {
    app.options
        .store
        .join("sessions")
        .join(format!("{}.json", app.entries[index].id()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_three_real_http_gateways_show_text_before_terminal_and_save_authoritative_material() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let preview = "provisional 你好🙂";
        let final_text = if provider == "openai-responses" {
            "authoritative 完成🙂"
        } else {
            preview
        };
        let (prefix, suffix) = sequence(provider, preview, final_text);
        let mut server = Fixture::new(provider, prefix, suffix).await;
        create_provider(&mut app, provider, &server.endpoint).await;
        assert!(server.requests.lock().unwrap().is_empty());
        app.event(Event::Paste("input only".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |a| a.entries[0].preview.text.contains(preview)).await;
        assert_eq!(app.entries[0].execution, "Running");
        assert!(app.entries[0].preview.invocation.is_some());
        assert!(!app.entries[0].transcript.text.contains(preview));
        assert!(
            !std::fs::read_to_string(record(&app, 0))
                .unwrap()
                .contains(preview)
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            screen.contains("provisional"),
            "temporary text must be visible before terminal: {screen}"
        );
        server.assert_request(provider);
        server.release();
        until(&mut app, |a| {
            a.entries[0].execution == "Completed"
                && a.entries[0].transcript.text.contains(final_text)
                && matches!(
                    a.entries[0].session.as_ref().unwrap().status().save,
                    SaveStatus::Saved { revision: 2, .. }
                )
        })
        .await;
        assert!(app.entries[0].preview.text.is_empty());
        assert!(app.entries[0].preview.invocation.is_none());
        assert!(app.entries[0].progress.is_none());
        let saved: Value =
            serde_json::from_slice(&std::fs::read(record(&app, 0)).unwrap()).unwrap();
        let material = saved["context"].to_string();
        assert!(material.contains(final_text));
        if provider == "openai-responses" {
            assert!(!material.contains(preview));
        }
        server.assert_request(provider);
        app.event(ctrl('q'));
        until(&mut app, |a| a.done).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn visible_partial_text_is_cleared_after_error_eof_or_cancellation_and_never_saved() {
    for mode in ["error", "eof", "cancel"] {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let (prefix, _) = sequence("openai-chat", "must not persist", "");
        let suffix = if mode == "error" {
            data(json!({"error":{"type":"server_error","message":"fixture failure"}}))
        } else {
            String::new()
        };
        let mut server = Fixture::new("openai-chat", prefix, suffix).await;
        create(&mut app, &server.endpoint).await;
        app.event(Event::Paste("accepted input".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |a| {
            a.entries[0].preview.text.contains("must not persist")
        })
        .await;
        if mode == "cancel" {
            app.event(ctrl('c'));
        } else {
            server.release();
        }
        until(&mut app, |a| {
            a.entries[0].execution != "Running"
                && matches!(
                    a.entries[0].session.as_ref().unwrap().status().save,
                    SaveStatus::Saved { revision: 2, .. }
                )
        })
        .await;
        if mode == "cancel" {
            assert_eq!(app.entries[0].execution, "Cancelled");
        } else {
            let (label, notice) = if mode == "error" {
                (
                    "Temporary provider failure",
                    "provider error (server_error)",
                )
            } else {
                (
                    "Response stream ended early",
                    "stream ended without a provider terminal",
                )
            };
            assert!(app.entries[0].execution.starts_with(label));
            assert!(
                app.entries[0].notice.starts_with("Model error: ")
                    && app.entries[0].notice.contains(notice),
                "{}",
                app.entries[0].notice
            );
        }
        assert!(app.entries[0].preview.text.is_empty());
        assert!(app.entries[0].progress.is_none());
        let snapshot = app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .read_materials()
            .unwrap();
        assert_eq!(snapshot.context.blocks().len(), 1);
        let saved = std::fs::read_to_string(record(&app, 0)).unwrap();
        assert!(saved.contains("accepted input"));
        assert!(!saved.contains("must not persist"));
        server.assert_request("openai-chat");
        app.event(ctrl('q'));
        until(&mut app, |a| a.done).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_switch_and_old_instance_completion_cannot_move_preview_or_results() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (prefix, suffix) = sequence("openai-responses", "first preview", "first final");
    let mut server = Fixture::new("openai-responses", prefix, suffix).await;
    create_provider(&mut app, "openai-responses", &server.endpoint).await;
    app.event(Event::Paste("first input".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[0].preview.text == "first preview").await;
    let work = app.entries[0].work.clone().unwrap();
    create_provider(&mut app, "openai-responses", &server.endpoint).await;
    assert_eq!(app.selected, 1);
    assert!(app.entries[1].preview.text.is_empty());
    app.event(Event::Paste("second draft".into()));
    server.release();
    until(&mut app, |a| {
        a.entries[0].execution == "Completed"
            && a.entries[0].transcript.text.contains("first final")
    })
    .await;
    assert_eq!(app.selected, 1);
    assert!(app.entries[1].preview.text.is_empty());
    assert!(!app.entries[1].transcript.text.contains("first"));
    assert_eq!(app.entries[1].editor.lines(), &["second draft"]);
    app.selected = 0;
    app.event(ctrl('w'));
    until(&mut app, |a| a.entries[0].session.is_none()).await;
    let stale_key = app.entries[0].key.clone();
    app.event(key(KeyCode::F(3)));
    until(&mut app, |a| a.entries[0].session.is_some()).await;
    app.complete(Message::Finished(
        stale_key,
        work,
        Err("late old completion".into()),
    ));
    assert_eq!(app.entries[0].execution, "No work in this instance");
    assert!(app.entries[0].preview.text.is_empty());
    server.assert_request("openai-responses");
    app.host
        .close(
            &app.entries[0].session.as_ref().unwrap().target(),
            ClosePolicy::Save,
        )
        .await
        .unwrap();
    app.host
        .close(
            &app.entries[1].session.as_ref().unwrap().target(),
            ClosePolicy::Save,
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_snapshot_survives_save_blocked_and_export_without_retaining_preview_or_replaying_http()
 {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (prefix, suffix) = sequence(
        "openai-responses",
        "temporary never export",
        "final export material",
    );
    let mut server = Fixture::new("openai-responses", prefix, suffix).await;
    create_provider(&mut app, "openai-responses", &server.endpoint).await;
    app.event(Event::Paste("input survives".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.entries[0].preview.text.contains("temporary never export")
    })
    .await;
    let path = record(&app, 0);
    let held = app.options.store.join("held.json");
    std::fs::rename(&path, &held).unwrap();
    std::fs::create_dir(&path).unwrap();
    server.release();
    until(&mut app, |a| {
        a.entries[0].execution == "Completed"
            && matches!(
                a.entries[0].session.as_ref().unwrap().status().save,
                SaveStatus::SaveBlocked { .. }
            )
    })
    .await;
    assert!(app.entries[0].preview.text.is_empty());
    app.event(ctrl('e'));
    until(&mut app, |a| {
        a.entries[0].notice.contains("Exported actual materials")
    })
    .await;
    let exports: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("tars-recovery-")
        })
        .collect();
    assert_eq!(exports.len(), 1);
    let exported = tars_core::material::load(exports[0].path()).unwrap();
    let material = serde_json::to_string(&exported).unwrap();
    assert!(material.contains("input survives"));
    assert!(material.contains("final export material"));
    assert!(!material.contains("temporary never export"));
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(&held, &path).unwrap();
    app.event(ctrl('r'));
    until(&mut app, |a| {
        matches!(
            a.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { revision: 2, .. }
        ) && a.entries[0].busy.is_none()
    })
    .await;
    server.assert_request("openai-responses");
    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}
