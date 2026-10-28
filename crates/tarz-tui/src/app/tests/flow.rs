use super::support::*;
use crate::{app::jobs::Message, render};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use tarz_core::application::{ClosePolicy, SaveStatus};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_provider_switch_redraw_repeat_and_reopen_preserve_one_submission() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    assert!(server.requests.lock().unwrap().is_empty());
    let first_id = app.entries[0].id().to_owned();
    app.event(Event::Paste("中文\n👩‍💻 e\u{301}".into()));
    app.event(ctrl('s'));
    app.event(ctrl('s'));
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('s'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    )));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    let work = app.entries[0].work.clone().unwrap();
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("draft in second view".into()));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    for _ in 0..20 {
        app.event(Event::Resize(120, 40));
        app.event(key(KeyCode::F(4)));
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
    }
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(app.entries[0].execution, "Running");
    server.release.add_permits(1);
    until(&mut app, |a| {
        a.entries[0].execution == "Completed" && a.entries[0].transcript.text.contains("你好")
    })
    .await;
    {
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests[0]["model"], "fixture-model");
        assert!(requests[0].to_string().contains("中文"));
    }
    app.selected = 0;
    app.event(ctrl('w'));
    until(&mut app, |a| {
        a.entries[0].session.is_none() && a.entries[0].busy.is_none()
    })
    .await;
    let old_key = app.entries[0].key.clone();
    app.event(key(KeyCode::F(3)));
    until(&mut app, |a| a.entries[0].session.is_some()).await;
    assert_eq!(app.entries[0].id(), first_id);
    assert!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .observe(&work)
            .is_err()
    );
    app.complete(Message::Finished(
        old_key,
        work,
        Err("late old event".into()),
    ));
    assert_eq!(app.entries[0].execution, "No work in this instance");
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    assert_eq!(
        app.entries[1].editor.lines(),
        &["draft in second view".to_string()]
    );
    app.event(ctrl('q'));
    assert!(app.confirmation.is_some());
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.done).await;
    assert!(app.host.lookup(&first_id.into()).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_and_quit_wait_for_real_material_save() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("cancel me".into()));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    let session = app.entries[0].session.clone().unwrap();
    app.event(ctrl('c'));
    until(&mut app, |a| a.entries[0].execution == "Cancelled").await;
    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
    assert!(matches!(
        session.status().save,
        SaveStatus::Saved { revision: 2, .. }
    ));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_failure_keeps_result_owner_and_original_retry_then_export_is_independent() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("persist me".into()));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    // Keep the separate OS lock open but obstruct the record's pre-write read.
    // Restoring the exact file allows the same retained operation to retry.
    let record = app
        .options
        .store
        .join("sessions")
        .join(format!("{}.json", app.entries[0].id()));
    let held = app.options.store.join("held.json");
    std::fs::rename(&record, &held).unwrap();
    std::fs::create_dir(&record).unwrap();
    server.release.add_permits(1);
    until(&mut app, |a| {
        a.entries[0].execution == "Completed"
            && a.entries[0]
                .session
                .as_ref()
                .is_some_and(|s| matches!(s.status().save, SaveStatus::SaveBlocked { .. }))
    })
    .await;
    app.event(ctrl('q'));
    until(&mut app, |a| !a.quitting && a.entries[0].busy.is_none()).await;
    assert!(app.entries[0].session.is_some());
    app.event(ctrl('e'));
    until(&mut app, |a| {
        a.entries[0].notice.contains("Exported actual materials")
    })
    .await;
    let exports: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("tarz-recovery-")
        })
        .collect();
    assert_eq!(exports.len(), 1);
    let recovered = tarz_core::material::load(exports[0].path()).unwrap();
    assert!(format!("{recovered:?}").contains("persist me"));
    std::fs::remove_dir(&record).unwrap();
    std::fs::rename(&held, &record).unwrap();
    app.event(ctrl('r'));
    until(&mut app, |a| a.entries[0].session.is_none()).await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    let reopened = app.host.open(app.entries[0].id().into()).await.unwrap();
    assert!(matches!(
        reopened.status().save,
        SaveStatus::Saved { revision: 2, .. }
    ));
    app.host
        .close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[test]
fn opened_notice_names_unavailable_mcp_servers_on_one_bounded_line() {
    use crate::app::jobs::opened_notice;
    use tarz_core::{McpServerStatus, McpSkippedTool};

    assert_eq!(
        opened_notice(&[], false, None),
        "Opened; only materials are restored, not old work records"
    );
    let skipped = McpSkippedTool {
        name: "mcp_git_bad.name".into(),
        reason: "invalid name".into(),
    };
    let status = vec![
        McpServerStatus {
            id: "git".into(),
            ready: false,
            reason: Some("startup timed out after 1s".into()),
            skipped_tools: Vec::new(),
        },
        McpServerStatus {
            id: "docs".into(),
            ready: false,
            reason: Some("x".repeat(600)),
            skipped_tools: Vec::new(),
        },
        McpServerStatus {
            id: "ok".into(),
            ready: true,
            reason: None,
            skipped_tools: vec![skipped.clone(), skipped],
        },
    ];
    assert_eq!(
        opened_notice(&status, false, None),
        "Opened; MCP server 'git' is unavailable: startup timed out after 1s \
         (+1 more unavailable); 2 MCP tools skipped"
    );
    let long = vec![McpServerStatus {
        id: "docs".into(),
        ready: false,
        reason: Some("漢".repeat(200)),
        skipped_tools: Vec::new(),
    }];
    let notice = opened_notice(&long, false, None);
    assert!(notice.len() <= 256 && !notice.contains('\n'));
    assert!(notice.starts_with("Opened; MCP server 'docs' is unavailable: 漢"));
    assert!(
        opened_notice(&[], true, None).contains("permission mode reverted from full-access to ask")
    );
    let command_clause = "command sandbox unavailable (see /status)";
    let command = opened_notice(&[], false, Some(command_clause));
    assert_eq!(
        command,
        "Opened; command sandbox unavailable (see /status); only materials are restored, \
         not old work records"
    );
    // The fixed command clause precedes the free-form MCP reason, so a long
    // reason cannot truncate it away; the whole notice keeps the same bound.
    let bounded = opened_notice(&long, false, Some(command_clause));
    assert!(
        bounded.starts_with(
            "Opened; command sandbox unavailable (see /status); MCP server 'docs' is unavailable: 漢"
        ),
        "{bounded:?}"
    );
    assert!(
        bounded.len() <= 256 && !bounded.contains('\n'),
        "{bounded:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_running_session_resets_the_execution_label() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("close while running".into()));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    assert_eq!(app.entries[0].execution, "Running");
    app.event(ctrl('w'));
    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].session.is_none()).await;
    assert_eq!(app.entries[0].execution, "No work in this instance");
    assert_eq!(app.entries[0].status, "Closed; F3 to reopen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_material_read_retries_after_one_second_and_then_updates() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    until(&mut app, |a| {
        a.entries[0].material_reads >= 1 && !a.entries[0].refreshing
    })
    .await;
    let key = app.entries[0].key.clone();
    let baseline = app.entries[0].material_reads;
    app.complete(Message::Materials(
        key.clone(),
        Err("injected read failure".into()),
    ));
    assert_eq!(app.entries[0].material_reads, baseline);

    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_millis(950) {
        app.poll();
        assert_eq!(app.entries[0].material_reads, baseline);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    app.poll();
    assert_eq!(app.entries[0].material_reads, baseline + 1);

    app.jobs.abort_all();
    while app.jobs.join_next().await.is_some() {}
    let version = app.entries[0]
        .requested_material_version
        .clone()
        .expect("retry recorded a version");
    let transcript = crate::transcript::Transcript {
        text: "retried conversation".into(),
        ..Default::default()
    };
    app.complete(Message::Materials(
        key,
        Ok((version, std::sync::Arc::new(transcript))),
    ));
    assert!(
        app.entries[0]
            .transcript
            .text
            .contains("retried conversation")
    );
    assert!(app.entries[0].material_retry_after.is_none());
    assert!(!app.entries[0].notice.contains("Material view unavailable"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adapter_panic_is_session_scoped_and_another_session_continues() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    create(&mut app, &server.endpoint).await;
    assert_eq!(app.entries.len(), 2);

    let panicking = app.entries[0].key.clone();
    app.spawn_job(Some(panicking), crate::app::jobs::JobKind::Work, async {
        panic!("injected adapter panic");
    });
    until(&mut app, |a| {
        a.entries[0]
            .notice
            .contains("Background task failed: injected adapter panic")
    })
    .await;
    assert!(app.entries[0].session.is_some());

    app.selected = 1;
    app.entries[1].editor.insert_str("still works");
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app, |a| {
        a.entries[1].execution == "Completed" && !a.entries[1].transcript.text.is_empty()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicked_jobs_clear_only_their_own_state() {
    use crate::app::jobs::JobKind;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let key = crate::app::state::Key {
        id: "entry-id".into(),
        generation: 0,
    };
    app.entries.push(crate::app::state::Entry::new(
        key.id.clone(),
        "model".into(),
    ));
    app.entries[0].busy = Some("closing");
    app.listing = true;

    app.spawn_job(Some(key.clone()), JobKind::Materials, async {
        panic!("materials panic");
    });
    until(&mut app, |a| {
        a.entries[0]
            .notice
            .contains("Background task failed: materials panic")
    })
    .await;
    assert_eq!(app.entries[0].busy, Some("closing"));
    assert!(app.entries[0].material_retry_after.is_some());

    app.spawn_job(Some(key.clone()), JobKind::Work, async {
        panic!("work panic");
    });
    until(&mut app, |a| {
        a.entries[0]
            .notice
            .contains("Background task failed: work panic")
    })
    .await;
    assert_eq!(app.entries[0].busy, Some("closing"));

    app.quitting = true;
    app.spawn_job(Some(key.clone()), JobKind::Close, async {
        panic!("close panic");
    });
    until(&mut app, |a| {
        !a.quitting && a.notice.contains("Exit paused")
    })
    .await;
    assert_eq!(app.entries[0].busy, None);

    app.spawn_job(None, JobKind::List, async {
        panic!("list panic");
    });
    until(&mut app, |a| {
        a.notice.contains("Background task failed: list panic")
    })
    .await;
    assert!(!app.listing);
}

#[test]
#[ignore = "requires a real PTY; run with --test-threads=1"]
fn blocking_job_panic_keeps_raw_mode_until_normal_exit() {
    let _screen = crate::terminal::Screen::enter().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut jobs = tokio::task::JoinSet::new();
    let entered = runtime.enter();
    crate::app::jobs::spawn_job(&mut jobs, None, crate::app::jobs::JobKind::Export, async {
        let _ = crate::app::jobs::blocking(|| panic!("blocking job panic")).await;
        panic!("job panic after a blocking failure");
    });
    drop(entered);
    let message = runtime.block_on(jobs.join_next()).unwrap().unwrap();
    assert!(matches!(message, Message::JobPanicked(..)));
    assert!(crossterm::terminal::is_raw_mode_enabled().unwrap());
}
