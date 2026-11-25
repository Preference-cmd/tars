use super::support::{Server, app, create, ctrl, key, until};
use crate::{
    app::{App, COMMANDS, Clock, Panel, matching_commands},
    input::{Form, OnboardingForm},
    options::{self, Options},
    render,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tars_core::{
    application::{ClosePolicy, PreparedCreate},
    config::{Config, ConfigSource, default_store_dir, default_tools_for_current_platform},
    definition::Provider,
};

fn screen_sized(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn screen(app: &App) -> String {
    screen_sized(app, 100, 40)
}

fn onboard_app(dir: &Path, config_path: PathBuf) -> App {
    let mut app = App::new(Options {
        store: dir.join("store"),
        recovery_dir: dir.to_path_buf(),
        config: Arc::new(Config::default()),
        profile: None,
        preset: None,
        config_path: None,
        code_execution_enabled: None,
        no_config: false,
        auto_onboard: true,
        onboarding_config_path: Some(config_path),
    });
    app.clock = Clock::manual(std::time::Instant::now());
    app
}

fn run_slash_command(app: &mut App, name: &str) {
    app.event(key(KeyCode::F(1)));
    for ch in name.chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
}

fn type_text(app: &mut App, text: &str) {
    app.event(Event::Paste(text.into()));
}

#[test]
fn ux04_zero_config_store_resolution_and_no_config_isolation() {
    // --no-config without --store must still be refused.
    let err = options::parse(["--no-config"].into_iter().map(str::to_owned))
        .err()
        .expect("--no-config without --store must fail");
    assert!(
        err.contains("--store or a configured store directory is required"),
        "{err}"
    );

    // --no-config with --store works and disables auto_onboard.
    let dir = tempfile::tempdir().unwrap();
    let explicit_store = dir.path().join("isolated");
    let opts = options::parse(
        [
            "--no-config",
            "--store",
            explicit_store.to_str().unwrap(),
            "--recovery-dir",
            dir.path().to_str().unwrap(),
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap()
    .unwrap();
    assert!(opts.no_config);
    assert!(!opts.auto_onboard);
    assert_eq!(opts.store, explicit_store);

    // When config is enabled and has no explicit `store`, falls back to default_store_dir().
    if let Some(expected_default) = default_store_dir() {
        let cfg_without_store = dir.path().join("no-store.toml");
        std::fs::write(&cfg_without_store, "version = 1\n").unwrap();
        let opts = options::parse(
            ["--config", cfg_without_store.to_str().unwrap()]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert!(!opts.no_config);
        assert!(!opts.auto_onboard);
        assert_eq!(opts.store, expected_default);
    }

    // --help and -V/--version return Ok(None) cleanly.
    assert!(
        options::parse(["--help"].into_iter().map(str::to_owned))
            .unwrap()
            .is_none()
    );
    assert!(
        options::parse(["-V"].into_iter().map(str::to_owned))
            .unwrap()
            .is_none()
    );
    assert!(
        options::parse(["--version"].into_iter().map(str::to_owned))
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux05_auto_onboard_opens_on_empty_store_and_esc_skips_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("nested/config.toml");
    let mut app = onboard_app(dir.path(), cfg_path.clone());

    app.list();
    until(&mut app, |a| !a.listing).await;
    assert!(app.onboarding.is_some());

    // Normal and compact screen rendering both show onboarding cleanly.
    let wide = screen_sized(&app, 100, 40);
    assert!(wide.contains("Welcome to TARS · First-run setup"), "{wide}");
    assert!(wide.contains("Provider"), "{wide}");
    assert!(wide.contains("Model"), "{wide}");
    assert!(wide.contains("API key env var"), "{wide}");
    assert!(wide.contains("Workspace"), "{wide}");
    assert!(
        wide.contains("Tab/↑↓ field · ←→ provider · Enter/Ctrl-S save & start · Esc skip"),
        "{wide}"
    );

    let compact = screen_sized(&app, 40, 14);
    assert!(
        compact.contains("Welcome to TARS · First-run setup"),
        "{compact}"
    );
    assert!(compact.contains("2 / 6"), "{compact}");

    // Esc skips without writing config.toml and marks onboarding dismissed.
    app.event(key(KeyCode::Esc));
    assert!(app.onboarding.is_none());
    assert!(app.onboarding_dismissed);
    assert!(
        app.notice
            .contains("Setup skipped · press F2 or /new to create a session"),
        "{}",
        app.notice
    );
    assert!(!cfg_path.exists());

    // Subsequent list() must not reopen onboarding once dismissed.
    app.list();
    until(&mut app, |a| !a.listing).await;
    assert!(app.onboarding.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux05_auto_onboard_skipped_when_store_already_has_session() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let mut app = onboard_app(dir.path(), cfg_path);
    let prepared =
        PreparedCreate::prepare_persistent(super::support::history_definition(None), None).unwrap();
    let session = app.host.create(prepared).await.unwrap();
    let target = session.target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();

    app.list();
    until(&mut app, |a| !a.listing).await;
    assert!(!app.entries.is_empty());
    assert!(app.onboarding.is_none());
}

#[test]
fn ux05_provider_cycling_and_live_env_badge_never_leaks_secret_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut form = OnboardingForm::new(Some(dir.path().to_path_buf()));
    assert_eq!(form.selected, 1);
    assert_eq!(form.provider(), Provider::OpenAiResponses);
    assert_eq!(form.fields[2].lines().join(""), "OPENAI_API_KEY");
    assert_eq!(form.fields[4].lines().join(""), "");
    assert_eq!(
        form.fields[5].lines().join(""),
        dir.path().display().to_string()
    );

    // Move Up to field 0 (Provider) and cycle Left to Anthropic.
    form.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(form.selected, 0);
    form.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(form.provider(), Provider::Anthropic);
    assert_eq!(form.fields[2].lines().join(""), "ANTHROPIC_API_KEY");
    assert_eq!(form.fields[4].lines().join(""), "8192");

    // Cycle Right back to OpenAiResponses clears untouched max_tokens.
    form.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(form.provider(), Provider::OpenAiResponses);
    assert_eq!(form.fields[2].lines().join(""), "OPENAI_API_KEY");
    assert_eq!(form.fields[4].lines().join(""), "");

    // Cycle Right to OpenAiChat, then Anthropic, then customize max_tokens.
    form.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(form.provider(), Provider::OpenAiChat);
    assert_eq!(form.fields[2].lines().join(""), "OPENAI_API_KEY");
    form.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(form.provider(), Provider::Anthropic);
    assert_eq!(form.fields[4].lines().join(""), "8192");

    // Edit Max tokens (field 4) so cycling no longer overwrites it.
    for _ in 0..4 {
        form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    }
    assert_eq!(form.selected, 4);
    form.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    form.key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE));
    assert!(form.max_tokens_touched);
    // Shift+Tab (both BackTab and Tab+SHIFT) moves focus backward.
    form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    assert_eq!(form.selected, 3);
    form.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(form.selected, 2);
    form.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    assert_eq!(form.selected, 1);
    form.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(form.selected, 0);
    form.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(form.provider(), Provider::OpenAiResponses);
    assert_eq!(form.fields[4].lines().join(""), "8190");

    // Custom env var name is preserved across cycling, and badge reflects [set]/[not set] without echoing values.
    let mut app = onboard_app(dir.path(), dir.path().join("config.toml"));
    form.fields[2] = TextArea::from(["PATH"]);
    app.onboarding = Some(form);
    let view_set = screen(&app);
    assert!(view_set.contains("API key env var [set]"), "{view_set}");
    if let Ok(path_val) = std::env::var("PATH")
        && path_val.len() > 8
    {
        assert!(!view_set.contains(&path_val), "must never echo env values");
    }

    let form = app.onboarding.as_mut().unwrap();
    form.selected = 0;
    form.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(form.fields[2].lines().join(""), "PATH");
    form.fields[2] = TextArea::from(["TARS_DEFINITELY_UNSET_ENV_VAR_TEST"]);
    let view_unset = screen(&app);
    assert!(
        view_unset.contains("API key env var [not set]"),
        "{view_unset}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux05_validation_before_write_and_create_new_atomicity() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();

    let mut app = onboard_app(dir.path(), cfg_path.clone());
    app.list();
    until(&mut app, |a| a.onboarding.is_some()).await;

    // 1. Secret-like api_key_env is rejected without echoing the value and without writing config.toml.
    type_text(&mut app, "gpt-5");
    app.event(key(KeyCode::Tab));
    let secret = "sk-live-secret-do-not-echo";
    app.onboarding.as_mut().unwrap().fields[2] = TextArea::from([secret]);
    app.event(key(KeyCode::Enter));
    let onboarding = app.onboarding.as_ref().unwrap();
    assert!(!onboarding.error.is_empty());
    assert!(!onboarding.error.contains(secret), "{}", onboarding.error);
    assert!(!cfg_path.exists());

    // 2. Invalid workspace path fails BEFORE writing config.toml.
    let onboarding = app.onboarding.as_mut().unwrap();
    onboarding.fields[2] = TextArea::from(["PATH"]);
    onboarding.fields[5] = TextArea::from([dir.path().join("missing-dir").to_str().unwrap()]);
    app.event(key(KeyCode::Enter));
    let onboarding = app.onboarding.as_ref().unwrap();
    assert!(
        onboarding.error.contains("Workspace path"),
        "{}",
        onboarding.error
    );
    assert!(
        !cfg_path.exists(),
        "invalid workspace must not leave a written config.toml"
    );

    // 3. Existing config.toml is never overwritten (AlreadyExists).
    std::fs::write(&cfg_path, "# existing user config\n").unwrap();
    app.onboarding.as_mut().unwrap().fields[5] = TextArea::from([ws.to_str().unwrap()]);
    app.event(ctrl('s'));
    let onboarding = app.onboarding.as_ref().unwrap();
    assert!(
        onboarding.error.contains("already exists"),
        "{}",
        onboarding.error
    );
    assert_eq!(
        std::fs::read_to_string(&cfg_path).unwrap(),
        "# existing user config\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux05_submit_writes_config_updates_host_and_opens_session_with_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("tars_home/config.toml");
    let ws = dir.path().join("workspace");
    std::fs::create_dir(&ws).unwrap();
    let server = Server::with_streaming_rounds(vec![vec![
        r#"data: {"id":"1","choices":[{"index":0,"delta":{"content":"Ready!"},"finish_reason":null}]}"#.to_owned() + "\n\n",
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
    ]])
    .await;

    let mut app = onboard_app(dir.path(), cfg_path.clone());
    app.list();
    until(&mut app, |a| a.onboarding.is_some()).await;

    // Cycle to openai-chat so it matches the local Chat Completions fixture server.
    app.event(key(KeyCode::Up));
    app.event(key(KeyCode::Right));
    assert_eq!(
        app.onboarding.as_ref().unwrap().provider(),
        Provider::OpenAiChat
    );
    app.event(key(KeyCode::Tab));
    type_text(&mut app, "fixture-model");
    app.onboarding.as_mut().unwrap().fields[2] = TextArea::from(["PATH"]);
    app.onboarding.as_mut().unwrap().fields[3] = TextArea::from([server.endpoint.as_str()]);
    app.onboarding.as_mut().unwrap().fields[5] = TextArea::from([ws.to_str().unwrap()]);

    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.onboarding.is_none() && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;

    assert!(cfg_path.is_file());
    let loaded = Config::load(ConfigSource::File(cfg_path.clone())).unwrap();
    let preset = loaded.preset(None).unwrap().unwrap();
    assert_eq!(preset.provider, Provider::OpenAiChat);
    assert_eq!(preset.model, "fixture-model");

    assert_eq!(app.options.config_path.as_ref(), Some(&cfg_path));
    assert!(app.options.preset.is_some());
    assert_eq!(
        app.user_rules_path,
        Some(
            dir.path()
                .canonicalize()
                .unwrap()
                .join("tars_home/permissions.toml")
        )
    );

    let session = app.entries[0].session.as_ref().unwrap();
    let def = session.definition();
    assert_eq!(def.workspace_root, Some(ws.canonicalize().unwrap()));
    assert_eq!(def.tools, default_tools_for_current_platform());

    // Immediately usable for conversation turns.
    type_text(&mut app, "hello");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[0].execution == "Completed").await;
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux06_doctor_menu_tab_complete_async_report_and_early_close() {
    assert!(COMMANDS.iter().any(|(name, _)| *name == "doctor"));
    assert_eq!(COMMANDS[matching_commands("doc")[0]].0, "doctor");

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 1\nstore = 'store'\ndefault_profile = 'default'\n\
         [profiles.default]\nprovider = 'openai-chat'\nmodel = 'fixture-model'\napi_key_env = 'PATH'\n",
    )
    .unwrap();
    let config = Arc::new(Config::load(ConfigSource::File(cfg_path.clone())).unwrap());
    let preset = config.preset(None).unwrap();
    let mut app = App::new(Options {
        store: dir.path().join("store"),
        recovery_dir: dir.path().to_path_buf(),
        config,
        profile: None,
        preset,
        config_path: Some(cfg_path),
        code_execution_enabled: None,
        no_config: false,
        auto_onboard: false,
        onboarding_config_path: None,
    });
    app.clock = Clock::manual(std::time::Instant::now());

    // Open a session with workspace so workspace checks appear too.
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            "http://127.0.0.1:9/chat/completions",
            "PATH",
            "120",
            ws.to_str().unwrap(),
            "read_file,find_files",
            "ask",
            "",
            "on",
            "off",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(&mut app, |a| {
        a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;

    // Tab completion completes `/doc` -> `/doctor`.
    app.event(Event::Paste("/doc".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/doctor"]);
    app.entries[0].editor = crate::input::editor();

    // Run `/doctor` via command menu.
    run_slash_command(&mut app, "doctor");
    match &app.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, "Doctor · environment & configuration check");
            assert_eq!(lines, &["Running diagnostics..."]);
        }
        _ => panic!("expected loading Doctor panel"),
    }

    until(&mut app, |a| {
        matches!(
            &a.panel,
            Some(Panel::Info { lines, .. }) if lines.iter().any(|l| l.starts_with("Summary:"))
        )
    })
    .await;

    let view = screen(&app);
    assert!(
        view.contains("Doctor · environment & configuration check"),
        "{view}"
    );
    assert!(view.contains("[ok]") || view.contains("[warn]"), "{view}");
    if let Ok(path_val) = std::env::var("PATH")
        && path_val.len() > 8
    {
        assert!(!view.contains(&path_val), "doctor must not echo env values");
    }

    // Scrolling and Esc close work cleanly.
    app.event(key(KeyCode::Down));
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());

    // Closing before DoctorLoaded arrives drops the late result instead of reopening the panel.
    run_slash_command(&mut app, "doctor");
    assert!(app.panel.is_some());
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    until(&mut app, |a| a.jobs.is_empty()).await;
    assert!(app.panel.is_none());

    // Navigating to /help before DoctorLoaded arrives must not clobber the /help panel.
    run_slash_command(&mut app, "doctor");
    app.event(key(KeyCode::Esc));
    run_slash_command(&mut app, "help");
    match &app.panel {
        Some(Panel::Info { title, .. }) => assert_eq!(title, "Keyboard shortcuts"),
        _ => panic!("expected /help panel"),
    }
    until(&mut app, |a| a.jobs.is_empty()).await;
    match &app.panel {
        Some(Panel::Info { title, .. }) => assert_eq!(title, "Keyboard shortcuts"),
        _ => panic!("expected /help panel to remain intact after DoctorLoaded"),
    }
    app.event(key(KeyCode::Esc));

    // When `app.sandbox_status` is populated, `/doctor` reuses it without re-probing.
    let status = match tars_core::sandbox::Sandbox::open() {
        Ok(sb) => {
            let mut report = sb.report().clone();
            report.network =
                tars_core::sandbox::Capability::Unsupported("injected-doctor-reuse-check".into());
            tars_core::sandbox::SandboxStatus::Available(report)
        }
        Err(unavail) => tars_core::sandbox::SandboxStatus::Unavailable(unavail),
    };
    let expected_status_str = status.to_string();
    app.sandbox_status = Some(status);
    run_slash_command(&mut app, "doctor");
    until(&mut app, |a| {
        matches!(
            &a.panel,
            Some(Panel::Info { lines, .. }) if lines.iter().any(|l| l.starts_with("Summary:"))
        )
    })
    .await;
    let lines = match &app.panel {
        Some(Panel::Info { lines, .. }) => lines,
        _ => panic!("expected doctor panel"),
    };
    assert!(
        lines.iter().any(|l| l.contains(&expected_status_str)),
        "expected doctor output to contain injected sandbox status '{expected_status_str}': {lines:#?}"
    );
    #[cfg(target_os = "macos")]
    assert!(
        lines
            .iter()
            .any(|l| l.contains("injected-doctor-reuse-check")),
        "macOS doctor must reuse injected SandboxStatus without re-probing: {lines:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux05_empty_workspace_succeeds_with_empty_tools_and_too_small_guards_keys() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("tars_home/config.toml");
    let mut app = onboard_app(dir.path(), cfg_path.clone());
    app.list();
    until(&mut app, |a| a.onboarding.is_some()).await;

    // When terminal is too small, non-Ctrl-Q keys and paste are ignored, while Ctrl-Q quits.
    app.event(Event::Resize(20, 8));
    assert!(app.too_small);
    app.event(Event::Paste("ignored".into()));
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::Enter));
    assert!(app.onboarding.is_some());
    assert_eq!(
        app.onboarding.as_ref().unwrap().fields[1].lines().join(""),
        ""
    );
    app.event(ctrl('q'));
    assert!(app.quitting);

    // Restore normal size and submit onboarding with cleared empty Workspace.
    app.quitting = false;
    app.event(Event::Resize(100, 40));
    assert!(!app.too_small);
    type_text(&mut app, "fixture-model");
    app.onboarding.as_mut().unwrap().fields[2] = TextArea::from(["PATH"]);
    app.onboarding.as_mut().unwrap().fields[5] = TextArea::from([""]);

    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.onboarding.is_none() && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;

    assert!(cfg_path.is_file());
    let def = app.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(def.workspace_root, None);
    assert!(def.tools.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux07_init_guardrails_and_writable_vs_readonly_submission() {
    assert!(COMMANDS.iter().any(|(name, _)| *name == "init"));
    assert_eq!(COMMANDS[matching_commands("ini")[0]].0, "init");

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let server = Server::with_streaming_rounds(vec![
        vec![
            r#"data: {"id":"1","choices":[{"index":0,"delta":{"content":"Created AGENTS.md"},"finish_reason":null}]}"#.to_owned() + "\n\n",
            "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ],
        vec![
            r#"data: {"id":"2","choices":[{"index":0,"delta":{"content":"Draft AGENTS.md"},"finish_reason":null}]}"#.to_owned() + "\n\n",
            "data: {\"id\":\"2\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ],
    ])
    .await;

    // 1. No open session guardrail.
    let mut app = app(dir.path());
    run_slash_command(&mut app, "init");
    assert_eq!(
        app.notice,
        "Open a session with a workspace first to run /init"
    );

    // 2. No workspace bound guardrail.
    create(&mut app, &server.endpoint).await;
    run_slash_command(&mut app, "init");
    assert_eq!(
        app.entries[0].notice,
        "Bind a workspace to this session first to run /init"
    );

    // 3. Tab completion `/in` -> `/init`, plus busy & child-agent guardrails on a writable workspace session.
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            server.endpoint.as_str(),
            "PATH",
            "120",
            ws.to_str().unwrap(),
            "list_directory,read_file,write_file,edit_file",
            "ask",
            "",
            "on",
            "off",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(&mut app, |a| {
        a.entries.len() == 2 && a.entries[1].session.is_some()
    })
    .await;
    assert_eq!(app.selected, 1);

    app.event(Event::Paste("/in".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[1].editor.lines(), &["/init"]);

    // Preserve unrelated draft across `/init` via F1.
    app.entries[1].editor = TextArea::from(["keep my draft"]);
    app.entries[1].viewed_agent_id = Some("child-1".into());
    run_slash_command(&mut app, "init");
    assert_eq!(
        app.entries[1].notice,
        "Viewing child agent (read-only); run /agents main to run /init"
    );
    app.entries[1].viewed_agent_id = None;

    app.entries[1].busy = Some("opening");
    run_slash_command(&mut app, "init");
    assert_eq!(
        app.entries[1].notice,
        "Wait for the current session operation before running /init"
    );
    app.entries[1].busy = None;

    // 4. Writable `/init` submits writable prompt and does not touch input_history or editor draft.
    run_slash_command(&mut app, "init");
    until(&mut app, |a| a.entries[1].execution == "Completed").await;
    assert!(app.entries[1].input_history.is_empty());
    assert_eq!(app.entries[1].editor.lines(), &["keep my draft"]);

    // 5. Read-only workspace session submits read-only draft prompt.
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            server.endpoint.as_str(),
            "PATH",
            "120",
            ws.to_str().unwrap(),
            "list_directory,read_file",
            "ask",
            "",
            "on",
            "off",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(&mut app, |a| {
        a.entries.len() == 3 && a.entries[2].session.is_some()
    })
    .await;
    assert_eq!(app.selected, 2);

    run_slash_command(&mut app, "init");
    until(&mut app, |a| a.entries[2].execution == "Completed").await;

    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let writable_req = serde_json::to_string(&requests[0]).unwrap();
    assert!(writable_req.contains("AGENTS.md"), "{writable_req}");
    assert!(
        writable_req.contains("create `AGENTS.md` at the workspace root"),
        "{writable_req}"
    );
    assert!(!writable_req.contains("File write/edit tools are disabled"));

    let readonly_req = serde_json::to_string(&requests[1]).unwrap();
    assert!(readonly_req.contains("AGENTS.md"), "{readonly_req}");
    assert!(
        readonly_req.contains("File write/edit tools are disabled"),
        "{readonly_req}"
    );
}
