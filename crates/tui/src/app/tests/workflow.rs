use super::support::{Server, app, create, ctrl, key, until};
use crate::{
    app::{
        Confirmation, Panel,
        jobs::Message,
        state::{Entry, MAX_QUEUED_PROMPTS},
    },
    render,
};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;
use tars_core::{
    application::{ClosePolicy, PreparedCreate, SaveStatus},
    definition::{CredentialRef, Provider, SessionDefinition},
    session::WorkResult,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
};

fn screen(app: &crate::app::App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

async fn wait_idle_saved(app: &mut crate::app::App, index: usize) {
    until(app, |a| {
        a.entries.get(index).is_some_and(|entry| {
            !entry.turn_in_flight
                && entry.session.as_ref().is_some_and(|s| {
                    s.status().active_work.is_none()
                        && matches!(s.status().save, SaveStatus::Saved { .. })
                })
        })
    })
    .await;
}

fn unpaired_definition(endpoint: &str) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture-model".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
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
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
        code_execution: None,
    }
}

async fn spawn_failing_server() -> (String, Arc<Semaphore>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/chat/completions", listener.local_addr().unwrap());
    let release = Arc::new(Semaphore::new(0));
    let barrier = release.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let barrier = barrier.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                barrier.acquire().await.unwrap().forget();
                let body = r#"{"error":{"message":"synthetic failure"}}"#;
                let resp = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });
    (endpoint, release, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf05_running_turn_queues_fifo_renders_escaped_preview_and_auto_dequeues_without_history_dup()
 {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // Submit turn 1.
    app.event(Event::Paste("first turn".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    assert!(app.entries[0].is_turn_running());

    // Queue two prompts while turn 1 is running (first includes untrusted controls/newlines).
    app.event(Event::Paste("second\n\tprompt".into()));
    app.event(key(KeyCode::Enter));
    assert!(!app.entries[0].has_draft());

    app.event(Event::Paste("third prompt".into()));
    app.event(ctrl('s'));
    assert!(!app.entries[0].has_draft());

    assert_eq!(
        app.entries[0].queued_prompts.iter().collect::<Vec<_>>(),
        vec!["second\n\tprompt", "third prompt"]
    );
    assert_eq!(
        app.entries[0].input_history,
        vec![
            "first turn".to_owned(),
            "second\n\tprompt".to_owned(),
            "third prompt".to_owned(),
        ]
    );
    assert_eq!(server.requests.lock().unwrap().len(), 1);

    // Verify footer renders Queued (2) with escaped single-line preview.
    assert_eq!(
        render::format_queue_preview("second\x1b[31m\n\tprompt", 48),
        "second\\x1b[31m  prompt"
    );
    let rendered = screen(&app, 120, 40);
    assert!(
        rendered.contains("Queued (2): second  prompt"),
        "{rendered}"
    );
    assert!(rendered.contains("· /queue"), "{rendered}");

    // Release turn 1 -> turn 2 auto-dequeues; release turn 2 -> turn 3 auto-dequeues.
    server.release.add_permits(1);
    until(&mut app, |_| server.requests.lock().unwrap().len() == 2).await;
    assert_eq!(
        app.entries[0].queued_prompts.iter().collect::<Vec<_>>(),
        vec!["third prompt"]
    );

    server.release.add_permits(1);
    until(&mut app, |_| server.requests.lock().unwrap().len() == 3).await;
    assert!(app.entries[0].queued_prompts.is_empty());

    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    // Input history must NOT duplicate queued prompts on auto-dequeue.
    assert_eq!(
        app.entries[0].input_history,
        vec![
            "first turn".to_owned(),
            "second\n\tprompt".to_owned(),
            "third prompt".to_owned(),
        ]
    );
    {
        let requests = server.requests.lock().unwrap();
        assert!(requests[1].to_string().contains("second"));
        assert!(requests[2].to_string().contains("third prompt"));
    }

    // Counterexample: after turn finishes, `entry.work` stays `Some` for `/repair`,
    // yet `is_turn_running()` must be false so a new submit executes immediately.
    assert!(app.entries[0].work.is_some());
    assert!(!app.entries[0].is_turn_running());
    app.event(Event::Paste("direct fourth".into()));
    app.event(key(KeyCode::Enter));
    assert!(app.entries[0].queued_prompts.is_empty());
    until(&mut app, |_| server.requests.lock().unwrap().len() == 4).await;
    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf05_queue_enforces_max_capacity_and_rejects_when_busy() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    app.event(Event::Paste("turn 1".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

    for idx in 0..MAX_QUEUED_PROMPTS {
        app.event(Event::Paste(format!("queued-{idx}")));
        app.event(key(KeyCode::Enter));
        assert!(!app.entries[0].has_draft());
    }
    assert_eq!(app.entries[0].queued_prompts.len(), MAX_QUEUED_PROMPTS);

    // 9th prompt must be rejected and retain draft.
    app.event(Event::Paste("overflow prompt".into()));
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].queued_prompts.len(), MAX_QUEUED_PROMPTS);
    assert_eq!(app.entries[0].editor.lines(), &["overflow prompt"]);
    assert!(
        app.entries[0].notice.contains("Queue full"),
        "{}",
        app.entries[0].notice
    );

    // Busy operation (e.g. compacting/closing) also rejects enqueue and retains draft.
    app.entries[0].busy = Some("compacting");
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].editor.lines(), &["overflow prompt"]);
    assert!(
        app.entries[0]
            .notice
            .contains("wait for the current session operation"),
        "{}",
        app.entries[0].notice
    );
    app.entries[0].busy = None;

    app.entries[0].queued_prompts.clear();
    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;
    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf05_cancel_failed_save_blocked_and_repair_pause_queue_without_auto_submit() {
    // 1. Cancelled turn pauses queue.
    {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let server = Server::new().await;
        create(&mut app, &server.endpoint).await;

        app.event(Event::Paste("cancel me".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

        app.event(Event::Paste("must pause on cancel".into()));
        app.event(key(KeyCode::Enter));
        assert_eq!(app.entries[0].queued_prompts.len(), 1);

        app.event(ctrl('c'));
        until(&mut app, |a| a.entries[0].execution == "Cancelled").await;
        wait_idle_saved(&mut app, 0).await;

        assert!(app.entries[0].queue_paused);
        assert_eq!(
            app.entries[0].queued_prompts.front().map(String::as_str),
            Some("must pause on cancel")
        );
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        let rendered = screen(&app, 120, 40);
        assert!(rendered.contains("Queued (1, paused):"), "{rendered}");

        let target = app.entries[0].session.as_ref().unwrap().target();
        app.host.close(&target, ClosePolicy::Save).await.unwrap();
    }

    // 2. Failed turn pauses queue.
    {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let (endpoint, release, task) = spawn_failing_server().await;
        create(&mut app, &endpoint).await;

        app.event(Event::Paste("fail me".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |a| a.entries[0].turn_in_flight).await;

        app.event(Event::Paste("must pause on failure".into()));
        app.event(key(KeyCode::Enter));
        release.add_permits(1);
        wait_idle_saved(&mut app, 0).await;

        assert!(app.entries[0].queue_paused);
        assert_eq!(app.entries[0].queued_prompts.len(), 1);
        assert!(
            app.entries[0].notice.contains("paused"),
            "{}",
            app.entries[0].notice
        );

        let target = app.entries[0].session.as_ref().unwrap().target();
        app.host.close(&target, ClosePolicy::Save).await.unwrap();
        task.abort();
    }

    // 3. SaveBlocked pauses queue.
    {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let server = Server::new().await;
        create(&mut app, &server.endpoint).await;

        app.event(Event::Paste("persist fail".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

        app.event(Event::Paste("must pause on save blocked".into()));
        app.event(key(KeyCode::Enter));

        let conn = rusqlite::Connection::open(app.options.store.join("tars.db")).unwrap();
        conn.execute(
            "UPDATE sessions SET policy_json = 'corrupt' WHERE session_id = ?1",
            rusqlite::params![app.entries[0].id()],
        )
        .unwrap();
        drop(conn);

        server.release.add_permits(1);
        until(&mut app, |a| {
            a.entries[0]
                .session
                .as_ref()
                .is_some_and(|s| matches!(s.status().save, SaveStatus::SaveBlocked { .. }))
        })
        .await;
        app.poll();

        assert!(app.entries[0].queue_paused);
        assert_eq!(app.entries[0].queued_prompts.len(), 1);
        assert_eq!(server.requests.lock().unwrap().len(), 1);

        let target = app.entries[0].session.as_ref().unwrap().target();
        app.host
            .close(&target, ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }

    // 4. Needs-repair session pauses queue and refuses auto-submit even on Completed finish.
    {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path());
        let server = Server::new().await;
        let context = Context::from_blocks(vec![
            tars_core::application::text_input("old input"),
            ContextBlock::new(
                causa::runtime::new_block_id(),
                BlockContent::ToolCall(ToolCallPayload::new(
                    "read_file",
                    serde_json::json!({"path": "missing"}),
                )),
                BlockMeta::default().with_provider_call_id("unpaired-call"),
            ),
        ])
        .unwrap();
        let session = app
            .host
            .create(PreparedCreate::persistent(
                unpaired_definition(&server.endpoint),
                context,
            ))
            .await
            .unwrap();
        let mut entry = Entry::new(
            session.target().conversation_id().as_str().into(),
            "fixture-model".into(),
        );
        let work = tars_core::session::WorkRef {
            conversation_id: session.target().conversation_id().clone(),
            turn_id: causa::kernel::TurnId::new("synthetic-work"),
        };
        entry.session = Some(session);
        entry.work = Some(work.clone());
        entry.turn_in_flight = true;
        entry
            .queued_prompts
            .push_back("must pause on repair".into());
        app.entries.push(entry);

        let key = app.entries[0].key.clone();
        app.complete(Message::Finished(
            key,
            work,
            Ok((
                Arc::new(WorkResult::Run(causa::runtime::TurnResult::Completed {
                    final_output: causa::kernel::ModelOutput::new(
                        causa::kernel::ModelResponse {
                            items: vec![causa::kernel::ModelItem::text("ok")],
                        },
                        causa::kernel::ModelStopReason::EndTurn,
                    ),
                })),
                Arc::new(Default::default()),
            )),
        ));
        app.poll();

        assert!(app.entries[0].queue_paused);
        assert_eq!(app.entries[0].queued_prompts.len(), 1);
        assert_eq!(server.requests.lock().unwrap().len(), 0);

        let target = app.entries[0].session.as_ref().unwrap().target();
        app.host.close(&target, ClosePolicy::Save).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf06_queue_commands_session_isolation_and_close_quit_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    create(&mut app, &server.endpoint).await;

    // Session 0 runs a turn and queues two prompts.
    app.selected = 0;
    app.event(Event::Paste("s0-turn1".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

    app.event(Event::Paste("s0-queued-1".into()));
    app.event(key(KeyCode::Enter));
    app.event(Event::Paste("s0-queued-2".into()));
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].queued_prompts.len(), 2);

    // Switch to session 1 via F4: session 1 queue is completely independent (empty).
    app.event(key(KeyCode::F(4)));
    assert_eq!(app.selected, 1);
    assert!(app.entries[1].queued_prompts.is_empty());

    // Switch back to session 0 and test `/queue list`.
    app.event(key(KeyCode::F(4)));
    assert_eq!(app.selected, 0);
    command(&mut app, "queue list");
    match &app.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, "Queued prompts (2)");
            let joined = lines.join("\n");
            assert!(joined.contains("1. s0-queued-1"), "{joined}");
            assert!(joined.contains("2. s0-queued-2"), "{joined}");
        }
        _ => panic!("expected queue list panel"),
    }
    app.event(key(KeyCode::Esc));

    // `/queue pop` refuses to overwrite a non-empty draft.
    app.event(Event::Paste("existing draft".into()));
    command(&mut app, "queue pop");
    assert_eq!(app.entries[0].queued_prompts.len(), 2);
    assert_eq!(app.entries[0].editor.lines(), &["existing draft"]);
    assert!(
        app.entries[0].notice.contains("clear it before /queue pop"),
        "{}",
        app.entries[0].notice
    );

    // Clear draft, `/queue pop` restores front prompt into editor.
    app.entries[0].editor = crate::input::editor();
    command(&mut app, "queue pop");
    assert_eq!(app.entries[0].editor.lines(), &["s0-queued-1"]);
    assert_eq!(
        app.entries[0].queued_prompts.iter().collect::<Vec<_>>(),
        vec!["s0-queued-2"]
    );

    // Clear editor so ONLY queued_prompts remains non-empty, then test Ctrl-W & Ctrl-Q confirmation.
    app.entries[0].editor = crate::input::editor();
    app.event(ctrl('w'));
    assert!(matches!(
        app.confirmation,
        Some(Confirmation::Close {
            has_draft: false,
            has_queue: true
        })
    ));
    let confirm_view = screen(&app, 120, 40);
    assert!(confirm_view.contains("Queued messages"), "{confirm_view}");
    app.event(key(KeyCode::Char('n')));
    assert!(app.confirmation.is_none());
    assert_eq!(app.entries[0].queued_prompts.len(), 1);

    // Switch to session 1 (empty draft & empty queue): Ctrl-Q must STILL confirm because session 0 has queued prompts!
    app.selected = 1;
    app.event(ctrl('q'));
    assert!(matches!(
        app.confirmation,
        Some(Confirmation::Quit {
            has_drafts: false,
            has_queue: true
        })
    ));
    app.event(key(KeyCode::Esc));
    assert!(app.confirmation.is_none());

    // `/queue clear` clears remaining prompt on session 0.
    app.selected = 0;
    command(&mut app, "queue clear");
    assert!(app.entries[0].queued_prompts.is_empty());
    assert!(!app.entries[0].queue_paused);

    // Now Ctrl-C turn 1 and Ctrl-Q exits cleanly without confirmation.
    app.event(ctrl('c'));
    until(&mut app, |a| a.entries[0].execution == "Cancelled").await;
    app.event(ctrl('q'));
    assert!(app.confirmation.is_none());
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wf07_rename_fork_and_sessions_delete_with_guardrails() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    let original_id = app.entries[0].id().to_owned();

    // Bare `/rename` shows usage.
    command(&mut app, "rename");
    assert!(app.entries[0].notice.contains("Usage: /rename"));

    // Invalid title (control char) is rejected.
    command(&mut app, "rename bad\ttitle");
    assert!(app.entries[0].notice.contains("Invalid title"));
    assert_eq!(app.entries[0].title(), None);

    // `/rename Alpha Branch` updates title live and in header.
    command(&mut app, "rename Alpha Branch");
    until(&mut app, |a| {
        a.entries[0].title().as_deref() == Some("Alpha Branch")
            && a.entries[0].notice.contains("Renamed session")
    })
    .await;
    let rendered = screen(&app, 120, 40);
    assert!(rendered.contains("Alpha Branch"), "{rendered}");

    // `/rename --clear` clears the title.
    command(&mut app, "rename --clear");
    until(&mut app, |a| {
        a.entries[0].title().is_none() && a.entries[0].notice.contains("Cleared session title")
    })
    .await;

    // Set a fresh title before forking so derived title / explicit fork title works.
    command(&mut app, "rename Base Session");
    until(&mut app, |a| {
        a.entries[0].title().as_deref() == Some("Base Session")
            && a.entries[0].notice.contains("Renamed session")
    })
    .await;
    wait_idle_saved(&mut app, 0).await;

    // Run one turn on original so original has distinct conversation materials.
    app.event(Event::Paste("original history prompt".into()));
    app.event(key(KeyCode::Enter));
    assert!(
        !app.entries[0].has_draft(),
        "submit did not accept draft: notice={:?}, exec={:?}, status={:?}, busy={:?}, confirmation={:?}, panel={:?}",
        app.entries[0].notice,
        app.entries[0].execution,
        app.entries[0].status,
        app.entries[0].busy,
        app.confirmation.is_some(),
        app.panel.is_some(),
    );
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

    // Forking while turn is in flight must be rejected.
    command(&mut app, "fork Busy Fork");
    assert!(
        app.entries[0].notice.contains("Wait for the current turn"),
        "{}",
        app.entries[0].notice
    );
    assert_eq!(app.entries.len(), 1);

    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    // Forking while viewing read-only child agent must be rejected.
    app.entries[0].viewed_agent_id = Some("child-id".into());
    command(&mut app, "fork Child Fork");
    assert!(app.entries[0].notice.contains("read-only"));
    app.entries[0].viewed_agent_id = None;

    // Fork cleanly with explicit title -> creates new session, opens and switches to it.
    command(&mut app, "fork Forked Exploration");
    until(&mut app, |a| {
        a.entries.len() == 2 && a.selected == 1 && a.entries[1].session.is_some()
    })
    .await;
    let forked_id = app.entries[1].id().to_owned();
    assert_ne!(forked_id, original_id);
    assert_eq!(
        app.entries[1].title().as_deref(),
        Some("Forked Exploration")
    );
    // Original session remains intact and open!
    assert_eq!(app.entries[0].id(), original_id);
    assert!(app.entries[0].session.is_some());
    assert_eq!(app.entries[0].title().as_deref(), Some("Base Session"));

    // Open `/sessions` panel:
    command(&mut app, "sessions");
    let sessions_view = screen(&app, 120, 40);
    assert!(
        sessions_view.contains("Ctrl-D/Del delete"),
        "{sessions_view}"
    );

    // Plain 'd' must filter query, NOT trigger delete!
    app.event(key(KeyCode::Char('d')));
    match &app.panel {
        Some(Panel::Sessions { query, .. }) => assert_eq!(query, "d"),
        _ => panic!("expected sessions panel with 'd' filter"),
    }
    assert!(app.confirmation.is_none());
    app.event(key(KeyCode::Backspace));

    // Trying Ctrl-D on an OPEN session must refuse with clear notice.
    app.event(ctrl('d'));
    assert!(app.confirmation.is_none());
    assert!(app.notice.contains("Close this session before deleting"));
    app.event(key(KeyCode::Esc));

    // Close the forked session (selected == 1), then delete it via `/sessions` + `Delete` -> `y`.
    app.selected = 1;
    app.event(ctrl('w'));
    until(&mut app, |a| {
        a.entries[1].session.is_none() && a.entries[1].busy.is_none()
    })
    .await;

    command(&mut app, "sessions");
    // Filter to the forked session so it is highlighted at index 0.
    app.event(Event::Paste("Forked".into()));
    app.event(key(KeyCode::Delete));
    assert!(matches!(
        &app.confirmation,
        Some(Confirmation::DeleteSession { id, .. }) if id == &forked_id
    ));
    let delete_confirm_view = screen(&app, 120, 40);
    assert!(
        delete_confirm_view.contains("Delete session?"),
        "{delete_confirm_view}"
    );

    // Cancelling with 'n' preserves the entry and stored record.
    app.event(key(KeyCode::Char('n')));
    assert!(app.confirmation.is_none());
    assert!(app.entries.iter().any(|e| e.id() == forked_id));

    // Reopen `/sessions`, trigger `Ctrl-D`, confirm `'y'` -> removes entry and deletes from store.
    command(&mut app, "sessions");
    app.event(Event::Paste("Forked".into()));
    app.event(ctrl('d'));
    assert!(matches!(
        &app.confirmation,
        Some(Confirmation::DeleteSession { id, .. }) if id == &forked_id
    ));
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.entries.len() == 1).await;
    assert_eq!(app.entries[0].id(), original_id);

    let listed = app.host.list().await.unwrap();
    assert!(
        listed
            .iter()
            .all(|item| item.conversation_id.as_str() != forked_id)
    );
    let orig_listing = listed
        .iter()
        .find(|item| item.conversation_id.as_str() == original_id)
        .unwrap();
    assert_eq!(
        orig_listing.record.as_ref().unwrap().title.as_deref(),
        Some("Base Session")
    );

    // Tab completion covers `/rename `, `/fork `, `/queue `, and `/edit`.
    app.selected = 0;
    for (typed, expected) in [
        ("/ren", "/rename "),
        ("/fo", "/fork "),
        ("/que", "/queue "),
        ("/ed", "/edit"),
    ] {
        app.entries[0].editor = crate::input::editor();
        app.event(Event::Paste(typed.into()));
        app.event(key(KeyCode::Tab));
        assert_eq!(app.entries[0].editor.lines(), &[expected]);
    }
    app.entries[0].editor = crate::input::editor();

    // Fork without explicit title derives `<source> (fork)` and stays independent after source deletion.
    command(&mut app, "fork");
    until(&mut app, |a| {
        a.entries.len() == 2 && a.selected == 1 && a.entries[1].session.is_some()
    })
    .await;
    assert_eq!(
        app.entries[1].title().as_deref(),
        Some("Base Session (fork)")
    );
    let derived_fork_id = app.entries[1].id().to_owned();

    // Close and delete original session; derived fork remains open and usable.
    app.selected = 0;
    app.event(ctrl('w'));
    until(&mut app, |a| {
        a.entries[0].session.is_none() && a.entries[0].busy.is_none()
    })
    .await;
    command(&mut app, "sessions");
    app.event(Event::Paste(original_id.clone()));
    app.event(ctrl('d'));
    assert!(matches!(
        &app.confirmation,
        Some(Confirmation::DeleteSession { id, .. }) if id == &original_id
    ));
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.entries.len() == 1).await;
    assert_eq!(app.entries[0].id(), derived_fork_id);

    // Clean shutdown.
    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test]
async fn wf08_external_editor_guards_history_reset_and_failure_preservation() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // Seed input history & enter history browsing mode.
    app.entries[0].push_input_history("earlier prompt");
    app.event(Event::Paste("unsent draft".into()));
    app.event(key(KeyCode::Home));
    app.event(key(KeyCode::Up));
    assert!(app.entries[0].history_cursor.is_some());

    // Guardrail 1: read-only child agent view blocks Ctrl-G and /edit.
    app.entries[0].viewed_agent_id = Some("child-1".into());
    app.event(ctrl('g'));
    assert!(!app.run_pending_external_edit_with(|_| Ok("hacked".into())));
    assert!(app.entries[0].notice.contains("read-only"));
    app.entries[0].viewed_agent_id = None;

    // Guardrail 2: modal panel blocks Ctrl-G.
    command(&mut app, "sessions");
    app.event(ctrl('g'));
    assert!(!app.run_pending_external_edit_with(|_| Ok("hacked".into())));
    app.event(key(KeyCode::Esc));

    // Guardrail 3: confirmation dialog blocks Ctrl-G.
    app.confirmation = Some(Confirmation::Discard);
    app.event(ctrl('g'));
    assert!(!app.run_pending_external_edit_with(|_| Ok("hacked".into())));
    app.event(key(KeyCode::Esc));

    // Guardrail 4: pending approval blocks Ctrl-G.
    app.entries[0]
        .approval_queue
        .push_back(crate::app::state::PendingApproval::mock(
            "write_file".into(),
            tars_core::permission::PermissionMode::Ask,
            tars_core::permission::ApprovalSubject::FileWrite {
                path: "a.txt".into(),
                action: tars_core::permission::WriteAction::Created,
                diff: "+hi".into(),
            },
            |_| {},
        ));
    app.event(ctrl('g'));
    assert!(!app.run_pending_external_edit_with(|_| Ok("hacked".into())));
    app.entries[0].approval_queue.clear();

    // Success via `/edit`: replaces draft and resets history cursor.
    command(&mut app, "edit");
    let ran = app.run_pending_external_edit_with(|initial| {
        assert_eq!(initial, "earlier prompt");
        Ok("edited\nmultiline prompt".into())
    });
    assert!(ran);
    assert_eq!(
        app.entries[0].editor.lines(),
        &["edited", "multiline prompt"]
    );
    assert!(app.entries[0].history_cursor.is_none());

    // Failure via `Ctrl-G`: preserves original draft.
    app.event(ctrl('g'));
    let ran = app.run_pending_external_edit_with(|initial| {
        assert_eq!(initial, "edited\nmultiline prompt");
        Err("Editor exited with status 1; draft preserved".into())
    });
    assert!(ran);
    assert_eq!(
        app.entries[0].editor.lines(),
        &["edited", "multiline prompt"]
    );
    assert!(app.entries[0].notice.contains("status 1"));

    // Unopened session cannot launch external edit.
    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[0].session = None;
    app.event(ctrl('g'));
    assert!(!app.run_pending_external_edit_with(|_| Ok("ignored".into())));
}

#[cfg(unix)]
#[tokio::test]
async fn wf08_ctrl_g_with_real_subprocess_editor_script() {
    use crate::input::editor::{EditorCommand, edit_prompt_with_command};
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    let script_path = dir.path().join("fake_editor.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\nprintf 'hello from subprocess editor\\nline 2\\n' > \"$1\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script_path, perms).unwrap();

    let cmd = EditorCommand {
        program: script_path.to_string_lossy().into_owned(),
        args: Vec::new(),
    };

    app.event(Event::Paste("initial draft".into()));
    app.event(ctrl('g'));
    assert!(app.run_pending_external_edit_with(|initial| {
        assert_eq!(initial, "initial draft");
        edit_prompt_with_command(&cmd, initial)
    }));
    assert_eq!(
        app.entries[0].editor.lines(),
        &["hello from subprocess editor", "line 2", ""]
    );

    // Non-zero exit script leaves draft untouched.
    let fail_script = dir.path().join("fail_editor.sh");
    std::fs::write(&fail_script, "#!/bin/sh\nexit 2\n").unwrap();
    let mut perms = std::fs::metadata(&fail_script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fail_script, perms).unwrap();
    let fail_cmd = EditorCommand {
        program: fail_script.to_string_lossy().into_owned(),
        args: Vec::new(),
    };

    app.event(ctrl('g'));
    assert!(app.run_pending_external_edit_with(|initial| {
        edit_prompt_with_command(&fail_cmd, initial)
    }));
    assert_eq!(
        app.entries[0].editor.lines(),
        &["hello from subprocess editor", "line 2", ""]
    );
    assert!(app.entries[0].notice.contains("status 2"));

    // Verify Ctrl-G with KeyModifiers::CONTROL works even with uppercase or standard 'g'.
    let _ = KeyModifiers::CONTROL;

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}
