use super::support::*;
use crate::{
    app::{Panel, state::Entry},
    render,
};
use causa::kernel::Context;
use ratatui::{Terminal, backend::TestBackend};
use tars_core::{
    application::PreparedCreate,
    definition::{CredentialRef, Provider, SessionDefinition},
    format_timestamp_utc,
};

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn test_definition(model: &str) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: model.into(),
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
    }
}

#[tokio::test]
async fn test_be23_tui_sessions_ordering_open_first_then_newest() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    // Create 3 entries:
    // Entry 0: closed, updated_at_ms = 1_000
    let mut e0 = Entry::new("session-closed-old".into(), "model-0".into());
    e0.updated_at_ms = 1_000;
    e0.title = Some("Session Closed Old".into());

    // Entry 1: closed, updated_at_ms = 5_000 (newest closed)
    let mut e1 = Entry::new("session-closed-new".into(), "model-1".into());
    e1.updated_at_ms = 5_000;
    e1.title = Some("Session Closed New".into());

    // Entry 2: open session (session is Some), updated_at_ms = 2_000
    let managed = app
        .host
        .create(PreparedCreate::persistent(
            test_definition("model-open"),
            Context::new(),
        ))
        .await
        .unwrap();
    let open_id = managed.target().conversation_id().as_str().to_owned();
    let mut e2 = Entry::new(open_id, "model-open".into());
    e2.updated_at_ms = 2_000;
    e2.title = Some("Session Open Active".into());
    e2.session = Some(managed);

    app.entries = vec![e0, e1, e2];

    // Ordering should be:
    // Open session (index 2) FIRST
    // Then newest closed (index 1)
    // Then older closed (index 0)
    let matches = app.matching_sessions("");
    assert_eq!(matches, vec![2, 1, 0]);
}

#[test]
fn test_be23_tui_sessions_filtering_by_title_model_and_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    let mut e0 = Entry::new("uuid-alpha-1234".into(), "claude-model".into());
    e0.title = Some("Research project on AI".into());

    let mut e1 = Entry::new("uuid-beta-5678".into(), "gpt-special-edition".into());
    e1.title = Some("Bug fixing report".into());

    let mut e2 = Entry::new("uuid-gamma-9999".into(), "gemini-flash".into());
    e2.title = None; // No title

    app.entries = vec![e0, e1, e2];

    // Filter by title
    let matches_title = app.matching_sessions("research");
    assert_eq!(matches_title, vec![0]);

    // Filter by model
    let matches_model = app.matching_sessions("special");
    assert_eq!(matches_model, vec![1]);

    // Filter by id
    let matches_id = app.matching_sessions("gamma");
    assert_eq!(matches_id, vec![2]);
}

#[test]
fn test_be23_tui_sessions_rendering_shows_title_time_and_model_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    let mut e0 = Entry::new("session-with-title".into(), "custom-model-alpha".into());
    e0.title = Some("Alpha Task Title".into());
    e0.updated_at_ms = 1_700_000_000_000; // 2023-11-14 22:13:20 UTC

    let mut e1 = Entry::new("session-no-title".into(), "custom-model-beta".into());
    e1.title = None;
    e1.updated_at_ms = 1_700_000_000_000;

    app.entries = vec![e0, e1];

    app.panel = Some(Panel::Sessions {
        query: String::new(),
        selected: 0,
    });

    let rendered = screen(&app);
    let time_str = format_timestamp_utc(1_700_000_000_000);

    // Item 1 has title and time on line 1, model on line 2
    assert!(rendered.contains("Alpha Task Title"));
    assert!(rendered.contains(&time_str));
    assert!(rendered.contains("custom-model-alpha"));

    // Item 2 has no title: model and time on line 1, model on line 2
    assert!(rendered.contains("custom-model-beta"));
}

#[tokio::test]
async fn opening_the_picker_starts_on_the_current_session_row() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let mut older = Entry::new("session-older".into(), "model-older".into());
    older.updated_at_ms = 1_000;
    let mut newer = Entry::new("session-newer".into(), "model-newer".into());
    newer.updated_at_ms = 5_000;
    app.entries = vec![older, newer];
    app.selected = 0;

    app.event(key(crossterm::event::KeyCode::F(1)));
    for c in "sessions".chars() {
        app.event(key(crossterm::event::KeyCode::Char(c)));
    }
    app.event(key(crossterm::event::KeyCode::Enter));

    // The newer session is listed first, so the current one is the second row.
    assert_eq!(app.matching_sessions(""), vec![1, 0]);
    assert!(matches!(
        app.panel,
        Some(Panel::Sessions { selected: 1, .. })
    ));
}
