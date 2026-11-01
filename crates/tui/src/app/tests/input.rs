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
    let rows = Rows::wrap(
        Arc::new(Transcript {
            text: text.into(),
            ..Transcript::default()
        }),
        3,
    );
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
        tars_core::definition::Provider::Anthropic
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

#[test]
fn workspace_form_uses_shared_resolver_and_preserves_profile_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path,"version = 1\nstore = 'data'\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'openai-chat'\nmodel = 'workspace-model'\nworkspace_root = 'workspace'\ntools = ['read_file','search_text']\n[profiles.local.tool_limits]\nmax_search_matches = 7\n").unwrap();
    let options =
        crate::options::parse(["--config".into(), path.to_string_lossy().into_owned()].into_iter())
            .unwrap()
            .unwrap();
    let mut form = input::Form::new(
        options.config.clone(),
        options.profile.clone(),
        options.preset.as_ref(),
    );
    assert_eq!(form.fields.len(), 9);
    let frozen = form.definition().unwrap();
    assert_eq!(
        frozen.workspace_root,
        Some(dir.path().join("workspace").canonicalize().unwrap())
    );
    assert_eq!(frozen.tools.len(), 2);
    assert_eq!(frozen.tool_limits.max_search_matches, 7);
    form.fields[6] = ratatui_textarea::TextArea::from([""]);
    assert!(form.definition().unwrap().tools.is_empty());
    form.fields[6] = ratatui_textarea::TextArea::from(["shell"]);
    assert!(form.definition().is_err());
}

#[test]
fn clearing_visible_endpoint_and_workspace_fields_overrides_the_profile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "version = 1\nstore = 'data'\ndefault_profile = 'local'\n\
         [profiles.local]\nprovider = 'openai-chat'\nmodel = 'clear-model'\n\
         endpoint = 'http://127.0.0.1:9/v1/chat/completions'\n\
         workspace_root = 'workspace'\ntools = ['read_file']\n",
    )
    .unwrap();
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";
    let workspace = dir.path().join("workspace").canonicalize().unwrap();
    // Both resolution paths are covered: the default profile with no explicit
    // name, and the same profile selected by name.
    for profile in [None, Some("local")] {
        let mut args = vec!["--config".to_owned(), path.to_string_lossy().into_owned()];
        if let Some(name) = profile {
            args.extend(["--profile".to_owned(), name.to_owned()]);
        }
        let options = crate::options::parse(args.into_iter()).unwrap().unwrap();
        let mut app = crate::app::App::new(options);
        app.event(key(KeyCode::F(2)));
        let form = app.form.as_mut().unwrap();
        assert_eq!(form.fields[2].lines().join(""), endpoint);
        assert_eq!(
            form.fields[5].lines().join(""),
            workspace.display().to_string()
        );
        assert_eq!(form.fields[6].lines().join(""), "read_file");

        // Clearing the endpoint must not restore the profile's endpoint.
        form.fields[2] = ratatui_textarea::TextArea::from([""]);
        let without_endpoint = form.definition().unwrap();
        assert_eq!(without_endpoint.endpoint, None);
        assert_eq!(without_endpoint.workspace_root, Some(workspace.clone()));
        assert_eq!(without_endpoint.tools.len(), 1);

        // Clearing the workspace while tools stay selected is refused.
        form.fields[5] = ratatui_textarea::TextArea::from([""]);
        let error = form.definition().unwrap_err();
        assert!(
            error.contains("workspace root is required when tools are enabled"),
            "{error}"
        );

        // Clearing both fields and the tool selection yields an explicit no-root,
        // no-endpoint definition instead of the profile's values.
        form.fields[6] = ratatui_textarea::TextArea::from([""]);
        let cleared = form.definition().unwrap();
        assert_eq!(cleared.endpoint, None);
        assert_eq!(cleared.workspace_root, None);
        assert!(cleared.tools.is_empty());
    }
}

#[test]
fn wm11_tui_form_resolves_mcp_ids_through_shared_config() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(
        &toml_path,
        r#"version = 1
store = 'data'
default_profile = 'local'
[profiles.local]
provider = 'openai-chat'
model = 'mcp-form-model'
mcp = ['git']
"#,
    )
    .unwrap();
    std::fs::write(
        &json_path,
        r#"{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "args": ["mcp-server-git"]
    },
    "docs": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_MCP_TOKEN}" }
    }
  }
}"#,
    )
    .unwrap();
    let options = crate::options::parse(
        ["--config".into(), toml_path.to_string_lossy().into_owned()].into_iter(),
    )
    .unwrap()
    .unwrap();
    let mut form = input::Form::new(
        options.config.clone(),
        options.profile.clone(),
        options.preset.as_ref(),
    );
    assert_eq!(form.fields.len(), 9);
    assert_eq!(
        input::FIELD_NAMES[8],
        "MCP servers (comma-separated ids from mcp.json)"
    );
    assert_eq!(form.fields[8].lines().join(""), "git");
    assert_eq!(form.definition().unwrap().mcp_servers.len(), 1);
    assert_eq!(form.definition().unwrap().mcp_servers[0].id, "git");

    form.fields[8] = ratatui_textarea::TextArea::from(["git,docs"]);
    let selected = form.definition().unwrap();
    assert_eq!(selected.mcp_servers.len(), 2);
    assert_eq!(selected.mcp_servers[1].id, "docs");

    form.fields[8] = ratatui_textarea::TextArea::from([""]);
    assert!(form.definition().unwrap().mcp_servers.is_empty());

    form.fields[8] = ratatui_textarea::TextArea::from(["absent"]);
    assert!(form.definition().unwrap_err().contains("absent"));
    form.fields[8] = ratatui_textarea::TextArea::from(["Git"]);
    assert!(form.definition().is_err());

    // Without a preset the same configuration still resolves an entered id.
    let mut fresh = input::Form::new(options.config.clone(), options.profile.clone(), None);
    fresh.fields[1] = ratatui_textarea::TextArea::from(["fixture-model"]);
    fresh.fields[8] = ratatui_textarea::TextArea::from(["git"]);
    assert_eq!(fresh.definition().unwrap().mcp_servers[0].id, "git");
}

#[test]
fn wm11_tui_app_notice_displays_mcp_config_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("config.toml");
    let json_path = dir.path().join("mcp.json");
    std::fs::write(&toml_path, "version = 1\nstore = 'data'\n").unwrap();
    std::fs::write(
        &json_path,
        r#"{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "disabled": true
    }
  }
}"#,
    )
    .unwrap();
    let options = crate::options::parse(
        ["--config".into(), toml_path.to_string_lossy().into_owned()].into_iter(),
    )
    .unwrap()
    .unwrap();
    let app = crate::app::App::new(options);
    assert!(app.notice.contains("git") && app.notice.contains("disabled"));
}
