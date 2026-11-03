use super::support::app;
#[cfg(target_os = "macos")]
use super::support::{ctrl, key, until};
use crate::{input::Form, render};
#[cfg(target_os = "macos")]
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
#[cfg(target_os = "macos")]
use serde_json::json;
use std::sync::Arc;
use tars_core::config::Config;
use tars_core::definition::{
    CommandNetwork, CommandSettings, CredentialRef, Provider, SessionDefinition, WorkspaceTool,
};

#[cfg(target_os = "macos")]
use super::support::advance_past_approval_window;
#[cfg(target_os = "macos")]
use super::wire;

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 50)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn preset(command: CommandSettings) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(120),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(std::env::current_dir().unwrap()),
        tools: vec![WorkspaceTool::RunCommand],
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: Some(command),
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
    }
}

#[test]
fn new_session_form_shows_command_settings_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.form = Some(Form::new(
        Arc::new(Config::default()),
        None,
        Some(&preset(CommandSettings {
            network: CommandNetwork::Open,
            default_timeout_secs: 90,
            max_timeout_secs: 300,
        })),
    ));
    let view = screen(&app);
    assert!(
        view.contains("Command: network open · timeout 90s (max 300s) · preset"),
        "{view}"
    );
    // The summary is not a field: tab order still covers exactly the inputs.
    let form = app.form.as_ref().unwrap();
    assert_eq!(form.fields.len(), crate::input::FIELD_NAMES.len());
    assert!(Form::default().command_summary().is_none());
}

#[test]
fn command_summary_follows_the_tools_field_not_the_preset() {
    let mut definition = preset(CommandSettings {
        network: CommandNetwork::Open,
        default_timeout_secs: 90,
        max_timeout_secs: 300,
    });
    definition.tools = vec![WorkspaceTool::ReadFile];
    let mut form = Form::new(Arc::new(Config::default()), None, Some(&definition));
    assert!(form.command_summary().is_none());
    form.fields[6] = TextArea::from(["run_command"]);
    assert_eq!(
        form.command_summary().unwrap(),
        "Command: network none · timeout 120s (max 600s) · defaults"
    );
    form.fields[6] = TextArea::from(["read_file"]);
    assert!(form.command_summary().is_none());
    form.fields[6] = TextArea::from(["not_a_tool"]);
    assert!(form.command_summary().is_none());
}

#[cfg(target_os = "macos")]
async fn create_command_session(
    app: &mut crate::app::App,
    endpoint: &str,
    workspace: &std::path::Path,
    mode: &str,
) {
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "PATH",
            "120",
            &workspace.display().to_string(),
            "run_command",
            mode,
            "",
            "on",
        ]
        .into_iter()
        .map(|value| TextArea::from([value]))
        .collect(),
        selected: 1,
        error: String::new(),
        limits: (256, 10, 64),
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(app, |a| a.entries[a.selected].session.is_some()).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_panel_names_the_backend_and_every_capability() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let fixture = wire::Fixture::with_responses("openai-chat", vec![]);
    let mut app = app(dir.path());
    create_command_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.event(key(KeyCode::Char('/')));
    for character in "status".chars() {
        app.event(key(KeyCode::Char(character)));
    }
    app.event(key(KeyCode::Enter));
    let view = screen(&app);
    assert!(
        view.contains(
            "Commands: seatbelt: file read full; file write full; network full; \
             signal isolation full"
        ),
        "{view}"
    );
    assert!(view.contains("lifecycle partial"), "{view}");
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_panel_shows_the_command_workdir_timeout_and_network_then_denies() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let responses = vec![
        wire::tool(
            "openai-chat",
            "run_command",
            json!({"command": "printf denied > denied.txt", "workdir": "sub", "timeout_secs": 30}),
            "call_command_deny",
        ),
        wire::answer_with_text("openai-chat", "command declined"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);
    let mut app = app(dir.path());
    create_command_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("run the denied command");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    let view = screen(&app);
    assert!(
        view.contains("run_command wants to run a command (permission mode: ask)"),
        "{view}"
    );
    assert!(view.contains("Working directory: sub"), "{view}");
    assert!(view.contains("Timeout: 30s"), "{view}");
    assert!(view.contains("Network: none"), "{view}");
    assert!(
        view.contains(
            "Workspace: writable; .git hooks and config read-only; credentials unreadable"
        ),
        "{view}"
    );
    assert!(view.contains("$ printf denied > denied.txt"), "{view}");

    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert!(!workspace.join("sub").join("denied.txt").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_panel_shows_outside_sandbox_warning_and_justification() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let responses = vec![
        wire::tool(
            "openai-chat",
            "run_command",
            json!({
                "command": "printf denied > outside.txt",
                "outside_sandbox": true,
                "justification": "needs the user cargo cache"
            }),
            "call_command_outside_deny",
        ),
        wire::answer_with_text("openai-chat", "command declined"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);
    let mut app = app(dir.path());
    create_command_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("run the outside command");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    let view = screen(&app);
    assert!(
        view.contains("run_command wants to run a command outside the sandbox"),
        "{view}"
    );
    assert!(view.contains("NOT in the sandbox"), "{view}");
    assert!(
        view.contains("Justification: needs the user cargo cache"),
        "{view}"
    );
    assert!(!view.contains("Network: none"), "{view}");
    assert!(view.contains("$ printf denied > outside.txt"), "{view}");

    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert!(!workspace.join("outside.txt").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_panel_scrolls_a_multiline_outside_justification() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let justification = (0..60)
        .map(|index| format!("reason {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let responses = vec![
        wire::tool(
            "openai-chat",
            "run_command",
            json!({
                "command": "printf denied > outside.txt",
                "outside_sandbox": true,
                "justification": justification
            }),
            "call_command_outside_scroll",
        ),
        wire::answer_with_text("openai-chat", "command declined"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);
    let mut app = app(dir.path());
    create_command_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("run the outside command");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    for _ in 0..80 {
        app.event(key(KeyCode::PageDown));
    }
    let view = screen(&app);
    assert!(view.contains("$ printf denied > outside.txt"), "{view}");

    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert!(!workspace.join("outside.txt").exists());
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_panel_allows_the_exact_command_with_y() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let responses = vec![
        wire::tool(
            "openai-chat",
            "run_command",
            json!({"command": "printf allowed > allowed.txt", "timeout_secs": 20}),
            "call_command_allow",
        ),
        wire::answer_with_text("openai-chat", "command ran"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);
    let mut app = app(dir.path());
    create_command_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("run the allowed command");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    let view = screen(&app);
    assert!(view.contains("$ printf allowed > allowed.txt"), "{view}");
    assert!(view.contains("Timeout: 20s"), "{view}");

    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("allowed.txt")).unwrap(),
        "allowed"
    );
}

#[test]
fn the_startup_clause_is_shown_only_when_the_sandbox_is_unavailable() {
    use crate::app::jobs::{COMMAND_UNAVAILABLE_NOTICE, command_sandbox_notice, opened_notice};

    assert_eq!(command_sandbox_notice(true), None);
    assert_eq!(
        command_sandbox_notice(false),
        Some(COMMAND_UNAVAILABLE_NOTICE)
    );
    assert!(COMMAND_UNAVAILABLE_NOTICE.contains("command sandbox unavailable"));
    let notice = opened_notice(&[], false, command_sandbox_notice(false));
    assert!(
        notice.starts_with("Opened; command sandbox unavailable (see /status);"),
        "{notice}"
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_status_line_renders_the_core_sandbox_status_verbatim() {
    use crate::app::commands::command_status_line;

    let status = tars_core::sandbox::SandboxStatus::Unavailable(
        tars_core::sandbox::Sandbox::open().unwrap_err(),
    );
    let line = command_status_line(&status);
    assert_eq!(line, format!("Commands: {status}"));
    assert!(line.starts_with("Commands: "), "{line}");
    assert!(line.contains("no sandbox backend"), "{line}");
}

/// End-to-end display of a probe failure in the running TUI app: with `HOME`
/// unset the Seatbelt probe fails at session creation, so opening the session
/// must show the fixed startup clause and `/status` must show the full
/// unavailable status. Ignored by default because CI has `HOME` set; run it
/// with `env -u HOME CARGO_HOME="$HOME/.cargo" cargo test -p tars-tui -- --ignored
/// unavailable_command_sandbox`.
#[cfg(target_os = "macos")]
#[ignore = "run with HOME unset so the Seatbelt probe fails and the session reports an unavailable sandbox"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_command_sandbox_is_reported_at_open_and_in_status() {
    assert!(
        std::env::var_os("HOME").is_none(),
        "run this probe with HOME unset"
    );
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    // Creation sends no model request, so the endpoint is never contacted.
    let mut app = app(dir.path());
    create_command_session(&mut app, "http://127.0.0.1:9/fixture", &workspace, "ask").await;

    let status = app.entries[0]
        .session
        .as_ref()
        .expect("the session must open with the command tool disabled")
        .command_status()
        .expect("the bound command tool reports its probe result");
    assert!(!status.is_available(), "{status}");
    assert!(
        status.to_string().contains("sandbox is unavailable"),
        "{status}"
    );
    assert!(status.to_string().contains("HOME is not set"), "{status}");
    assert!(
        app.entries[0]
            .notice
            .contains("command sandbox unavailable (see /status)"),
        "{}",
        app.entries[0].notice
    );

    app.event(key(KeyCode::Char('/')));
    for character in "status".chars() {
        app.event(key(KeyCode::Char(character)));
    }
    app.event(key(KeyCode::Enter));
    let view = screen(&app);
    assert!(
        view.contains("Commands: seatbelt sandbox is unavailable: HOME is not set"),
        "{view}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn us10_status_panel_contains_usage_section_equal_to_session_usage_lines() {
    use super::support::{Server, app, create, ctrl, key, until};
    use crossterm::event::{Event, KeyCode};
    use tars_core::application::SaveStatus;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("run prompt for us10".into()));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app, |app| app.entries[0].execution == "Completed").await;
    until(&mut app, |app| {
        matches!(
            app.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { .. }
        )
    })
    .await;

    // Open /status panel
    app.event(key(KeyCode::F(1)));
    for character in "status".chars() {
        app.event(key(KeyCode::Char(character)));
    }
    app.event(key(KeyCode::Enter));

    let lines = match &app.panel {
        Some(crate::app::Panel::Info { lines, .. }) => lines.clone(),
        other => panic!("expected Info panel, present: {}", other.is_some()),
    };

    let session = app.entries[0].session.as_ref().unwrap();
    let usage = session.session_usage().expect("session usage present");
    let expected = tars_core::usage::session_usage_lines(&usage);
    assert!(!expected.is_empty());

    let start_idx = lines
        .iter()
        .position(|l| l == &expected[0])
        .unwrap_or_else(|| {
            panic!(
                "panel lines do not contain initial usage line '{}':\n{lines:#?}",
                expected[0]
            )
        });
    assert_eq!(
        &lines[start_idx..start_idx + expected.len()],
        &expected[..],
        "usage section in /status does not match session_usage_lines exactly"
    );
}
