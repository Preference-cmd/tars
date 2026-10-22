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
        opened_notice(&[]),
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
        opened_notice(&status),
        "Opened; MCP server 'git' is unavailable: startup timed out after 1s \
         (+1 more unavailable); 2 MCP tools skipped"
    );
    let long = vec![McpServerStatus {
        id: "docs".into(),
        ready: false,
        reason: Some("漢".repeat(200)),
        skipped_tools: Vec::new(),
    }];
    let notice = opened_notice(&long);
    assert!(notice.len() <= 256 && !notice.contains('\n'));
    assert!(notice.starts_with("Opened; MCP server 'docs' is unavailable: 漢"));
}
