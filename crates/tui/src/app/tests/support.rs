use super::super::App;
use crate::{app::Clock, input::Form, options::Options};
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
    let mut app = App::new(Options {
        store: dir.join("store"),
        recovery_dir: dir.to_path_buf(),
        config: Arc::new(tars_core::config::Config::default()),
        profile: None,
        preset: None,
        config_path: None,
    });
    app.clock = Clock::manual(std::time::Instant::now());
    app
}
pub(super) fn advance_past_approval_window(app: &mut App) {
    app.update_approval_guard();
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
}
pub(super) fn advance_past_repair_window(app: &mut App) {
    app.update_repair_guard();
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
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
            if predicate(app) {
                return;
            }
            tokio::select! {
                Some(message) = app.jobs.join_next(), if !app.jobs.is_empty() => {
                    // A job aborted by a test (quitting the splash) is not a
                    // message and must not fail the harness.
                    if let Ok(message) = message {
                        app.complete(message);
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(5)) => {},
            }
        }
    })
    .await
    .expect("TUI predicate must settle");
}
pub(super) async fn create(app: &mut App, endpoint: &str) {
    create_provider(app, "openai-chat", endpoint).await;
}
pub(super) async fn create_provider(app: &mut App, provider: &str, endpoint: &str) {
    // PATH supplies a nonsecret fixture token without mutating process-global
    // environment (unsafe in edition 2024). The endpoint is local only.
    app.form = Some(Form {
        fields: [
            provider,
            "fixture-model",
            endpoint,
            "PATH",
            "120",
            "",
            "",
            "ask",
            "",
            "on",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        selected: 1,
        error: String::new(),
        limits: (256, 10, 64),
        generation: (
            (provider == "anthropic").then_some(1024),
            None,
            Default::default(),
        ),
        ..Form::default()
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
        Self::with_options(None).await
    }

    pub async fn with_streaming_rounds(rounds: Vec<Vec<String>>) -> Self {
        Self::with_options(Some(rounds)).await
    }

    async fn with_options(streaming_rounds: Option<Vec<Vec<String>>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/chat/completions", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let release = Arc::new(Semaphore::new(0));
        let barrier = release.clone();
        let streaming_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rounds_arc = streaming_rounds.map(Arc::new);
        let task = tokio::spawn(async move {
            // Owned children are aborted when the listener task is aborted.
            let mut children = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = tokio::select! {
                    socket = listener.accept() => socket.unwrap(),
                    completed = children.join_next(), if !children.is_empty() => {
                        completed.unwrap().unwrap();
                        continue;
                    },
                };
                if !is_post(&socket).await {
                    continue;
                }
                let captured = captured.clone();
                let barrier = barrier.clone();
                let streaming_counter = streaming_counter.clone();
                let rounds_arc = rounds_arc.clone();
                children.spawn(async move {
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
                    let request: Value = serde_json::from_slice(
                        &data[header_end..header_end + content_len],
                    )
                    .unwrap();
                    // Work runs stream their model text. A compaction summary
                    // call uses the non-streaming gateway, so it receives one
                    // complete JSON answer instead.
                    let streaming = request
                        .get("stream")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    captured.lock().unwrap().push(request);
                    if !streaming {
                        let body = serde_json::json!({
                            "id": "fixture",
                            "object": "chat.completion",
                            "choices": [{
                                "index": 0,
                                "message": {
                                    "role": "assistant",
                                    "content": "fixture summary: continue the task",
                                },
                                "finish_reason": "stop",
                            }],
                            "usage": {
                                "prompt_tokens": 1,
                                "completion_tokens": 1,
                                "total_tokens": 2,
                            },
                        })
                        .to_string();
                        let _ = socket
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                    body.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        return;
                    }
                    if let Some(rounds) = &rounds_arc {
                        let round = streaming_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let chunks = rounds.get(round).or_else(|| rounds.last()).cloned().unwrap_or_default();
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await;
                        for chunk in chunks {
                            let _ = socket.write_all(format!("{:x}\r\n{}\r\n", chunk.len(), chunk).as_bytes()).await;
                        }
                        let _ = socket.write_all(b"0\r\n\r\n").await;
                    } else {
                        let prefix = r#"data: {"id":"fixture","choices":[{"index":0,"delta":{"content":"你好，terminal 👩‍💻"},"finish_reason":null}]}"#.to_owned() + "\n\n";
                        let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await;
                        let _ = socket.write_all(format!("{:x}\r\n{prefix}\r\n", prefix.len()).as_bytes()).await;
                        barrier.acquire().await.unwrap().forget();
                        let suffix = "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                        let _ = socket.write_all(format!("{:x}\r\n{suffix}\r\n0\r\n\r\n", suffix.len()).as_bytes()).await;
                    }
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

/// Whether an accepted connection comes from the client under test. Other
/// local programs, such as an editor that scans new ports, may connect to a
/// fixture as well; only the client's requests start with `POST `.
pub async fn is_post(socket: &tokio::net::TcpStream) -> bool {
    const PREFIX: &[u8] = b"POST ";
    let mut head = [0; PREFIX.len()];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.peek(&mut head)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return false,
            Ok(Ok(n)) if head[..n] != PREFIX[..n] => return false,
            Ok(Ok(n)) if n == PREFIX.len() => return true,
            Ok(Ok(_)) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
}

/// A definition with no tools, no workspace and no model calls: fixtures only
/// need the session record and the compaction runtime.
pub(super) fn history_definition(
    context_window: Option<u64>,
) -> tars_core::definition::SessionDefinition {
    tars_core::definition::SessionDefinition {
        provider: tars_core::definition::Provider::OpenAiChat,
        model: "fixture-model".into(),
        endpoint: None,
        credential: tars_core::definition::CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(120),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
    }
}

/// One text Context block with a deterministic estimate.
pub(super) fn text_context(text: &str) -> causa::kernel::Context {
    causa::kernel::Context::from_blocks(vec![tars_core::application::text_input(text)]).unwrap()
}

/// A saved session after one whole-Context compaction per span in `spans`.
///
/// Record `k` removed the previous brief (for `k > 0`) and span `k`, and
/// inserted brief `k`; the current Context is the newest brief followed by
/// `current`. Every record is written through its JSON form.
pub(super) fn write_history_session(
    store: &std::path::Path,
    current: &causa::kernel::Context,
    spans: &[causa::kernel::Context],
) -> String {
    let conversation_id = causa::runtime::new_block_id().0.to_string();
    let sessions = store.join("sessions");
    let history = sessions.join(format!("{conversation_id}.history"));
    std::fs::create_dir_all(&history).unwrap();
    let brief = |position: usize| {
        causa::kernel::ContextBlock::new(
            causa::runtime::new_block_id(),
            causa::kernel::BlockContent::text(
                causa::kernel::Role::User,
                format!("brief {position}"),
            ),
            causa::kernel::BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
        )
    };
    let mut entries = Vec::new();
    let mut previous: Option<causa::kernel::ContextBlock> = None;
    for (position, span) in spans.iter().enumerate() {
        let record_id = causa::runtime::new_block_id().0.to_string();
        let inserted = brief(position);
        let mut originals = previous.iter().cloned().collect::<Vec<_>>();
        originals.extend(span.blocks().iter().cloned());
        let estimate_before = 100_000 + 10_000 * u64::try_from(position).unwrap();
        let estimate_after = 5_000 + 1_000 * u64::try_from(position).unwrap();
        entries.push(serde_json::json!({
            "record_id": record_id,
            "created_at_ms": 1u64,
            "trigger": "auto",
            "kind": "summarize",
            "removed_block_count": originals.len(),
            "inserted_block_ids": [inserted.id()],
        }));
        let document = serde_json::json!({
            "format": "tars-history-v1.1",
            "conversation_id": conversation_id,
            "record_id": record_id,
            "created_at_ms": 1u64,
            "trigger": "auto",
            "kind": "summarize",
            "estimate_before": estimate_before,
            "estimate_after": estimate_after,
            "replacements": [{
                "range": {"start": 0, "end": originals.len()},
                "originals": originals,
                "inserted_ids": [inserted.id()],
            }],
        });
        std::fs::write(
            history.join(format!("{record_id}.json")),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        previous = Some(inserted);
    }
    let mut blocks = previous.into_iter().collect::<Vec<_>>();
    blocks.extend(current.blocks().iter().cloned());
    let context = causa::kernel::Context::from_blocks(blocks).unwrap();
    let record = serde_json::json!({
        "format": "tars-session-v2.2",
        "conversation_id": conversation_id,
        "revision": 1u64,
        "last_commit_id": causa::runtime::new_block_id().0.to_string(),
        "definition": serde_json::to_value(history_definition(None)).unwrap(),
        "context": serde_json::to_value(&context).unwrap(),
        "history": entries,
        "updated_at_ms": 1000u64,
    });
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{conversation_id}.json")),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    conversation_id
}

/// Delete one record file by index, leaving the session record still listing it.
pub(super) fn delete_history_record(store: &std::path::Path, conversation_id: &str, index: usize) {
    let sessions = store.join("sessions");
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sessions.join(format!("{conversation_id}.json"))).unwrap(),
    )
    .unwrap();
    let record_id = record["history"][index]["record_id"].as_str().unwrap();
    std::fs::remove_file(
        sessions
            .join(format!("{conversation_id}.history"))
            .join(format!("{record_id}.json")),
    )
    .unwrap();
}

/// Open a stored session into a fresh view at the standard test size.
pub(super) async fn open_in_view(app: &mut super::super::App, id: &str) {
    let session = app.host.open(id.into()).await.unwrap();
    let mut entry = crate::app::Entry::new(id.to_owned(), "fixture-model".into());
    entry.session = Some(session);
    app.entries.push(entry);
    app.selected = 0;
    set_view_size(app);
    until(app, |app| {
        app.entries[0].rows.is_some() && app.entries[0].material_version.is_some()
    })
    .await;
}

/// Create a session from one definition and show it in a fresh view.
pub(super) async fn create_in_view(
    app: &mut super::super::App,
    definition: tars_core::definition::SessionDefinition,
    context: causa::kernel::Context,
) {
    let session = app
        .host
        .create(tars_core::application::PreparedCreate::persistent(
            definition, context,
        ))
        .await
        .unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    let mut entry = crate::app::Entry::new(id, "fixture-model".into());
    entry.session = Some(session);
    app.entries.push(entry);
    app.selected = 0;
    set_view_size(app);
    until(app, |app| {
        app.entries[0].rows.is_some() && app.entries[0].material_version.is_some()
    })
    .await;
}

fn set_view_size(app: &mut super::super::App) {
    app.view_width = crate::render::transcript_width(100);
    app.view_height = 50;
    app.viewport_height = crate::render::conversation_height(50, 100, app);
}
