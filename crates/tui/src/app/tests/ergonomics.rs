use super::support::{Server, app, ctrl, key, until};
use crate::{input::Form, render};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_textarea::TextArea;
use tars_core::application::ClosePolicy;
use tars_core::permission::PermissionMode;

fn create_form(endpoint: &str, workspace: Option<&std::path::Path>) -> Form {
    let ws = workspace
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "env:PATH",
            "120",
            &ws,
            "",
            "ask",
            "",
            "on",
            "off",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        selected: 1,
        error: String::new(),
        limits: (256, 10, 64),
        generation: (None, None, Default::default()),
        max_model_attempts: None,
        render_options: Default::default(),
        tool_limits: Default::default(),
        write_limits: None,
        ..Form::default()
    }
}

#[tokio::test]
async fn prompt_history_navigates_preserves_draft_dedups_and_respects_multiline_bounds() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    server.release.add_permits(4);
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);

    app.form = Some(create_form(&server.endpoint, None));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;

    // Submit three prompts (two consecutive identical "second" prompts should deduplicate).
    for prompt in ["first prompt", "second", "second", "line A\nline B"] {
        app.event(Event::Paste(prompt.into()));
        app.event(ctrl('s'));
        assert!(
            !app.entries[app.selected].has_draft(),
            "submit rejected with notice: {}",
            app.entries[app.selected].notice
        );
        until(&mut app, |a| {
            a.entries[a.selected].execution == "Completed"
                && a.entries[a.selected].session.as_ref().is_some_and(|s| {
                    s.status().active_work.is_none()
                        && matches!(
                            s.status().save,
                            tars_core::application::SaveStatus::Saved { .. }
                        )
                })
        })
        .await;
    }

    assert_eq!(
        app.entries[app.selected].input_history,
        vec![
            "first prompt".to_owned(),
            "second".to_owned(),
            "line A\nline B".to_owned(),
        ]
    );

    // Type an unsent draft, then press Up to enter history.
    app.event(Event::Paste("unsent draft".into()));
    app.event(key(KeyCode::Up));
    assert_eq!(
        app.entries[app.selected].editor.lines(),
        &["line A", "line B"]
    );
    assert_eq!(app.entries[app.selected].editor.cursor().0, 0);

    // Pressing Down inside multiline history entry moves down to line 1 first (does NOT skip).
    app.event(key(KeyCode::Down));
    assert_eq!(app.entries[app.selected].editor.cursor().0, 1);
    assert_eq!(
        app.entries[app.selected].editor.lines(),
        &["line A", "line B"]
    );

    // Pressing Down at the bottom line of the newest entry restores the saved draft.
    app.event(key(KeyCode::Down));
    assert_eq!(app.entries[app.selected].editor.lines(), &["unsent draft"]);
    assert!(app.entries[app.selected].history_cursor.is_none());

    // Step all the way back to the oldest entry.
    app.event(key(KeyCode::Up)); // line A\nline B (cursor at line 0)
    app.event(key(KeyCode::Up)); // second
    assert_eq!(app.entries[app.selected].editor.lines(), &["second"]);
    app.event(key(KeyCode::Up)); // first prompt
    assert_eq!(app.entries[app.selected].editor.lines(), &["first prompt"]);
    // Extra Up at oldest entry stays on oldest entry.
    app.event(key(KeyCode::Up));
    assert_eq!(app.entries[app.selected].editor.lines(), &["first prompt"]);

    // Ordinary text edit clears history cursor.
    app.event(key(KeyCode::Char('!')));
    assert!(app.entries[app.selected].history_cursor.is_none());

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test]
async fn tab_completion_completes_slash_commands_and_at_paths_without_stealing_shift_tab() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws_root = ws.path().canonicalize().unwrap();
    std::fs::create_dir(ws_root.join("src")).unwrap();
    std::fs::create_dir(ws_root.join(".git")).unwrap();
    std::fs::write(ws_root.join("src/lib.rs"), "").unwrap();
    std::fs::write(ws_root.join("src/main.rs"), "").unwrap();
    std::fs::write(ws_root.join("Cargo.toml"), "").unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());
    app.form = Some(create_form(&server.endpoint, Some(&ws_root)));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;

    // 1. Slash command unique match with args ("/ag" -> "/agents ").
    app.event(Event::Paste("/ag".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/agents "]);

    // Clear editor and test multi-match slash command ("/c" -> shows candidates).
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("/c".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/c"]);
    assert!(
        app.entries[app.selected].notice.contains("/compact")
            && app.entries[app.selected].notice.contains("/cancel"),
        "notice: {}",
        app.entries[app.selected].notice
    );

    // 2. @path completion: single dir match "@sr" -> "@src/", then "@src/l" -> "@src/lib.rs ".
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("inspect @sr".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["inspect @src/"]);
    app.event(Event::Paste("l".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(
        app.entries[app.selected].editor.lines(),
        &["inspect @src/lib.rs "]
    );

    // Multi-match @src/ lists candidates and excludes .git.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("@src/".into()));
    app.event(key(KeyCode::Tab));
    assert!(
        app.entries[app.selected].notice.contains("src/lib.rs")
            && app.entries[app.selected].notice.contains("src/main.rs"),
        "notice: {}",
        app.entries[app.selected].notice
    );

    // Unsafe @../ and @.git/ are rejected without leaking entries.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("@../".into()));
    app.event(key(KeyCode::Tab));
    assert!(
        app.entries[app.selected]
            .notice
            .contains("Invalid workspace path")
    );

    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("@.git/".into()));
    app.event(key(KeyCode::Tab));
    assert!(
        app.entries[app.selected]
            .notice
            .contains("No workspace path matches")
    );

    // 3. Shift-Tab and BackTab still cycle permission mode instead of completing.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("@sr".into()));
    let mode_before = app.entries[app.selected]
        .session
        .as_ref()
        .unwrap()
        .permission_mode()
        .unwrap()
        .0;
    assert_eq!(mode_before, PermissionMode::Ask);
    app.event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)));
    assert_eq!(
        app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );
    assert_eq!(app.entries[app.selected].editor.lines(), &["@sr"]);

    // 4. Regression: pressing Tab at column 0 on "/ag" must not panic or mutate editor.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("/ag".into()));
    app.event(key(KeyCode::Home));
    assert_eq!(app.entries[app.selected].editor.cursor(), (0, 0));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/ag"]);
    assert_eq!(app.entries[app.selected].editor.cursor(), (0, 0));

    // 5. Regression: pressing Tab on @path preceded by multibyte UTF-8 text completes safely.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("请查看 @src/m".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(
        app.entries[app.selected].editor.lines(),
        &["请查看 @src/main.rs "]
    );

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}
