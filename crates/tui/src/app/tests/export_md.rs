use super::support::{Server, app, create, key, until};
use crate::{
    app::{COMMANDS, Panel},
    input::slash_command_takes_args,
};
use crossterm::event::{Event, KeyCode};
use tars_core::{
    application::{ClosePolicy, SaveStatus, SpawnAgentOutcome, SpawnAgentRequest},
    definition::ChildNarrowingRequest,
    events::{AgentInteraction, OnParentComplete, OperationExecutionMode},
};

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

#[test]
fn export_md_is_registered_in_commands_help_and_completion() {
    assert!(COMMANDS.iter().any(|(name, _)| *name == "export-md"));
    assert!(slash_command_takes_args("export-md"));

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    command(&mut app, "help");
    match &app.panel {
        Some(Panel::Info { lines, .. }) => {
            assert!(
                lines.iter().any(|line| line.contains("/export-md [path]")),
                "{lines:?}"
            );
        }
        other => panic!("expected help info panel, got {}", other.is_some()),
    }
    app.panel = None;

    // Without an open session, /export-md sets a friendly notice without panicking.
    command(&mut app, "export-md");
    assert!(
        app.notice.contains("Open a session first"),
        "notice: {}",
        app.notice
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_md_exports_root_explicit_refuses_overwrite_and_exports_child_agent() {
    let dir = tempfile::tempdir().unwrap();
    let recovery = dir.path().join("recovery");
    std::fs::create_dir_all(&recovery).unwrap();
    let mut app = app(dir.path());
    app.options.recovery_dir = recovery.clone();
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // Complete one root turn.
    app.event(Event::Paste("root user prompt".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app, |a| {
        !a.entries[a.selected].turn_in_flight
            && a.entries[a.selected].session.as_ref().is_some_and(|s| {
                s.status().active_work.is_none()
                    && matches!(s.status().save, SaveStatus::Saved { .. })
            })
    })
    .await;

    // Tab completion completes `/export-` -> `/export-md `.
    app.event(Event::Paste("/export-".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/export-md "]);
    app.entries[app.selected].editor = crate::input::editor();

    // 1. Default /export-md writes into recovery_dir, skips tars:prelude:*, keeps content.
    command(&mut app, "export-md");
    until(&mut app, |a| {
        a.entries[a.selected].busy.is_none()
            && a.entries[a.selected]
                .notice
                .starts_with("Exported Markdown:")
    })
    .await;

    let exported_files: Vec<_> = std::fs::read_dir(&recovery)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(exported_files.len(), 1, "{exported_files:?}");
    let default_md = std::fs::read_to_string(&exported_files[0]).unwrap();
    assert!(default_md.contains("## User\n\nroot user prompt\n"));
    assert!(default_md.contains("## Assistant\n\n"));
    assert!(!default_md.contains("tars:prelude:"));
    assert!(!default_md.contains("<available_skills>"));

    // 2. Explicit path /export-md <path> writes there and refuses to overwrite an existing file.
    let explicit_path = dir.path().join("explicit-export.md");
    command(&mut app, &format!("export-md {}", explicit_path.display()));
    until(&mut app, |a| {
        a.entries[a.selected].busy.is_none()
            && a.entries[a.selected]
                .notice
                .starts_with("Exported Markdown:")
    })
    .await;
    let explicit_md = std::fs::read_to_string(&explicit_path).unwrap();
    assert!(explicit_md.contains("root user prompt"));

    std::fs::write(&explicit_path, "keep existing contents\n").unwrap();
    command(&mut app, &format!("export-md {}", explicit_path.display()));
    until(&mut app, |a| {
        a.entries[a.selected].busy.is_none()
            && a.entries[a.selected]
                .notice
                .contains("Markdown export failed:")
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(&explicit_path).unwrap(),
        "keep existing contents\n"
    );

    // 3. Spawn a child agent, let it finish, switch view to it, and /export-md.
    let session = app.entries[app.selected].session.clone().unwrap();
    let root_id = app.entries[app.selected].id().to_owned();
    server.release.add_permits(1);
    let SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(SpawnAgentRequest {
            caller_agent_id: root_id,
            caller_operation_id: None,
            source_block_id: None,
            name: "researcher".into(),
            preset_name: None,
            role_prompt: None,
            prompt: "child specific task".into(),
            narrowing: ChildNarrowingRequest::default(),
            interaction: AgentInteraction::Reusable,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            timeout_secs: None,
            max_rounds: None,
            coalesce_key: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected background spawn");
    };

    until(&mut app, |a| {
        a.entries[a.selected]
            .session
            .as_ref()
            .is_some_and(|s| s.active_finite_background_tasks() == 0)
    })
    .await;

    command(&mut app, "agents researcher");
    assert_eq!(
        app.entries[app.selected].viewed_agent_id.as_deref(),
        Some(spawn.agent_id.as_str())
    );
    let child_export = dir.path().join("child-export.md");
    command(&mut app, &format!("export-md {}", child_export.display()));
    until(&mut app, |a| {
        a.entries[a.selected].busy.is_none()
            && a.entries[a.selected]
                .notice
                .starts_with("Exported Markdown:")
    })
    .await;
    let child_md = std::fs::read_to_string(&child_export).unwrap();
    assert!(child_md.contains("- **Agent:** researcher\n"), "{child_md}");
    assert!(child_md.contains("child specific task"), "{child_md}");
    assert!(
        !child_md.contains("## User\n\nroot user prompt"),
        "{child_md}"
    );

    let target = session.target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}
