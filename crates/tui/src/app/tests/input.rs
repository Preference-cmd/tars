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
    let values: Vec<_> = (0..rows.len()).map(|i| rows.row_text(i).unwrap()).collect();
    assert_eq!(values, ["中e\u{301}", "👩‍💻", "文", "abc"]);
    let rows = Rows::wrap(rows.transcript.clone(), 1);
    assert!((0..rows.len()).any(|i| rows.row_text(i).as_deref() == Some("👩‍💻")));
}
#[test]
fn paste_controls_keep_actions_explicit() {
    let mut editor = input::editor();
    input::paste(&mut editor, "a\x1b[31m\x13b\t\nc", true);
    assert_eq!(editor.lines().join("\n"), "a[31mb\t\nc");
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.event(Event::Paste("unsaved scratch".into()));
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

#[tokio::test]
async fn shared_toml_populates_tui_session_and_explicit_store_wins() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "version = 1\nstore = 'data'\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'anthropic'\nmodel = 'shared-model'\napi_key_env = 'SHARED_KEY'\ncapacity = 7\n").unwrap();
    let options =
        crate::options::parse(["--config".into(), path.to_string_lossy().into_owned()].into_iter())
            .unwrap()
            .unwrap();
    assert_eq!(options.store, dir.path().join("data"));
    let mut app = crate::app::App::new(options);
    let definition = app
        .options
        .preset
        .as_ref()
        .expect("shared TOML preset populated");
    assert_eq!(definition.model, "shared-model");
    assert_eq!(definition.retained_work_capacity, 7);
    assert_eq!(
        definition.provider,
        tars_core::definition::Provider::Anthropic
    );
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        a.entries.len() == 1 && a.entries[0].busy.is_none()
    })
    .await;
    assert_eq!(app.entries[0].model(), "shared-model");
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

#[tokio::test]
async fn workspace_session_uses_shared_resolver_and_preserves_profile_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("workspace")).unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path,"version = 1\nstore = 'data'\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'openai-chat'\nmodel = 'workspace-model'\napi_key_env = 'PATH'\nworkspace_root = 'workspace'\ntools = ['read_file','search_text']\n[profiles.local.tool_limits]\nmax_search_matches = 7\n").unwrap();
    let options =
        crate::options::parse(["--config".into(), path.to_string_lossy().into_owned()].into_iter())
            .unwrap()
            .unwrap();
    let config = options.config.clone();
    let mut app = crate::app::App::new(options);
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    let frozen = app.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(
        frozen.workspace_root,
        Some(dir.path().join("workspace").canonicalize().unwrap())
    );
    assert_eq!(frozen.tools.len(), 2);
    assert_eq!(frozen.tool_limits.max_search_matches, 7);
    assert!(
        config
            .definition(
                None,
                tars_core::config::DefinitionOverrides {
                    tools: Some(tars_core::config::parse_tools("").unwrap()),
                    ..Default::default()
                },
            )
            .unwrap()
            .tools
            .is_empty()
    );
    assert!(tars_core::config::parse_tools("shell").is_err());
}

#[test]
fn clearing_endpoint_and_workspace_overrides_works_via_definition_overrides() {
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
    for profile in [None, Some("local")] {
        let mut args = vec!["--config".to_owned(), path.to_string_lossy().into_owned()];
        if let Some(name) = profile {
            args.extend(["--profile".to_owned(), name.to_owned()]);
        }
        let options = crate::options::parse(args.into_iter()).unwrap().unwrap();
        let preset = options.preset.as_ref().unwrap();
        assert_eq!(preset.endpoint.as_deref(), Some(endpoint));
        assert_eq!(preset.workspace_root.as_ref(), Some(&workspace));
        assert_eq!(preset.tools.len(), 1);

        let mut without_endpoint = preset.clone();
        without_endpoint.endpoint = None;
        without_endpoint.validate().unwrap();
        assert_eq!(without_endpoint.workspace_root, Some(workspace.clone()));
        assert_eq!(without_endpoint.tools.len(), 1);

        let mut missing_ws = without_endpoint.clone();
        missing_ws.workspace_root = None;
        let error = missing_ws.validate().unwrap_err().to_string();
        assert!(
            error.contains("workspace root is required when tools are enabled"),
            "{error}"
        );

        let mut cleared = missing_ws;
        cleared.tools.clear();
        cleared.validate().unwrap();
        assert_eq!(cleared.endpoint, None);
        assert_eq!(cleared.workspace_root, None);
        assert!(cleared.tools.is_empty());
    }
}

#[tokio::test]
async fn wm11_tui_resolves_mcp_ids_through_shared_config() {
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
api_key_env = 'PATH'
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
    let config = options.config.clone();
    let profile = options.profile.clone();
    let mut app = crate::app::App::new(options);
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    let def = app.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(def.mcp_servers.len(), 1);
    assert_eq!(def.mcp_servers[0].id, "git");

    let selected = config
        .definition(
            profile.as_deref(),
            tars_core::config::DefinitionOverrides {
                mcp: Some(tars_core::config::parse_mcp("git,docs").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(selected.mcp_servers.len(), 2);
    assert_eq!(selected.mcp_servers[1].id, "docs");

    let cleared = config
        .definition(
            profile.as_deref(),
            tars_core::config::DefinitionOverrides {
                mcp: Some(tars_core::config::parse_mcp("").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(cleared.mcp_servers.is_empty());

    let absent_err = config
        .definition(
            profile.as_deref(),
            tars_core::config::DefinitionOverrides {
                mcp: Some(tars_core::config::parse_mcp("absent").unwrap()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(absent_err.contains("absent"));
    assert!(tars_core::config::parse_mcp("Git").is_err());

    let fresh = config
        .definition(
            None,
            tars_core::config::DefinitionOverrides {
                model: Some("fixture-model".into()),
                mcp: Some(tars_core::config::parse_mcp("git").unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(fresh.mcp_servers[0].id, "git");
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
