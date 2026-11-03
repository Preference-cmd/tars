use super::support::*;
use crate::{app::state::Entry, input, render};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;

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

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

#[test]
fn test_be20_tui_form_prompt_cache_validation_and_resolution() {
    let mut form = input::Form::default();
    input::paste(&mut form.fields[1], "fixture-model", false);

    // Default is "on"
    assert_eq!(form.fields[9].lines().join(""), "on");
    let def_on = form.definition().unwrap();
    assert!(def_on.prompt_cache);

    // Change to "off"
    form.fields[9] = TextArea::from(["off"]);
    let def_off = form.definition().unwrap();
    assert!(!def_off.prompt_cache);

    // Invalid value returns clear error message
    form.fields[9] = TextArea::from(["invalid"]);
    let err = form.definition().unwrap_err();
    assert_eq!(err, "Prompt cache: on | off");
}

#[test]
fn test_be20_tui_status_displays_prompt_cache_setting() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    let mut entry = Entry::new("status-test".into(), "fixture-model".into());
    entry.prompt_cache = true;
    app.entries.push(entry);

    // Open /status panel
    command(&mut app, "status");

    let rendered = screen(&app);
    assert!(rendered.contains("Prompt cache: on"), "{rendered}");

    // Test with prompt_cache off
    app.event(key(KeyCode::Esc));
    app.entries[0].prompt_cache = false;
    command(&mut app, "status");

    let rendered_off = screen(&app);
    assert!(rendered_off.contains("Prompt cache: off"), "{rendered_off}");
}
