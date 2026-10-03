use super::super::App;
use crate::{input::Form, options::Options};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::TextArea;
use serde_json::Value;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
};

pub(super) fn app(dir: &std::path::Path) -> App {
    App::new(Options {
        store: dir.join("store"),
        recovery_dir: dir.to_path_buf(),
        preset: None,
        config_path: None,
    })
}
pub(super) fn ctrl(letter: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL))
}
pub(super) fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}
pub(super) async fn until(app: &mut App, predicate: impl Fn(&App) -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            app.poll();
            if predicate(app) { return; }
            tokio::select! {
                Some(message) = app.jobs.join_next(), if !app.jobs.is_empty() => app.complete(message.unwrap()),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
    }).await.expect("TUI predicate must settle");
}
pub(super) async fn create(app: &mut App, endpoint: &str) {
    // PATH supplies a nonsecret fixture token without mutating process-global
    // environment (unsafe in edition 2024). The endpoint is local only.
    app.form = Some(Form {
        fields: ["openai-chat", "fixture-model", endpoint, "PATH", "120"]
            .into_iter()
            .map(|v| TextArea::from([v]))
            .collect(),
        selected: 1,
        error: String::new(),
        limits: (256, 10, 64),
    });
    app.event(ctrl('s'));
    until(app, |a| a.entries[a.selected].session.is_some()).await;
}
pub(super) struct Server {
    pub endpoint: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    pub async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let release = Arc::new(Semaphore::new(0));
        let barrier = release.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let captured = captured.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let (header_end, content_len) = loop {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..count]);
                        if let Some(end) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&data[..end]);
                            assert!(headers.starts_with("POST /chat/completions HTTP/1.1"));
                            let len = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap();
                            break (end + 4, len);
                        }
                    };
                    while data.len() < header_end + content_len {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        data.extend_from_slice(&chunk[..count]);
                    }
                    captured.lock().unwrap().push(
                        serde_json::from_slice(&data[header_end..header_end + content_len])
                            .unwrap(),
                    );
                    barrier.acquire().await.unwrap().forget();
                    let body = r#"{"id":"fixture","object":"chat.completion","created":0,"model":"fixture-model","choices":[{"index":0,"message":{"role":"assistant","content":"你好，terminal 👩‍💻"},"finish_reason":"stop"}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            endpoint,
            requests,
            release,
            task,
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
