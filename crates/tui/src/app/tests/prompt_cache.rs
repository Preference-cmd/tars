use super::support::*;
use crate::{app::state::Entry, render};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use tars_core::config::{Config, DefinitionOverrides};

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

#[tokio::test]
async fn test_be20_tui_prompt_cache_resolution_and_new_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_default = Config::default();

    let def_on = cfg_default
        .definition(
            None,
            DefinitionOverrides {
                provider: Some(tars_core::definition::Provider::OpenAiChat),
                model: Some("fixture-model".into()),
                api_key_env: Some("PATH".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(def_on.prompt_cache);

    let def_off = cfg_default
        .definition(
            None,
            DefinitionOverrides {
                provider: Some(tars_core::definition::Provider::OpenAiChat),
                model: Some("fixture-model".into()),
                api_key_env: Some("PATH".into()),
                prompt_cache: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!def_off.prompt_cache);

    // `/new` inherits `prompt_cache: false` from the open session.
    let mut app = app(dir.path());
    create_with_definition(&mut app, def_off).await;
    assert!(!app.entries[0].prompt_cache());
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        a.entries.len() == 2 && a.entries[1].session.is_some()
    })
    .await;
    assert!(!app.entries[1].prompt_cache());
    assert!(
        !app.entries[1]
            .session
            .as_ref()
            .unwrap()
            .definition()
            .prompt_cache
    );
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
