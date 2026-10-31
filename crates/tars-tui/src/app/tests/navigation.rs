use super::support::*;
use crate::{
    app::{Panel, state::Entry},
    render,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

#[tokio::test]
async fn menus_keep_drafts_and_pastes_cannot_execute_commands() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.entries
        .push(Entry::new("first".into(), "First model".into()));
    app.entries
        .push(Entry::new("second".into(), "Second model".into()));
    app.event(Event::Paste("/quit".into()));
    assert!(app.panel.is_none());
    assert!(!app.quitting);
    command(&mut app, "sessions");
    assert!(matches!(app.panel, Some(Panel::Sessions { .. })));
    app.event(Event::Paste("Second".into()));
    assert_eq!(app.selected, 0);
    app.event(key(KeyCode::Enter));
    assert_eq!(app.selected, 1);
    assert_eq!(app.entries[0].editor.lines(), &["/quit"]);
    app.event(key(KeyCode::F(1)));
    app.event(Event::Paste("quit\r\n".into()));
    assert!(!app.quitting);
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    assert!(app.entries[0].work.is_none());
    // Unknown commands remain in the picker and never enter the model prompt.
    command(&mut app, "not-a-command");
    assert!(matches!(app.panel, Some(Panel::Commands { .. })));
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::Char('/')));
    assert!(matches!(app.panel, Some(Panel::Commands { .. })));
    app.event(key(KeyCode::Esc));
    command(&mut app, "discard");
    assert!(app.confirmation.is_some());
    app.event(key(KeyCode::Char('n')));
    assert!(app.confirmation.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_sends_once_ctrl_j_edits_and_menu_escape_does_not_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("First line".into()));
    app.event(ctrl('j'));
    app.event(Event::Paste("Second line".into()));
    assert_eq!(
        app.entries[0].editor.lines().join("\n"),
        "First line\nSecond line"
    );
    app.event(key(KeyCode::Enter));
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    )));
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::NONE,
        KeyEventKind::Release,
    )));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    app.event(key(KeyCode::F(1)));
    app.event(key(KeyCode::Esc));
    assert_eq!(app.entries[0].execution, "Running");
    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].execution == "Completed").await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    command(&mut app, "quit");
    until(&mut app, |a| a.done).await;
}

#[tokio::test]
async fn compact_layout_keeps_menu_and_confirmation_controls_visible() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    for (width, height) in [(120, 40), (80, 24), (50, 18), (30, 12)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        app.event(key(KeyCode::F(1)));
        app.event(Event::Paste("new".into()));
        terminal.draw(|f| render::draw(f, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("/new"), "{width}x{height}: {screen}");
        app.event(key(KeyCode::Enter));
        terminal.draw(|f| render::draw(f, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("Model"), "{width}x{height}: {screen}");
        app.event(ctrl('q'));
        terminal.draw(|f| render::draw(f, &app)).unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("y / n"), "{width}x{height}: {screen}");
        app.event(key(KeyCode::Char('n')));
        app.event(key(KeyCode::Esc));
    }
}

#[test]
fn long_draft_leaves_input_hint_and_status_visible_at_minimum_size() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let mut entry = Entry::new("draft".into(), "model".into());
    entry
        .editor
        .insert_str("one\ntwo\nthree\nfour\nfive\nsix\nvisible cursor line");
    entry.execution = "Completed".into();
    entry.status = "saved r2".into();
    app.entries.push(entry);
    let mut terminal = Terminal::new(TestBackend::new(30, 12)).unwrap();
    terminal.draw(|f| render::draw(f, &app)).unwrap();
    let text = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert_eq!(render::conversation_height(12, 30, &app), 1);
    assert!(text.contains("visible cursor line"), "{text}");
    assert!(text.contains("Enter send"), "{text}");
    assert!(text.contains("saved r2"), "{text}");
}

#[test]
fn f4_switches_only_on_press() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    for id in ["first", "second", "third"] {
        app.entries.push(Entry::new(id.into(), "model".into()));
    }
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        app.event(Event::Key(KeyEvent::new_with_kind(
            KeyCode::F(4),
            KeyModifiers::NONE,
            kind,
        )));
        assert_eq!(app.selected, 0, "{kind:?} must not switch sessions");
    }
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::F(4),
        KeyModifiers::NONE,
        KeyEventKind::Press,
    )));
    assert_eq!(app.selected, 1);
}
