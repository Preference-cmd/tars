use super::support::{Server, app, ctrl, key, until};
use super::wire;
use crate::{input::Form, render};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use serde_json::json;
use std::time::Duration;
use tars_core::application::ClosePolicy;

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

fn create_form(endpoint: &str, workspace: &std::path::Path, tools: &str) -> Form {
    Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "PATH",
            "120",
            &workspace.display().to_string(),
            tools,
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
        generation: (None, None, Default::default()),
        max_model_attempts: None,
        render_options: Default::default(),
        tool_limits: Default::default(),
        write_limits: None,
        ..Form::default()
    }
}

#[tokio::test]
async fn workspace_rules_trust_popup_guard_scroll_diff_decline_and_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".tars")).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let old_toml = "\
version = 1
[[command]]
prefix = [\"echo\"]
decision = \"allow\"
";
    std::fs::write(workspace.join(".tars/permissions.toml"), old_toml).unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());

    // 1. Create session with initial untrusted workspace allow rule -> popup opens.
    app.form = Some(create_form(&server.endpoint, &workspace, ""));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    let id = app.entries[app.selected].id().to_owned();

    assert!(app.entries[app.selected].workspace_rules_trust.is_some());
    assert!(
        !app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );
    let s = screen(&app);
    assert!(
        s.contains("Trust workspace command allow rules?"),
        "screen: {s}"
    );
    assert!(s.contains("allow: echo"), "screen: {s}");

    // 2. Guard window (500ms): pressing `y`, `Enter`, or `n` inside guard window must NOT decide.
    app.event(key(KeyCode::Char('y')));
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());
    app.event(key(KeyCode::Char('n')));
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());
    app.event(key(KeyCode::Enter));
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());

    // Advance past guard window and confirm (`y`).
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[a.selected].workspace_rules_trust.is_none()
            && a.entries[a.selected].busy.is_none()
    })
    .await;
    assert!(
        app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );
    assert!(
        app.entries[app.selected]
            .notice
            .contains("Trusted workspace rules")
    );

    // Close session so we can modify `.tars/permissions.toml` externally and reopen.
    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[app.selected].session = None;

    // 3. Modify `.tars/permissions.toml` outside sandbox to add many rules + outside_sandbox = true.
    let mut updated = old_toml.to_owned();
    for i in 0..12 {
        updated.push_str(&format!(
            "\n[[command]]\nprefix = [\"cmd{i}\"]\ndecision = \"allow\"\n"
        ));
    }
    updated.push_str(
        "\n[[command]]\nprefix = [\"git\", \"push\"]\ndecision = \"allow\"\noutside_sandbox = true\n",
    );
    std::fs::write(workspace.join(".tars/permissions.toml"), &updated).unwrap();

    // Reopen session in TUI -> popup shows diff + [OUTSIDE SANDBOX] + supports scrolling.
    app.open();
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());

    let s2 = screen(&app);
    assert!(s2.contains("[OUTSIDE SANDBOX]"), "screen: {s2}");
    assert!(s2.contains("WARNING: [OUTSIDE SANDBOX]"), "screen: {s2}");

    // Scroll down to see the diff section.
    app.event(key(KeyCode::PageDown));
    app.event(key(KeyCode::PageDown));
    app.event(key(KeyCode::PageDown));
    assert!(
        app.entries[app.selected]
            .workspace_rules_trust
            .as_ref()
            .unwrap()
            .scroll
            > 0
    );
    let s2_scrolled = screen(&app);
    assert!(
        s2_scrolled.contains("Changes since last trusted version:")
            || s2_scrolled.contains("+prefix = [\"git\", \"push\"]"),
        "scrolled screen: {s2_scrolled}"
    );

    // 4. Decline (`n` after guard window): popup closes, session remains untrusted.
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('n')));
    assert!(app.entries[app.selected].workspace_rules_trust.is_none());
    assert!(
        !app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );
    assert!(app.entries[app.selected].notice.contains("Declined"));

    // Close and reopen again: since declined, popup appears again; this time accept via Enter.
    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[app.selected].session = None;
    app.open();
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.entries[a.selected].workspace_rules_trust.is_none()
            && a.entries[a.selected].busy.is_none()
    })
    .await;
    assert!(
        app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );

    // Reopen one final time: trusted copy matches, so NO popup appears.
    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[app.selected].session = None;
    app.open();
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_rules_trust.is_none());
    assert_eq!(app.entries[app.selected].id(), id);

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn tui_trust_confirmation_activates_allow_rules_for_run_command() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".tars")).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    std::fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\n",
    )
    .unwrap();

    let fixture = wire::Fixture::with_responses(
        "openai-chat",
        vec![
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-trusted"}),
                "call_1",
            ),
            wire::answer_with_text("openai-chat", "command completed"),
        ],
    );

    let mut app = app(dir.path());
    app.form = Some(create_form(&fixture.endpoint, &workspace, "run_command"));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_rules_trust.is_some());

    // Confirm trust after guard window.
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[a.selected].workspace_rules_trust.is_none()
            && a.entries[a.selected].busy.is_none()
    })
    .await;

    // Submit work that calls `echo tui-trusted`: must run without any approval prompt!
    app.entries[app.selected].editor.insert_str("run echo");
    app.event(key(KeyCode::Enter));

    let timeout = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        app.poll();
        assert!(
            app.entries[app.selected].approval_queue.is_empty(),
            "trusted workspace allow rule must not trigger an approval prompt"
        );
        if app.entries[app.selected].execution == "Completed" {
            break;
        }
        assert!(tokio::time::Instant::now() < timeout, "work timed out");
        tokio::select! {
            Some(message) = app.jobs.join_next(), if !app.jobs.is_empty() => {
                if let Ok(message) = message {
                    app.complete(message);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}
