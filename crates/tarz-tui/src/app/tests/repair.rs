use super::support::{Server, app, create, ctrl, key, until};
use crate::{
    app::{Panel, state::Entry},
    render,
};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use tarz_core::{
    application::{ClosePolicy, PreparedCreate, SaveStatus},
    definition::{CredentialRef, Provider, SessionDefinition},
};
#[path = "../../../../tarz-core/tests/common/workspace_http.rs"]
#[allow(dead_code)] // This shared fixture also supports tool replies used by core/CLI tests.
mod wire;

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
async fn incomplete(app: &mut crate::app::App, provider: Provider, endpoint: &str) {
    let definition = SessionDefinition {
        provider,
        model: "fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 16,
        max_model_rounds: 4,
        max_tool_calls: 4,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        workspace_root: None,
        tools: vec![],
        tool_limits: Default::default(),
    };
    let context = Context::from_blocks(vec![
        tarz_core::application::text_input("old input"),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolCall(ToolCallPayload::new(
                "read_file",
                serde_json::json!({"path":"old"}),
            )),
            BlockMeta::default().with_provider_call_id("old-unanswered"),
        ),
    ])
    .unwrap();
    let session = app
        .host
        .create(PreparedCreate::persistent(definition, context))
        .await
        .unwrap();
    let mut entry = Entry::new(
        session.target().conversation_id().as_str().into(),
        "fixture".into(),
    );
    entry.session = Some(session);
    entry.execution = "Original execution label".into();
    app.entries.push(entry);
    until(app, |app| {
        app.entries[0].material_version.is_some() && app.entries[0].rows.is_some()
    })
    .await;
}
async fn inspect(app: &mut crate::app::App) {
    app.event(key(KeyCode::F(7)));
    until(
        app,
        |app| matches!(&app.panel, Some(Panel::Repair(panel)) if !panel.busy),
    )
    .await;
}
async fn plan(app: &mut crate::app::App) {
    app.event(key(KeyCode::Char('r')));
    until(app, |app| matches!(&app.panel, Some(Panel::Repair(panel)) if !panel.busy && panel.plan.is_some())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_ui_preview_discard_resize_switch_apply_save_and_explicit_send() {
    for (protocol, provider) in [
        ("openai-chat", Provider::OpenAiChat),
        ("openai-responses", Provider::OpenAiResponses),
        ("anthropic", Provider::Anthropic),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let fixture = wire::Fixture::new(protocol, vec![wire::Reply::Answer], None);
        fixture.release();
        let mut app = app(dir.path());
        incomplete(&mut app, provider, &fixture.endpoint).await;
        let session = app.entries[0].session.as_ref().unwrap().clone();
        let before = session.read_materials().unwrap();
        app.event(Event::Paste("保留 draft 👩‍💻".into()));
        app.event(key(KeyCode::Enter));
        assert!(app.entries[0].notice.contains("F7"));
        assert!(app.entries[0].work.is_none());
        assert_eq!(app.entries[0].editor.lines(), &["保留 draft 👩‍💻"]);
        app.event(key(KeyCode::F(7)));
        let jobs = app.jobs.len();
        for _ in 0..20 {
            app.event(key(KeyCode::F(7)));
        }
        assert_eq!(
            app.jobs.len(),
            jobs,
            "one inspection per instance even when refresh repeats"
        );
        until(
            &mut app,
            |app| matches!(&app.panel, Some(Panel::Repair(panel)) if !panel.busy),
        )
        .await;
        assert!(screen(&app).contains("1 unpaired declarations"));
        plan(&mut app).await;
        let explanation = match &app.panel {
            Some(Panel::Repair(panel)) => panel.plan.as_ref().unwrap().summary().explanation_id,
            _ => unreachable!(),
        };
        for size in [(42, 15), (140, 50), (80, 24), (110, 32)] {
            app.event(Event::Resize(size.0, size.1));
            app.event(key(KeyCode::PageDown));
            app.event(key(KeyCode::PageUp));
            for _ in 0..10 {
                app.poll();
                let _ = screen(&app);
            }
            assert!(
                matches!(&app.panel, Some(Panel::Repair(panel)) if panel.plan.as_ref().unwrap().summary().explanation_id == explanation)
            );
        }
        assert_eq!(session.read_materials().unwrap().version, before.version);
        app.entries
            .push(Entry::new("other-view".into(), "other".into()));
        app.event(key(KeyCode::F(4)));
        assert!(app.panel.is_none());
        assert_eq!(app.selected, 1);
        app.event(key(KeyCode::F(4)));
        assert_eq!(app.selected, 0);
        assert_eq!(app.entries[0].editor.lines(), &["保留 draft 👩‍💻"]);
        assert_eq!(
            session.read_materials().unwrap().context.blocks(),
            before.context.blocks()
        );
        inspect(&mut app).await;
        plan(&mut app).await;
        app.event(key(KeyCode::Enter));
        until(&mut app, |app| {
            app.entries[0].notice.starts_with("Repair applied;")
        })
        .await;
        assert_eq!(app.entries[0].execution, "Original execution label");
        assert_eq!(app.entries[0].editor.lines(), &["保留 draft 👩‍💻"]);
        assert!(matches!(
            session.status().save,
            SaveStatus::Saved { revision: 2, .. }
        ));
        assert_eq!(
            fixture.accepted.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        app.event(key(KeyCode::Esc));
        app.event(key(KeyCode::Enter));
        until(&mut app, |app| {
            app.entries[0].execution == "Completed" && app.entries[0].status.contains("saved r3")
        })
        .await;
        fixture.wait_for_requests(1).await;
        assert!(
            fixture.requests.lock().unwrap()[0]
                .to_string()
                .contains("保留 draft")
        );
        assert!(
            !fixture.requests.lock().unwrap()[0]
                .to_string()
                .contains("old-unanswered")
        );
        assert!(!app.entries[0].has_draft());
        app.host
            .close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_ui_stale_candidate_and_late_inspection_do_not_touch_drafts_or_new_panels() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    incomplete(&mut app, Provider::OpenAiChat, "http://127.0.0.1:1/unused").await;
    let session = app.entries[0].session.as_ref().unwrap().clone();
    app.event(Event::Paste("unchanged draft".into()));
    inspect(&mut app).await;
    plan(&mut app).await;
    let snapshot = session.read_materials().unwrap();
    session
        .edit(snapshot.version, snapshot.context.into_blocks())
        .unwrap();
    session.wait_saved().await.unwrap();
    let before = session.read_materials().unwrap();
    app.event(key(KeyCode::Enter));
    until(&mut app, |app| {
        app.entries[0].notice.starts_with("Repair rejected")
    })
    .await;
    assert_eq!(app.entries[0].editor.lines(), &["unchanged draft"]);
    assert_eq!(session.read_materials().unwrap().version, before.version);
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::F(7)));
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::F(1)));
    until(&mut app, |app| app.entries[0].repair_job.is_none()).await;
    assert!(matches!(app.panel, Some(Panel::Commands { .. })));
    assert_eq!(session.read_materials().unwrap().version, before.version);
    app.host
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_ui_waiting_panel_dismissal_does_not_cancel_the_original_work() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("first input".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    let work = app.entries[0].work.clone().unwrap();
    let session = app.entries[0].session.as_ref().unwrap().clone();
    app.event(Event::Paste("next draft".into()));
    app.event(key(KeyCode::F(7)));
    assert!(screen(&app).contains("Waiting for the current work"));
    app.event(key(KeyCode::Esc));
    assert!(matches!(
        session.observe(&work).unwrap().state,
        tarz_core::session::WorkState::Running
    ));
    assert_eq!(app.entries[0].editor.lines(), &["next draft"]);
    server.release.add_permits(1);
    until(&mut app, |app| {
        app.entries[0].repair_job.is_none() && app.entries[0].execution == "Completed"
    })
    .await;
    assert!(app.panel.is_none());
    assert_eq!(app.entries[0].editor.lines(), &["next draft"]);
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    app.host
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_ui_save_failure_keeps_applied_materials_and_draft_then_retries_same_commit() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    incomplete(&mut app, Provider::Anthropic, "http://127.0.0.1:1/unused").await;
    let session = app.entries[0].session.as_ref().unwrap().clone();
    app.event(Event::Paste("keep after repair".into()));
    inspect(&mut app).await;
    plan(&mut app).await;
    let sessions = dir.path().join("store/sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let probe = sessions.join("permission-probe");
    if std::fs::write(&probe, "").is_ok() {
        std::fs::remove_file(probe).unwrap();
        std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
        app.host
            .close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
        return;
    }
    app.event(key(KeyCode::Enter));
    until(&mut app, |app| {
        app.entries[0]
            .notice
            .starts_with("Materials repaired, saving needs attention")
    })
    .await;
    let commit = match session.status().save {
        SaveStatus::SaveBlocked { commit_id, .. } => commit_id,
        _ => panic!("blocked"),
    };
    assert_eq!(app.entries[0].editor.lines(), &["keep after repair"]);
    assert!(session.inspect_repair(None).unwrap().diagnosis.is_ready());
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    app.event(ctrl('r'));
    until(&mut app, |app| {
        app.entries[0].busy.is_none() && app.entries[0].status.contains("saved r2")
    })
    .await;
    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sessions.join(format!(
            "{}.json",
            session.target().conversation_id().as_str()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["last_commit_id"], commit);
    assert_eq!(app.entries[0].execution, "Original execution label");
    assert_eq!(app.entries[0].editor.lines(), &["keep after repair"]);
    app.host
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}
