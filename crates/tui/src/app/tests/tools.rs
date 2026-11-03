use super::support::{app, ctrl, key, until};
use super::wire;
use crate::{app::state::Entry, input::Form, render};
use causa::kernel::{InvocationId, RoundId, ToolResultStatus, TurnId};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use std::sync::Arc;
use tars_core::session::{
    ProgressUpdate, ToolPhase, ToolProgress, ToolProgressSnapshot, WorkProgress, WorkRef,
};
use tokio::sync::broadcast;

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
fn work() -> WorkRef {
    WorkRef {
        conversation_id: "fixture".into(),
        turn_id: TurnId::new("work1"),
    }
}
fn call(phase: ToolPhase) -> ToolProgress {
    ToolProgress {
        invocation_id: InvocationId {
            turn_id: work().turn_id,
            round_id: RoundId(1),
        },
        call_block_id: causa::runtime::new_block_id(),
        tool_name: "read_file".into(),
        arguments_summary: "{\"path\":\"file\"}".into(),
        phase,
        status: None,
        operation_not_started: false,
        permission_decision: None,
    }
}
fn event(call: ToolProgress) -> Arc<WorkProgress> {
    Arc::new(WorkProgress {
        work: work(),
        invocation_id: call.invocation_id.clone(),
        update: ProgressUpdate::Tool(call),
    })
}

#[test]
fn complete_multiline_tool_details_scroll_unicode_to_the_last_row_and_resize() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.event(Event::Resize(100, 30));
    app.panel = Some(crate::app::Panel::Info {
        title: "Recorded tool materials".into(),
        lines: vec![format!(
            "FIRST_TOOL_LINE\n{}\nFINAL_TOOL_LINE",
            "中文 👩‍💻 e\u{301} ".repeat(500)
        )],
        scroll: 0,
    });
    assert!(screen(&app).contains("FIRST_TOOL_LINE"));
    for _ in 0..80 {
        app.event(key(KeyCode::PageDown));
    }
    assert!(screen(&app).contains("FINAL_TOOL_LINE"));
    app.event(Event::Resize(40, 15));
    app.event(Event::Resize(100, 30));
    for _ in 0..80 {
        app.event(key(KeyCode::PageUp));
    }
    assert!(screen(&app).contains("FIRST_TOOL_LINE"));
}

#[test]
fn bounded_tool_observations_are_identity_scoped_lagged_and_terminal_corrected() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let mut entry = Entry::new("fixture".into(), "model".into());
    entry.work = Some(work());
    let (sender, receiver) = broadcast::channel(2);
    entry.bind_progress(receiver);
    app.entries.push(entry);
    let mut first = call(ToolPhase::Waiting);
    sender.send(event(first.clone())).unwrap();
    app.poll_progress();
    assert!(screen(&app).contains("Waiting"));
    assert!(!screen(&app).contains("Running"));
    first.phase = ToolPhase::Running;
    sender.send(event(first.clone())).unwrap();
    let second = call(ToolPhase::Waiting);
    sender.send(event(second.clone())).unwrap();
    app.poll_progress();
    assert!(screen(&app).contains("Running"));
    assert_eq!(
        app.entries[0].tools.calls.len(),
        2,
        "same arguments remain distinct declarations"
    );
    app.event(Event::Paste("next draft".into()));
    app.event(Event::Resize(70, 20));
    app.event(key(KeyCode::PageUp));
    assert_eq!(app.entries[0].editor.lines(), &["next draft"]);
    first.phase = ToolPhase::Returned;
    first.status = Some(ToolResultStatus::Failed);
    sender.send(event(first.clone())).unwrap();
    app.poll_progress();
    assert!(screen(&app).contains("Returned"));
    assert!(screen(&app).contains("Pending material commit"));
    first.phase = ToolPhase::Committed;
    sender.send(event(first.clone())).unwrap();
    app.poll_progress();
    first.phase = ToolPhase::Running;
    sender.send(event(first.clone())).unwrap();
    app.poll_progress();
    assert_eq!(app.entries[0].tools.calls[0].phase, ToolPhase::Committed);
    for _ in 0..8 {
        sender.send(event(second.clone())).unwrap();
    }
    app.poll_progress();
    assert!(app.entries[0].tools.lost > 0);
    assert!(screen(&app).contains("Tool observations incomplete"));
    let mut unknown = first.clone();
    // Controlled cancellation can return an actual UnknownOutcome payload in U.
    // Its returned phase must not imply that execution completed successfully.
    unknown.phase = ToolPhase::Returned;
    unknown.status = Some(ToolResultStatus::UnknownOutcome);
    let mut notstarted = second.clone();
    notstarted.phase = ToolPhase::NotStarted;
    notstarted.status = Some(ToolResultStatus::UnknownOutcome);
    notstarted.operation_not_started = true;
    app.entries[0].tools.reconcile(&ToolProgressSnapshot {
        calls: vec![unknown, notstarted],
        truncated: false,
    });
    assert!(screen(&app).contains("Execution result unknown"));
    assert!(screen(&app).contains("Not executed"));
    let mut stale = event(second.clone()).as_ref().clone();
    stale.work.turn_id = TurnId::new("oldwork");
    sender.send(Arc::new(stale)).unwrap();
    app.poll_progress();
    assert_eq!(app.entries[0].tools.calls[1].phase, ToolPhase::NotStarted);
    app.entries[0].key.generation += 1;
    sender.send(event(second)).unwrap();
    app.poll_progress();
    assert!(app.entries[0].progress.is_none());
    assert!(app.entries[0].preview.text.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_real_protocols_read_actual_workspace_and_tool_details_are_on_demand() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let marker = format!(
            "retry original material {}",
            causa::runtime::new_block_id().0
        );
        std::fs::write(
            workspace.join("recovery.rs"),
            format!("header\n{marker}\nfooter\n"),
        )
        .unwrap();
        let fixture = wire::Fixture::new(provider, true);
        let mut app = app(dir.path());
        app.form = Some(Form {
            fields: [
                provider.to_owned(),
                "fixture-model".into(),
                fixture.endpoint.clone(),
                "PATH".into(),
                "120".into(),
                workspace.display().to_string(),
                "list_directory,read_file,search_text".into(),
                "ask".into(),
                "".into(),
                "on".into(),
            ]
            .into_iter()
            .map(|value| TextArea::from([value]))
            .collect(),
            generation: (
                (provider == "anthropic").then_some(1024),
                None,
                Default::default(),
            ),
            ..Form::default()
        });
        app.event(ctrl('s'));
        until(&mut app, |a| a.entries[0].session.is_some()).await;
        assert!(fixture.requests.lock().unwrap().is_empty());
        app.event(Event::Paste("Find recovery path".into()));
        app.event(key(KeyCode::Enter));
        until(&mut app, |a| {
            fixture.requests.lock().unwrap().len() == 3 && a.entries[0].tools.calls.len() == 2
        })
        .await;
        let current = screen(&app);
        assert!(current.contains("Tool read_file"), "{current}");
        assert!(
            app.entries[0]
                .tools
                .calls
                .iter()
                .all(|call| call.phase == ToolPhase::Committed)
        );
        app.event(Event::Paste("next input".into()));
        app.event(Event::Resize(60, 18));
        app.event(key(KeyCode::PageUp));
        app.event(key(KeyCode::PageDown));
        fixture.finish();
        until(&mut app, |a| {
            a.entries[0].execution == "Completed" && a.entries[0].transcript.tool_details.len() == 4
        })
        .await;
        assert_eq!(app.entries[0].editor.lines(), &["next input"]);
        assert_eq!(fixture.requests.lock().unwrap().len(), 3);
        let reads = app.entries[0].material_reads;
        app.event(key(KeyCode::F(6)));
        assert!(screen(&app).contains("Recorded tool materials"));
        if let Some(crate::app::Panel::Info { lines, .. }) = &app.panel {
            assert!(lines.iter().any(|line| line.contains(&marker)));
        } else {
            panic!("tool expansion panel missing");
        }
        app.event(key(KeyCode::Esc));
        for _ in 0..10 {
            app.poll();
            let _ = screen(&app);
        }
        assert_eq!(app.entries[0].material_reads, reads);
        assert_eq!(fixture.requests.lock().unwrap().len(), 3);
        let snapshot = app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .read_materials()
            .unwrap();
        assert!(
            serde_json::to_string(&snapshot.context)
                .unwrap()
                .contains(&marker)
        );
        app.entries[0].editor = crate::input::editor();
        app.event(ctrl('w'));
        until(&mut app, |a| a.entries[0].session.is_none()).await;
        app.event(key(KeyCode::F(3)));
        until(&mut app, |a| {
            a.entries[0].session.is_some() && a.entries[0].transcript.tool_details.len() == 4
        })
        .await;
        assert!(
            app.entries[0].tools.calls.is_empty(),
            "reopening never reconstructs old work states"
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), 3);
        app.event(ctrl('q'));
        until(&mut app, |a| a.done).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_blocked_after_real_tools_exports_and_retries_original_material_without_reexecution() {
    use tars_core::application::SaveStatus;
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let marker = format!(
        "retry original material {}",
        causa::runtime::new_block_id().0
    );
    std::fs::write(workspace.join("recovery.rs"), format!("header\n{marker}\n")).unwrap();
    let fixture = wire::Fixture::new("openai-responses", true);
    let mut app = app(dir.path());
    app.form = Some(Form {
        fields: [
            "openai-responses".into(),
            "fixture-model".into(),
            fixture.endpoint.clone(),
            "PATH".into(),
            "120".into(),
            workspace.display().to_string(),
            "read_file,search_text".into(),
            "ask".into(),
            "".into(),
            "on".into(),
        ]
        .into_iter()
        .map(|value| TextArea::from([value]))
        .collect(),
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[0].session.is_some()).await;
    app.event(Event::Paste("Find recovery path".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| fixture.requests.lock().unwrap().len() == 3).await;
    let record = app
        .options
        .store
        .join("sessions")
        .join(format!("{}.json", app.entries[0].id()));
    let held = dir.path().join("held.json");
    std::fs::rename(&record, &held).unwrap();
    std::fs::create_dir(&record).unwrap();
    fixture.finish();
    until(&mut app, |a| {
        a.entries[0].execution == "Completed"
            && matches!(
                a.entries[0].session.as_ref().unwrap().status().save,
                SaveStatus::SaveBlocked { .. }
            )
    })
    .await;
    assert!(app.entries[0].tools.corrected);
    assert!(
        app.entries[0]
            .tools
            .calls
            .iter()
            .all(|call| call.phase == ToolPhase::Committed)
    );
    app.event(ctrl('e'));
    until(&mut app, |a| {
        a.entries[0].notice.contains("Exported actual materials")
    })
    .await;
    let export = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("tars-recovery-")
        })
        .unwrap()
        .path();
    let material = tars_core::material::load(export).unwrap();
    assert!(serde_json::to_string(&material).unwrap().contains(&marker));
    assert_eq!(
        material
            .blocks()
            .iter()
            .filter(|block| matches!(block.content(), causa::kernel::BlockContent::ToolResult(_)))
            .count(),
        2
    );
    std::fs::remove_dir(&record).unwrap();
    std::fs::rename(held, &record).unwrap();
    app.event(ctrl('r'));
    until(&mut app, |a| {
        matches!(
            a.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { revision: 2, .. }
        ) && a.entries[0].busy.is_none()
    })
    .await;
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}
