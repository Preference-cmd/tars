use super::support::{Server, app, app_with_folder_trust, create, ctrl, key, until};
use crate::{
    app::{
        App, COMMANDS, CONFIRM_GUARD_WINDOW, Clock, Panel, TRUST_PANEL_TITLE, jobs::Message,
        matching_commands,
    },
    input::{FolderTrustWizard, Form},
    options::{self, Options, StartupAction},
    render,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer, style::Color};
use ratatui_textarea::TextArea;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tars_core::{
    application::{ClosePolicy, PreparedCreate, SessionHost},
    config::{Config, ConfigSource, default_store_dir, default_tools_for_sandbox},
};

fn render_buf(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn buf_text(buf: &Buffer) -> String {
    buf.content().iter().map(|cell| cell.symbol()).collect()
}

fn screen_sized(app: &App, width: u16, height: u16) -> String {
    buf_text(&render_buf(app, width, height))
}

fn screen(app: &App) -> String {
    screen_sized(app, 100, 40)
}

fn run_slash_command(app: &mut App, name: &str) {
    app.event(key(KeyCode::F(1)));
    for ch in name.chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
}

fn write_preset_config(dir: &Path, store: &Path, ws: Option<&Path>) -> PathBuf {
    let cfg_path = dir.join("config.toml");
    let ws_line = ws
        .map(|p| format!("workspace_root = '{}'\n", p.display()))
        .unwrap_or_default();
    std::fs::write(
        &cfg_path,
        format!(
            "version = 1\n\
             store = '{}'\n\
             default_profile = 'default'\n\
             [profiles.default]\n\
             provider = 'openai-chat'\n\
             model = 'fixture-model'\n\
             api_key_env = 'PATH'\n\
             {ws_line}",
            store.display(),
        ),
    )
    .unwrap();
    cfg_path
}

fn configured_trust_app(dir: &Path, cfg_path: &Path, workspace: Option<PathBuf>) -> App {
    let config = Arc::new(Config::load(ConfigSource::File(cfg_path.to_path_buf())).unwrap());
    let preset = config.preset(None).unwrap();
    let mut app = App::new(Options {
        store: dir.join("store"),
        recovery_dir: dir.to_path_buf(),
        config,
        profile: None,
        preset,
        config_path: Some(cfg_path.to_path_buf()),
        workspace,
        code_execution_enabled: None,
        no_config: false,
        check_folder_trust: true,
        startup_action: StartupAction::Default,
    });
    app.clock = Clock::manual(std::time::Instant::now());
    app
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

    // --no-config with --store works and enables check_folder_trust in CLI launches.
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
    assert!(opts.check_folder_trust);
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
        assert!(opts.check_folder_trust);
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
    assert!(COMMANDS.iter().any(|(name, _)| *name == "trust"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft03_folder_trust_wizard_replaces_model_form_and_never_writes_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("project");
    std::fs::create_dir(&ws).unwrap();
    let mut app = app_with_folder_trust(dir.path(), Some(ws.clone()));

    app.list();
    until(&mut app, |a| !a.listing && a.folder_trust.is_some()).await;

    let view = screen(&app);
    assert!(
        view.contains("Do you trust the files in this folder?"),
        "{view}"
    );
    assert!(view.contains("1. Yes, trust this folder"), "{view}");
    assert!(view.contains("2. No, exit"), "{view}");
    assert!(view.contains(".tars/permissions.toml"), "{view}");
    assert!(view.contains(".tars/agents/*.md"), "{view}");
    assert!(view.contains(&ws.display().to_string()), "{view}");
    for forbidden in [
        "First-run setup",
        "Provider",
        "API key env var",
        "Max tokens",
        "/provider",
    ] {
        assert!(
            !view.contains(forbidden),
            "unexpected '{forbidden}': {view}"
        );
    }

    // Confirming Yes without any config.toml trusts the folder and enters main screen non-blockingly.
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.folder_trust.is_none() && a.jobs.is_empty()).await;

    assert!(!app.quitting);
    assert!(app.entries.is_empty());
    assert!(app.host.is_workspace_folder_trusted(&ws).await.unwrap());
    assert_eq!(
        app.notice,
        "No model configured · run /new (F2) or configure ~/.tars/config.toml"
    );
    assert!(!dir.path().join("config.toml").exists());

    let main = screen(&app);
    assert!(main.contains("Welcome to TARS"), "{main}");
    assert!(main.contains("✓ Trusted"), "{main}");
    assert!(
        main.contains("/new (F2) to start, or configure ~/.tars/config.toml"),
        "{main}"
    );
    assert!(!main.contains("/provider"), "{main}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft04_pre_session_gating_guard_window_compact_and_startup_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let other_ws = dir.path().join("other-ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&other_ws).unwrap();
    let cfg_path = write_preset_config(dir.path(), &dir.path().join("store"), None);

    // Seed an unrelated session in the store so we prove existing sessions do NOT skip folder trust.
    let seeded_host = SessionHost::new(Some(dir.path().join("store")));
    let mut seed_def = super::support::history_definition(None);
    seed_def.workspace_root = Some(other_ws.clone());
    let prepared = PreparedCreate::prepare_persistent(seed_def, None).unwrap();
    let seeded_id = prepared.conversation_id().as_str().to_owned();
    let seeded = seeded_host.create(prepared).await.unwrap();
    seeded_host
        .close(&seeded.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 1. Untrusted workspace with preset & existing store session still gates on FolderTrustWizard.
    let mut app = configured_trust_app(dir.path(), &cfg_path, Some(ws.clone()));
    app.list();
    until(&mut app, |a| !a.listing && a.folder_trust.is_some()).await;
    assert_eq!(app.entries.len(), 1);
    assert!(
        app.entries.iter().all(|e| e.session.is_none()),
        "must not create/open any session before folder trust is confirmed"
    );

    // During 500ms guard window: navigation works, Yes keys are blocked, No/Esc/Ctrl-C exits.
    app.event(key(KeyCode::Char('y')));
    app.event(key(KeyCode::Char('1')));
    app.event(key(KeyCode::Enter));
    assert!(app.folder_trust.is_some());
    assert!(!app.host.is_workspace_folder_trusted(&ws).await.unwrap());

    app.event(key(KeyCode::Char('j')));
    assert_eq!(app.folder_trust.as_ref().unwrap().selected, 1);
    app.event(key(KeyCode::Char('k')));
    assert_eq!(app.folder_trust.as_ref().unwrap().selected, 0);
    app.event(key(KeyCode::Tab));
    assert_eq!(app.folder_trust.as_ref().unwrap().selected, 1);
    app.event(key(KeyCode::BackTab));
    assert_eq!(app.folder_trust.as_ref().unwrap().selected, 0);

    // Compact 30 x 12 renders wizard cleanly; < 30 x 12 renders resize prompt and allows Ctrl-C / Ctrl-Q.
    let compact = screen_sized(&app, 30, 12);
    let compact_norm = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        compact_norm.contains("Do you trust the files in this folder?"),
        "{compact}"
    );
    assert!(
        compact_norm.contains("1. Yes, trust this folder"),
        "{compact}"
    );
    assert!(compact_norm.contains("2. No, exit"), "{compact}");

    app.event(Event::Resize(29, 11));
    assert!(app.too_small);
    let tiny = screen_sized(&app, 29, 11);
    let tiny_norm = tiny.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        tiny_norm.contains("Resize to 30 x 12. Ctrl-C / Ctrl-Q quits."),
        "{tiny}"
    );
    app.event(ctrl('c'));
    assert!(app.quitting);
    assert!(!app.host.is_workspace_folder_trusted(&ws).await.unwrap());

    // Decline with 'n' during guard window on a fresh wizard -> zero DB writes, zero new sessions, quits.
    let mut decline_app = configured_trust_app(dir.path(), &cfg_path, Some(ws.clone()));
    decline_app.list();
    until(&mut decline_app, |a| !a.listing && a.folder_trust.is_some()).await;
    decline_app.event(key(KeyCode::Char('n')));
    assert!(decline_app.quitting);
    assert!(
        !decline_app
            .host
            .is_workspace_folder_trusted(&ws)
            .await
            .unwrap()
    );
    assert_eq!(decline_app.entries.len(), 1);

    // 2. Confirming after guard trusts folder and auto-creates default session.
    app.quitting = false;
    app.event(Event::Resize(100, 40));
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.folder_trust.is_none() && a.entries.iter().any(|e| e.session.is_some())
    })
    .await;
    assert!(app.host.is_workspace_folder_trusted(&ws).await.unwrap());
    let opened_ws = app
        .entries
        .iter()
        .find_map(|e| e.session.as_ref())
        .unwrap()
        .definition()
        .workspace_root
        .unwrap();
    assert_eq!(opened_ws, ws.canonicalize().unwrap());

    // F5 refresh never re-triggers wizard.
    app.event(key(KeyCode::F(5)));
    until(&mut app, |a| !a.listing).await;
    assert!(app.folder_trust.is_none());

    // 3. Reopening on the already-trusted directory skips wizard immediately.
    let mut trusted_reopen = configured_trust_app(dir.path(), &cfg_path, Some(ws.clone()));
    trusted_reopen.list();
    until(&mut trusted_reopen, |a| {
        !a.listing && a.entries.iter().any(|e| e.session.is_some())
    })
    .await;
    assert!(trusted_reopen.folder_trust.is_none());

    // 4. -c, -r, and -r <id> NEVER trigger FolderTrustWizard even on untrusted cwd/workspace.
    let untrusted_ws = dir.path().join("never-trusted");
    std::fs::create_dir_all(&untrusted_ws).unwrap();
    for action in [
        StartupAction::ContinueLatest,
        StartupAction::OpenSessionPicker,
        StartupAction::Resume(seeded_id),
    ] {
        let mut flag_app = configured_trust_app(dir.path(), &cfg_path, Some(untrusted_ws.clone()));
        flag_app.options.startup_action = action;
        flag_app.list();
        until(&mut flag_app, |a| !a.listing && a.jobs.is_empty()).await;
        assert!(flag_app.folder_trust.is_none());
    }

    // 5. InvalidWorkspace skips wizard and degrades cleanly (both with and without preset).
    let missing_ws = dir.path().join("does-not-exist");
    let mut invalid_with_preset =
        configured_trust_app(dir.path(), &cfg_path, Some(missing_ws.clone()));
    invalid_with_preset.list();
    until(&mut invalid_with_preset, |a| {
        !a.listing && a.jobs.is_empty()
    })
    .await;
    assert!(invalid_with_preset.folder_trust.is_none());
    assert!(
        invalid_with_preset
            .notice
            .contains("Could not start default session:")
            && invalid_with_preset.notice.contains("Run /doctor or /new"),
        "{}",
        invalid_with_preset.notice
    );

    let mut invalid_no_preset = app_with_folder_trust(dir.path(), Some(missing_ws));
    invalid_no_preset.list();
    until(&mut invalid_no_preset, |a| !a.listing && a.jobs.is_empty()).await;
    assert!(invalid_no_preset.folder_trust.is_none());
    assert!(
        invalid_no_preset.notice.starts_with("Invalid workspace:"),
        "{}",
        invalid_no_preset.notice
    );

    // 6. Store failure keeps wizard open with error and creates zero sessions.
    let mut no_store_app = app_with_folder_trust(dir.path(), Some(untrusted_ws.clone()));
    no_store_app.host = SessionHost::new(None);
    no_store_app.folder_trust = Some(FolderTrustWizard::new(
        untrusted_ws,
        no_store_app.clock.now(),
    ));
    no_store_app.advance_clock(CONFIRM_GUARD_WINDOW);
    no_store_app.event(key(KeyCode::Char('y')));
    until(&mut no_store_app, |a| {
        a.folder_trust.as_ref().is_some_and(|w| !w.saving) && a.jobs.is_empty()
    })
    .await;
    let wizard = no_store_app.folder_trust.as_ref().unwrap();
    assert!(!wizard.error.is_empty());
    assert!(no_store_app.entries.is_empty());

    // 7. Key Repeat never accidentally confirms or toggles wizard, and untrusted text is escaped.
    let mut escape_app = app_with_folder_trust(dir.path(), None);
    let mut w =
        FolderTrustWizard::new(PathBuf::from("/tmp/evil\x1b[31mws"), escape_app.clock.now());
    w.error = "bad\x1b[31merror".into();
    escape_app.folder_trust = Some(w);
    escape_app.advance_clock(CONFIRM_GUARD_WINDOW);
    escape_app.event(Event::Key(KeyEvent {
        kind: KeyEventKind::Repeat,
        ..KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
    }));
    assert!(escape_app.folder_trust.is_some());
    let rendered = screen(&escape_app);
    assert!(!rendered.contains('\x1b'), "must escape ANSI escapes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft05_folder_trust_never_bypasses_tars_rules_or_agents_trust() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    std::fs::create_dir_all(ws.join(".tars/agents")).unwrap();
    std::fs::write(
        ws.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"git\", \"status\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join(".tars/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Code reviewer\n---\nReview carefully.\n",
    )
    .unwrap();
    let cfg_path = write_preset_config(dir.path(), &dir.path().join("store"), Some(&ws));

    let mut app = configured_trust_app(dir.path(), &cfg_path, Some(ws.clone()));
    app.list();
    until(&mut app, |a| !a.listing && a.folder_trust.is_some()).await;

    // Confirm folder trust via wizard.
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.folder_trust.is_none() && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;

    assert!(app.host.is_workspace_folder_trusted(&ws).await.unwrap());
    let entry = &app.entries[0];
    assert!(
        entry.workspace_rules_trust.is_some(),
        "folder trust must NOT bypass .tars/permissions.toml trust prompt"
    );
    assert!(
        entry.workspace_agents_trust.is_some(),
        "folder trust must NOT bypass .tars/agents/*.md trust prompt"
    );
    assert!(!entry.session.as_ref().unwrap().workspace_rules_trusted());
    assert!(!entry.session.as_ref().unwrap().workspace_agents_trusted());

    // Trust rules and agents, close, mutate files, reopen -> content-hash mismatch re-prompts!
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[0].workspace_rules_trust.is_none() && a.jobs.is_empty()
    })
    .await;
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[0].workspace_agents_trust.is_none() && a.jobs.is_empty()
    })
    .await;
    assert!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );
    assert!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .workspace_agents_trusted()
    );

    app.event(ctrl('w'));
    until(&mut app, |a| a.entries[0].session.is_none()).await;

    std::fs::write(
        ws.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"git\", \"status\"]\ndecision = \"allow\"\n\n[[command]]\nprefix = [\"cargo\", \"test\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join(".tars/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Modified reviewer\n---\nReview even more carefully.\n",
    )
    .unwrap();

    app.event(key(KeyCode::F(3)));
    until(&mut app, |a| a.entries[0].session.is_some()).await;
    assert!(app.entries[0].workspace_rules_trust.is_some());
    assert!(app.entries[0].workspace_agents_trust.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft10_welcome_card_trust_badge_f4_switching_and_slash_trust() {
    let dir = tempfile::tempdir().unwrap();
    let ws_trusted = dir.path().join("ws-trusted");
    let ws_other = dir.path().join("ws-other");
    std::fs::create_dir_all(ws_trusted.join(".tars/agents")).unwrap();
    std::fs::create_dir_all(&ws_other).unwrap();
    std::fs::write(
        ws_trusted.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"git\", \"status\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        ws_trusted.join(".tars/agents/helper.md"),
        "---\nname: helper\ndescription: Helper\n---\nHelp.\n",
    )
    .unwrap();

    // 1. Unconfigured empty welcome card shows ~/.tars/config.toml guidance and Ctrl-C exit, never /provider.
    let mut unconfigured = app_with_folder_trust(dir.path(), Some(ws_trusted.clone()));
    unconfigured.list();
    until(&mut unconfigured, |a| {
        !a.listing && a.folder_trust.is_some()
    })
    .await;
    unconfigured.advance_clock(CONFIRM_GUARD_WINDOW);
    unconfigured.event(key(KeyCode::Char('y')));
    until(&mut unconfigured, |a| {
        a.folder_trust.is_none() && a.jobs.is_empty()
    })
    .await;

    let buf = render_buf(&unconfigured, 100, 40);
    let view = buf_text(&buf);
    assert!(view.contains("Welcome to TARS"), "{view}");
    assert!(view.contains("✓ Trusted"), "{view}");
    assert!(
        view.contains("/new (F2) to start, or configure ~/.tars/config.toml"),
        "{view}"
    );
    assert!(view.contains("Ctrl-C exit"), "{view}");
    assert!(!view.contains("/provider"), "{view}");
    let trusted_cell = buf
        .content()
        .iter()
        .find(|cell| cell.symbol() == "✓")
        .expect("green checkmark cell");
    assert_eq!(trusted_cell.fg, Color::Green);

    // 2. Configured welcome card (before opening) shows `/new · /sessions · /commands`.
    let cfg_path = write_preset_config(dir.path(), &dir.path().join("store"), Some(&ws_other));
    let configured_only = configured_trust_app(dir.path(), &cfg_path, Some(ws_other.clone()));
    let cfg_view = screen(&configured_only);
    assert!(cfg_view.contains("Welcome to TARS"), "{cfg_view}");
    assert!(
        cfg_view.contains("/new · /sessions · /commands"),
        "{cfg_view}"
    );
    assert!(!cfg_view.contains("✓ Trusted"), "{cfg_view}");

    // 3. F4 switching between trusted ws, different ws, and unbound session never shows stale badge.
    let server = Server::new().await;
    let multi_dir = tempfile::tempdir().unwrap();
    let mut multi = app_with_folder_trust(multi_dir.path(), Some(ws_trusted.clone()));
    multi.list();
    until(&mut multi, |a| !a.listing && a.folder_trust.is_some()).await;
    multi.advance_clock(CONFIRM_GUARD_WINDOW);
    multi.event(key(KeyCode::Char('y')));
    until(&mut multi, |a| {
        a.folder_trust.is_none() && a.jobs.is_empty()
    })
    .await;

    // Create entry 0 (ws_trusted), entry 1 (ws_other), entry 2 (unbound).
    for ws_arg in [ws_trusted.to_str().unwrap(), ws_other.to_str().unwrap(), ""] {
        multi.form = Some(Form {
            fields: [
                "openai-chat",
                "fixture-model",
                server.endpoint.as_str(),
                "PATH",
                "120",
                ws_arg,
                "",
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
        let expected_len = multi.entries.len() + 1;
        multi.event(ctrl('s'));
        assert!(
            multi.form.is_none(),
            "form error: {:?}",
            multi.form.as_ref().map(|f| &f.error)
        );
        until(&mut multi, |a| {
            a.entries.len() == expected_len && a.entries[expected_len - 1].session.is_some()
        })
        .await;
    }
    // Dismiss trust prompts on entry 0 so welcome card renders cleanly.
    multi.event(key(KeyCode::F(4)));
    assert_eq!(multi.selected, 0);
    multi.advance_clock(CONFIRM_GUARD_WINDOW);
    multi.event(key(KeyCode::Char('n')));
    multi.advance_clock(CONFIRM_GUARD_WINDOW);
    multi.event(key(KeyCode::Char('n')));
    let trusted_screen = screen(&multi);
    assert!(trusted_screen.contains("✓ Trusted"), "{trusted_screen}");
    assert!(
        trusted_screen.contains("Type below, then press Enter to send."),
        "{trusted_screen}"
    );

    // F4 -> entry 1 (ws_other): must NOT show ✓ Trusted!
    multi.event(key(KeyCode::F(4)));
    assert_eq!(multi.selected, 1);
    let other_screen = screen(&multi);
    assert!(!other_screen.contains("✓ Trusted"), "{other_screen}");

    // F4 -> entry 2 (unbound): must NOT show ✓ Trusted, and /trust reports unbound!
    multi.event(key(KeyCode::F(4)));
    assert_eq!(multi.selected, 2);
    let unbound_screen = screen(&multi);
    assert!(!unbound_screen.contains("✓ Trusted"));
    assert!(unbound_screen.contains("Workspace: none"));

    run_slash_command(&mut multi, "trust");
    match &multi.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, TRUST_PANEL_TITLE);
            assert!(
                lines
                    .iter()
                    .any(|l| l == "Workspace: none (not bound to a folder)"),
                "{lines:#?}"
            );
        }
        _ => panic!("expected Workspace trust panel for unbound session"),
    }
    multi.event(key(KeyCode::Esc));

    // Switch back to entry 0 (ws_trusted) and run /trust -> shows folder trust + .tars/ status!
    multi.event(key(KeyCode::F(4)));
    assert_eq!(multi.selected, 0);
    assert_eq!(COMMANDS[matching_commands("tru")[0]].0, "trust");
    run_slash_command(&mut multi, "trust");
    match &multi.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, TRUST_PANEL_TITLE);
            assert_eq!(lines, &["Loading workspace trust status..."]);
        }
        _ => panic!("expected loading Workspace trust panel"),
    }
    until(&mut multi, |a| {
        matches!(
            &a.panel,
            Some(Panel::Info { title, lines, .. })
                if title == TRUST_PANEL_TITLE && lines.iter().any(|l| l.contains("tars trust revoke"))
        )
    })
    .await;
    let trust_view = screen(&multi);
    assert!(trust_view.contains(TRUST_PANEL_TITLE), "{trust_view}");
    assert!(
        trust_view.contains("Folder trust (trusted_workspaces): Trusted"),
        "{trust_view}"
    );
    assert!(
        trust_view.contains(".tars/permissions.toml") && trust_view.contains("Not trusted"),
        "{trust_view}"
    );
    assert!(
        trust_view.contains(".tars/agents/*.md") && trust_view.contains("helper"),
        "{trust_view}"
    );
    assert!(
        trust_view.contains("Run 'tars trust revoke' to revoke folder trust"),
        "{trust_view}"
    );
    multi.event(key(KeyCode::Esc));

    // Stale TrustStatusLoaded must NOT clobber /help or reopen a closed panel.
    run_slash_command(&mut multi, "help");
    multi.complete(Message::TrustStatusLoaded(vec!["stale".into()]));
    match &multi.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, "Keyboard shortcuts");
            assert!(lines.iter().any(|l| l.contains("/trust")));
        }
        _ => panic!("expected /help panel to remain intact"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ux06_doctor_menu_tab_complete_async_report_and_early_close() {
    assert!(COMMANDS.iter().any(|(name, _)| *name == "doctor"));
    assert_eq!(COMMANDS[matching_commands("doc")[0]].0, "doctor");

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let cfg_path = write_preset_config(dir.path(), &dir.path().join("store"), None);
    let config = Arc::new(Config::load(ConfigSource::File(cfg_path.clone())).unwrap());
    let preset = config.preset(None).unwrap();
    let mut app = App::new(Options {
        store: dir.path().join("store"),
        recovery_dir: dir.path().to_path_buf(),
        config,
        profile: None,
        preset,
        config_path: Some(cfg_path),
        workspace: None,
        code_execution_enabled: None,
        no_config: false,
        check_folder_trust: false,
        startup_action: StartupAction::Default,
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if matches!(
        app.sandbox_status,
        Some(tars_core::sandbox::SandboxStatus::Available(_))
    ) {
        assert!(
            lines
                .iter()
                .any(|l| l.contains("injected-doctor-reuse-check")),
            "doctor must reuse injected SandboxStatus without re-probing: {lines:#?}"
        );
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folder_trust_reuses_startup_sandbox_status_for_default_tools() {
    use tars_core::sandbox::{Capability, Sandbox, SandboxPolicy, SandboxStatus};

    let root = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::new([root.path().to_path_buf()], []).unwrap();
    let cases: Vec<(&str, SandboxStatus, bool)> = match Sandbox::open() {
        Ok(sb) => {
            let mut bad_report = sb.report().clone();
            bad_report.network = Capability::Unsupported("injected-unavailable".into());
            let err = bad_report.check(&policy).unwrap_err();
            vec![
                (
                    "available",
                    SandboxStatus::Available(sb.report().clone()),
                    true,
                ),
                ("unavailable", SandboxStatus::Unavailable(err), false),
            ]
        }
        Err(err) => vec![("unavailable", SandboxStatus::Unavailable(err), false)],
    };

    for (tag, sandbox_status, expected_available) in cases {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join(format!("workspace_{tag}"));
        std::fs::create_dir_all(&ws).unwrap();
        let cfg_path = write_preset_config(dir.path(), &dir.path().join("store"), Some(&ws));

        let mut app = configured_trust_app(dir.path(), &cfg_path, Some(ws.clone()));
        app.sandbox_status = Some(sandbox_status);
        app.list();
        until(&mut app, |a| !a.listing && a.folder_trust.is_some()).await;

        app.advance_clock(CONFIRM_GUARD_WINDOW);
        app.event(key(KeyCode::Char('y')));
        until(&mut app, |a| {
            a.folder_trust.is_none() && a.entries.len() == 1 && a.entries[0].session.is_some()
        })
        .await;

        let def = app.entries[0].session.as_ref().unwrap().definition();
        assert_eq!(
            def.tools,
            default_tools_for_sandbox(expected_available),
            "case={tag}"
        );
    }
}
