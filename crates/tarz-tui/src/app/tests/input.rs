use super::support::*;
use crate::{
    app::state::Entry,
    input, render,
    transcript::{Rows, Transcript},
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;

#[test]
fn unicode_paste_newlines_editing_and_release_keys_do_not_submit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.entries.push(Entry::new("draft".into(), "model".into()));
    let text = "中文\r\n👩‍💻 e\u{301}\nlast";
    app.event(Event::Paste(text.into()));
    assert_eq!(
        app.entries[0].editor.lines().join("\n"),
        text.replace("\r\n", "\n")
    );
    assert!(app.entries[0].work.is_none());
    app.event(ctrl('j'));
    app.event(key(KeyCode::Char('好')));
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('好'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    )));
    assert_eq!(app.entries[0].editor.lines().last().unwrap(), "好");
    app.event(key(KeyCode::Backspace));
    assert_eq!(app.entries[0].editor.lines().last().unwrap(), "");
    app.event(ctrl('z'));
    assert_eq!(app.entries[0].editor.lines().last().unwrap(), "好");
    app.event(ctrl('y'));
    assert_eq!(app.entries[0].editor.lines().last().unwrap(), "");
    app.event(ctrl('z'));
    app.event(ctrl('q'));
    assert!(app.confirmation.is_some());
    app.event(key(KeyCode::Char('n')));
    assert!(!app.quitting);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|f| render::draw(f, &app)).unwrap();
}
#[test]
fn grapheme_wrapping_never_splits_combining_marks_or_zwj_sequences() {
    let text = "中e\u{301}👩‍💻文\nabc";
    let rows = Rows::wrap(Arc::new(Transcript { text: text.into() }), 3);
    let values: Vec<_> = rows
        .ranges
        .iter()
        .map(|r| &rows.transcript.text[r.clone()])
        .collect();
    assert_eq!(values, ["中e\u{301}", "👩‍💻", "文", "abc"]);
    let rows = Rows::wrap(rows.transcript.clone(), 1);
    assert!(
        rows.ranges
            .iter()
            .any(|r| &rows.transcript.text[r.clone()] == "👩‍💻")
    );
}
#[test]
fn paste_controls_and_form_validation_keep_actions_explicit() {
    let mut editor = input::editor();
    input::paste(&mut editor, "a\x1b[31m\x13b\t\nc", true);
    assert_eq!(editor.lines().join("\n"), "a[31mb\t\nc");
    let mut form = input::Form::default();
    assert!(form.definition().is_err());
    input::paste(&mut form.fields[1], "my-model\r\n", false);
    assert_eq!(form.definition().unwrap().model, "my-model");
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.form = Some(form);
    app.event(ctrl('q'));
    assert!(app.confirmation.is_some());
}
#[test]
fn textarea_grapheme_editing_probe() {
    let mut editor = input::editor();
    editor.insert_str("e\u{301}👩‍💻");
    input::edit(
        &mut editor,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(editor.lines(), &["e\u{301}".to_string()]);
    input::edit(
        &mut editor,
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
    );
    assert_eq!(editor.lines(), &[String::new()]);
}

#[test]
fn shared_toml_populates_tui_form_and_explicit_store_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "version = 1\nstore = 'data'\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'anthropic'\nmodel = 'shared-model'\napi_key_env = 'SHARED_KEY'\ncapacity = 7\n").unwrap();
    let options =
        crate::options::parse(["--config".into(), path.to_string_lossy().into_owned()].into_iter())
            .unwrap()
            .unwrap();
    assert_eq!(options.store, dir.path().join("data"));
    let mut app = crate::app::App::new(options);
    app.event(key(KeyCode::F(2)));
    let definition = app.form.as_ref().unwrap().definition().unwrap();
    assert_eq!(definition.model, "shared-model");
    assert_eq!(definition.retained_work_capacity, 7);
    assert_eq!(
        definition.provider,
        tarz_core::definition::Provider::Anthropic
    );
    let explicit = dir.path().join("override");
    let options = crate::options::parse(
        [
            "--config".into(),
            path.to_string_lossy().into_owned(),
            "--store".into(),
            explicit.to_string_lossy().into_owned(),
        ]
        .into_iter(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(options.store, explicit);
}
