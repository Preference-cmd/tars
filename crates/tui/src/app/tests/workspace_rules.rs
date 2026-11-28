use super::support::{Server, app, create_with_definition, ctrl, fixture_definition, key, until};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::wire;
use crate::render;
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::json;
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

async fn create_session(
    app: &mut crate::app::App,
    endpoint: &str,
    workspace: &std::path::Path,
    tools: &str,
) {
    let mut def = fixture_definition("openai-chat", endpoint);
    def.workspace_root = Some(workspace.to_path_buf());
    def.tools = tars_core::config::parse_tools(tools).expect("tools must parse");
    create_with_definition(app, def).await;
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
    create_session(&mut app, &server.endpoint, &workspace, "").await;
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
    create_session(&mut app, &fixture.endpoint, &workspace, "run_command").await;
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn settle_turn(app: &mut crate::app::App) {
    until(app, |a| {
        a.entries[0].execution == "Completed"
            && matches!(
                a.entries[0].session.as_ref().unwrap().status().save,
                tars_core::application::SaveStatus::Saved { .. }
            )
    })
    .await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc07_rc08_rc10_tui_command_options_session_workspace_rules_and_slash_rules() {
    use super::support::advance_past_approval_window;
    use tars_core::permission::{
        PermissionDecision, PermissionDecisionKind, RuleScope, RuleSource,
    };

    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let user_rules_dir = dir.path().join("user-tars");
    std::fs::create_dir_all(&user_rules_dir).unwrap();
    let user_rules_path = user_rules_dir.join("permissions.toml");
    std::fs::write(
        &user_rules_path,
        "version = 1\n[[command]]\nprefix = [\"uname\"]\ndecision = \"deny\"\n",
    )
    .unwrap();

    // Pre-create an untrusted workspace rules file with an `ask` rule on `date` and an `allow` rule on `pwd`
    std::fs::create_dir_all(workspace.join(".tars")).unwrap();
    std::fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"date\"]\ndecision = \"ask\"\n\n[[command]]\nprefix = [\"pwd\"]\ndecision = \"allow\"\n",
    )
    .unwrap();

    let fixture = wire::Fixture::with_responses(
        "openai-chat",
        vec![
            // Turn 1: `echo tui-session` while workspace is UNTRUSTED -> 3 options (no w), press `s`
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-session"}),
                "c1",
            ),
            wire::answer_with_text("openai-chat", "echo session ok"),
            // Turn 2: `echo tui-session 2` -> allowed by session rule without prompt
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-session 2"}),
                "c2",
            ),
            wire::answer_with_text("openai-chat", "echo session 2 ok"),
            // Turn 3: unmatchable `echo $HOME` -> 2 options only (s/w ignored), press `y`
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo $HOME"}),
                "c3",
            ),
            wire::answer_with_text("openai-chat", "unmatchable ok"),
            // Turn 4: `uname` -> denied by user rule, shows provenance in tool view
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "uname"}),
                "c4",
            ),
            wire::answer_with_text("openai-chat", "uname denied"),
            // Turn 5: after removing session rule via `/rules remove 1`, `echo` prompts again; now after trusting workspace we can test `w` too!
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-session after-remove"}),
                "c5",
            ),
            wire::answer_with_text("openai-chat", "after remove ok"),
        ],
    );

    let mut app = app(dir.path());
    app.host = tars_core::application::SessionHost::new(Some(dir.path().join("store")))
        .with_user_rules_path(Some(user_rules_path.clone()));
    app.user_rules_path = Some(user_rules_path.clone());

    create_session(&mut app, &fixture.endpoint, &workspace, "run_command").await;
    let session_id = app.entries[app.selected].id().to_owned();

    // Decline workspace trust initially so we verify 3-option untrusted behavior (`w` hidden & ignored).
    assert!(app.entries[0].workspace_rules_trust.is_some());
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('n')));
    assert!(app.entries[0].workspace_rules_trust.is_none());

    // Turn 1: `echo tui-session`
    app.entries[0].editor.insert_str("turn 1");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| !a.entries[0].approval_queue.is_empty()).await;

    let view1 = screen(&app);
    assert!(view1.contains("Rule prefix: echo tui-session"), "{view1}");
    assert!(view1.contains("2/s allow for session"), "{view1}");
    assert!(!view1.contains("3/w always in workspace"), "{view1}");

    // Inside 0.5s guard window, `s` must be ignored; `w`/`3` must also be ignored after guard!
    app.event(key(KeyCode::Char('s')));
    assert!(!app.entries[0].approval_queue.is_empty());
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('w')));
    app.event(key(KeyCode::Char('3')));
    assert!(!app.entries[0].approval_queue.is_empty());

    // Press `s` to allow for session.
    app.event(key(KeyCode::Char('s')));
    settle_turn(&mut app).await;
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .command_rules_snapshot()
            .session_rules()
            .len(),
        1
    );

    // Turn 2: `echo tui-session 2` must run without prompt and display `allowed by session rule`.
    app.entries[0].editor.insert_str("turn 2");
    app.event(key(KeyCode::Enter));
    settle_turn(&mut app).await;
    assert!(app.entries[0].approval_queue.is_empty());
    let view2 = screen(&app);
    assert!(
        view2.contains("allowed by session rule `echo tui-session`")
            || app.entries[0]
                .tools
                .lines()
                .iter()
                .any(|l| l.contains("allowed by session rule")),
        "tools lines: {:?}\nscreen: {view2}",
        app.entries[0].tools.lines()
    );

    // Turn 3: unmatchable `echo $HOME` shows 2 options only and ignores `s`/`2`/`w`/`3`.
    app.entries[0].editor.insert_str("turn 3");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| !a.entries[0].approval_queue.is_empty()).await;
    let view3 = screen(&app);
    assert!(!view3.contains("Rule prefix:"), "{view3}");
    assert!(!view3.contains("allow for session"), "{view3}");
    assert!(!view3.contains("always in workspace"), "{view3}");
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('s')));
    app.event(key(KeyCode::Char('2')));
    app.event(key(KeyCode::Char('w')));
    assert!(!app.entries[0].approval_queue.is_empty());
    app.event(key(KeyCode::Char('y')));
    settle_turn(&mut app).await;

    // Turn 4: `uname` denied by user rule -> shows `denied by user rule \`uname\`` in ToolView.
    app.entries[0].editor.insert_str("turn 4");
    app.event(key(KeyCode::Enter));
    settle_turn(&mut app).await;
    assert!(
        app.entries[0]
            .tools
            .lines()
            .iter()
            .any(|l| l.contains("denied by user rule `uname`")),
        "{:?}",
        app.entries[0].tools.lines()
    );

    // Inspect `/rules` panel: shows User rules, Workspace rules [UNTRUSTED], and Session rules (1-based).
    app.event(key(KeyCode::Char('/')));
    for ch in "rules".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
    let rules_screen = screen(&app);
    assert!(
        rules_screen.contains("User rules (read-only):"),
        "{rules_screen}"
    );
    assert!(rules_screen.contains("deny: uname"), "{rules_screen}");
    assert!(
        rules_screen.contains("Workspace rules [UNTRUSTED"),
        "{rules_screen}"
    );
    assert!(
        rules_screen.contains("Session rules (in-memory only"),
        "{rules_screen}"
    );
    assert!(
        rules_screen.contains("1. allow: echo tui-session"),
        "{rules_screen}"
    );
    app.event(key(KeyCode::Esc));

    // Remove rule #1 via `/rules remove 1`.
    app.event(key(KeyCode::Char('/')));
    for ch in "rules remove 1".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
    assert!(app.notice.contains("Removed in-memory session rule #1"));
    assert!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .command_rules_snapshot()
            .session_rules()
            .is_empty()
    );

    // Turn 5: now that session rule #1 was removed, `echo` prompts again!
    app.entries[0].editor.insert_str("turn 5");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| !a.entries[0].approval_queue.is_empty()).await;
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('2'))); // re-add via `2`
    settle_turn(&mut app).await;
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .command_rules_snapshot()
            .session_rules()
            .len(),
        1
    );

    // Clear all via `/rules clear`.
    app.event(key(KeyCode::Char('/')));
    for ch in "rules clear".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
    assert!(app.notice.contains("Cleared 1 in-memory session rule(s)"));

    // Also test `ToolView` formatting with `AskedAndAllowed` rule provenance.
    let mut asked_call = app.entries[0].tools.calls[0].clone();
    asked_call.permission_decision = Some(PermissionDecision {
        mode: tars_core::permission::PermissionMode::FullAccess,
        revision: 0,
        kind: PermissionDecisionKind::AskedAndAllowed,
        rule: Some(RuleSource {
            scope: RuleScope::Workspace,
            prefix: vec!["git".into(), "push".into()],
        }),
    });
    app.entries[0].tools.apply(&asked_call, app.clock.now());
    assert!(
        app.entries[0]
            .tools
            .lines()
            .iter()
            .any(|l| l.contains("asked by workspace rule `git push`")),
        "{:?}",
        app.entries[0].tools.lines()
    );

    // Reopen session -> session rules are cleared and user_rules_path was never touched.
    app.event(ctrl('w'));
    until(&mut app, |a| a.entries[0].session.is_none()).await;
    assert_eq!(app.entries[0].id(), session_id);
    app.event(key(KeyCode::F(3)));
    until(&mut app, |a| a.entries[0].session.is_some()).await;
    assert!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .command_rules_snapshot()
            .session_rules()
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(&user_rules_path).unwrap(),
        "version = 1\n[[command]]\nprefix = [\"uname\"]\ndecision = \"deny\"\n"
    );

    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc07_rc10_tui_four_option_workspace_append_and_outside_sandbox_render() {
    use super::support::advance_past_approval_window;

    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let user_rules_dir = dir.path().join("user-tars");
    std::fs::create_dir_all(&user_rules_dir).unwrap();
    let user_rules_path = user_rules_dir.join("permissions.toml");

    let fixture = wire::Fixture::with_responses(
        "openai-chat",
        vec![
            // Turn 1: `echo tui-ws` in trusted workspace -> 4 options, press `w`
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-ws"}),
                "c1",
            ),
            wire::answer_with_text("openai-chat", "ws rule added"),
            // Turn 2: `echo tui-ws 2` -> allowed by workspace rule without prompt
            wire::tool(
                "openai-chat",
                "run_command",
                json!({"command": "echo tui-ws 2"}),
                "c2",
            ),
            wire::answer_with_text("openai-chat", "ws rule reused"),
            // Turn 3: `python3 -c 'print(1)'` outside sandbox -> shows full interpreter segment + outside_sandbox = true
            wire::tool(
                "openai-chat",
                "run_command",
                json!({
                    "command": "python3 -c 'print(1)'",
                    "outside_sandbox": true,
                    "justification": "test outside rendering"
                }),
                "c3",
            ),
            wire::answer_with_text("openai-chat", "outside denied"),
        ],
    );

    let mut app = app(dir.path());
    app.host = tars_core::application::SessionHost::new(Some(dir.path().join("store")))
        .with_user_rules_path(Some(user_rules_path.clone()));
    app.user_rules_path = Some(user_rules_path.clone());

    create_session(&mut app, &fixture.endpoint, &workspace, "run_command").await;
    assert!(app.entries[0].workspace_rules_trust.is_none());

    // Turn 1: 4 options shown, press `w` to save to `.tars/permissions.toml`
    app.entries[0].editor.insert_str("turn 1");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| !a.entries[0].approval_queue.is_empty()).await;
    let view1 = screen(&app);
    assert!(view1.contains("Rule prefix: echo tui-ws"), "{view1}");
    assert!(view1.contains("3/w always in workspace"), "{view1}");
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('w')));
    settle_turn(&mut app).await;

    assert_eq!(
        std::fs::read_to_string(workspace.join(".tars/.gitignore")).unwrap(),
        "*\n"
    );
    let ws_toml = std::fs::read_to_string(workspace.join(".tars/permissions.toml")).unwrap();
    assert!(
        ws_toml.contains("prefix = [\"echo\", \"tui-ws\"]"),
        "{ws_toml}"
    );
    assert!(!user_rules_path.exists());

    // Turn 2: runs directly without prompting
    app.entries[0].editor.insert_str("turn 2");
    app.event(key(KeyCode::Enter));
    settle_turn(&mut app).await;
    assert!(app.entries[0].approval_queue.is_empty());

    // Turn 3: outside sandbox interpreter command renders whole segment + outside_sandbox = true
    app.entries[0].editor.insert_str("turn 3");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| !a.entries[0].approval_queue.is_empty()).await;
    let view3 = screen(&app);
    assert!(
        view3.contains("Rule prefix: python3 -c print(1) (includes outside_sandbox = true)"),
        "{view3}"
    );
    assert!(
        view3.contains("outside_sandbox = true bypasses sandbox"),
        "{view3}"
    );
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));
    settle_turn(&mut app).await;

    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}
