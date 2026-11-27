use super::support::*;
use crate::{
    app::{
        App, COMMANDS, CONFIRM_GUARD_WINDOW, Panel, matching_commands,
        provider::{CredentialStoreCache, ProviderFieldKind, ProviderWizardStep},
    },
    input::{FIELD_NAMES, Form},
    options::{Options, StartupAction},
    render,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend, style::Color};
use ratatui_textarea::TextArea;
use std::path::PathBuf;
use std::sync::Arc;
use tars_core::{
    application::ClosePolicy,
    config::{Config, CredentialsStore, PROVIDER_PRESETS},
    definition::{CredentialRef, Provider, WorkspaceTool},
};

fn render_buf(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn screen_sized(app: &App, width: u16, height: u16) -> String {
    render_buf(app, width, height)
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn screen(app: &App) -> String {
    screen_sized(app, 100, 50)
}

fn run_command(app: &mut App, name: &str) {
    app.event(key(KeyCode::F(1)));
    for ch in name.chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
}

fn app_with_config(dir: &std::path::Path, workspace: Option<PathBuf>) -> App {
    let cfg_path = dir.join("config.toml");
    let creds_path = dir.join("credentials.toml");
    let mut app = App::new(Options {
        store: dir.join("store"),
        recovery_dir: dir.to_path_buf(),
        config: Arc::new(Config::default()),
        profile: None,
        preset: None,
        config_path: Some(cfg_path),
        credentials_path: Some(creds_path),
        workspace,
        code_execution_enabled: None,
        no_config: false,
        check_folder_trust: false,
        startup_action: StartupAction::Default,
    });
    app.clock = crate::app::Clock::manual(std::time::Instant::now());
    app
}

#[test]
fn cr10_provider_command_registered_and_no_config_refused() {
    assert!(COMMANDS.iter().any(|(name, _)| *name == "provider"));
    assert_eq!(COMMANDS[matching_commands("prov")[0]].0, "provider");

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.options.no_config = true;
    app.options.config_path = Some(dir.path().join("config.toml"));
    app.options.credentials_path = Some(dir.path().join("credentials.toml"));

    run_command(&mut app, "provider");
    assert!(app.panel.is_none());
    assert_eq!(
        app.notice,
        "Disabled under --no-config; use /new or omit --no-config"
    );
    assert!(!dir.path().join("config.toml").exists());
    assert!(!dir.path().join("credentials.toml").exists());

    // Empty `/model` under `--no-config` shows disabled notice instead of opening wizard.
    run_command(&mut app, "model");
    assert!(app.panel.is_none());
    assert_eq!(
        app.notice,
        "No model profiles configured (--no-config); use /new"
    );

    // `/help` documents `/provider`.
    run_command(&mut app, "help");
    let help_view = screen(&app);
    assert!(help_view.contains("/provider"), "{help_view}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr10_provider_wizard_presets_ollama_masking_paste_esc_and_compact() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app_with_config(dir.path(), None);

    run_command(&mut app, "provider");
    assert!(matches!(app.panel, Some(Panel::Provider(_))));

    let step1_view = screen(&app);
    assert!(step1_view.contains("Add model provider"), "{step1_view}");
    for preset in PROVIDER_PRESETS {
        assert!(step1_view.contains(preset.label), "{step1_view}");
    }
    assert!(step1_view.contains("Custom provider"), "{step1_view}");

    // Navigate with j/k and select Ollama (index 6).
    for _ in 0..6 {
        app.event(key(KeyCode::Char('j')));
    }
    app.event(key(KeyCode::Enter));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::Configure);
        assert_eq!(wizard.profile_name, "ollama");
        assert_eq!(wizard.model, "qwen3:8b");
        assert_eq!(wizard.api_key, "ollama");
        assert!(wizard.ollama_default_hint);
    } else {
        panic!("expected Step 2");
    }
    let ollama_view = screen(&app);
    assert!(ollama_view.contains("••••••"), "{ollama_view}");
    assert!(
        ollama_view.contains("local default · replace if your server requires auth"),
        "{ollama_view}"
    );

    // Compact 30x12 renders Step 2 cleanly without panic.
    let compact_view = screen_sized(&app, 30, 12);
    assert!(compact_view.contains("Configure"), "{compact_view}");

    // Esc from Step 2 clears secret buffer and returns to Step 1.
    app.event(key(KeyCode::Esc));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::SelectPreset);
        assert!(wizard.api_key.is_empty());
        assert_eq!(wizard.api_key.capacity(), 0);
    } else {
        panic!("expected Step 1");
    }

    // Select OpenAI (index 0), paste API key, verify masking & zero plaintext in view.
    for _ in 0..6 {
        app.event(key(KeyCode::Char('k')));
    }
    app.event(key(KeyCode::Enter));
    let secret = "sk-paste-never-echo-98765";
    app.event(Event::Paste(format!("{secret}\r\n\x1b")));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.current_field(), Some(ProviderFieldKind::ApiKey));
        assert_eq!(wizard.api_key, secret);
    } else {
        panic!("expected Step 2");
    }
    let masked_view = screen(&app);
    assert!(!masked_view.contains(secret), "secret leaked into UI!");
    assert!(
        masked_view.contains(&"•".repeat(secret.chars().count())),
        "{masked_view}"
    );

    // Esc in Step 2 -> Step 1, Esc in Step 1 -> closes panel.
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr10_provider_save_hot_reloads_host_preserving_open_sessions_and_saves_0600() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let creds_path = dir.path().join("credentials.toml");
    let server = Server::new().await;

    let mut app = app_with_config(dir.path(), None);
    // Open an existing session A BEFORE running `/provider`.
    create(&mut app, &server.endpoint).await;
    let existing_id = app.entries[0].id().to_owned();
    assert!(app.host.lookup(&existing_id.clone().into()).is_some());

    // Open `/provider`, choose Custom (index 7), configure Anthropic with protocol cycling & paste.
    run_command(&mut app, "provider");
    for _ in 0..7 {
        app.event(key(KeyCode::Down));
    }
    app.event(key(KeyCode::Enter));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(wizard.is_custom);
        assert_eq!(wizard.current_field(), Some(ProviderFieldKind::Protocol));
    } else {
        panic!("expected Custom Step 2");
    }

    // Paste on Protocol field is ignored.
    app.event(Event::Paste("ignored-on-protocol".into()));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.provider, Provider::OpenAiChat);
    }

    // Cycle Protocol: OpenAiChat -> OpenAiResponses -> Anthropic (auto-fills max_tokens = 8192).
    app.event(key(KeyCode::Right));
    app.event(key(KeyCode::Char(' ')));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.provider, Provider::Anthropic);
        assert_eq!(wizard.max_tokens, "8192");
    }
    // Cycle back to OpenAiChat -> restores empty max_tokens.
    app.event(key(KeyCode::Right));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.provider, Provider::OpenAiChat);
        assert_eq!(wizard.max_tokens, "");
    }

    // Enter custom endpoint, profile name, model, and API key via paste.
    app.event(key(KeyCode::Tab)); // Endpoint
    app.event(Event::Paste(server.endpoint.clone()));
    app.event(key(KeyCode::Tab)); // ProfileName ("custom" -> append "-gw")
    app.event(Event::Paste("-gw".into()));
    app.event(key(KeyCode::Tab)); // Model
    app.event(Event::Paste("custom-model-1".into()));
    app.event(key(KeyCode::Tab)); // ApiKey
    let secret = "sk-custom-store-secret-4242";
    app.event(Event::Paste(secret.into()));

    app.event(ctrl('s'));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;

    // 1. Existing open session A was NOT auto-replaced and STILL exists in HostInner.entries!
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].id(), existing_id);
    assert!(
        app.host.lookup(&existing_id.clone().into()).is_some(),
        "host must be updated in place via update_runtime_config without losing open entries"
    );
    assert_eq!(
        app.notice,
        "Saved provider custom-gw · ready for /new or /model"
    );

    // 2. credentials.toml exists with 0600 permissions and secret; config.toml has NO plaintext secret.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&creds_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    assert_eq!(
        CredentialsStore::load(&creds_path)
            .unwrap()
            .get("custom-gw"),
        Some(secret)
    );
    let config_raw = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        config_raw.contains("credential = \"custom-gw\""),
        "{config_raw}"
    );
    assert!(
        !config_raw.contains(secret),
        "config.toml leaked plaintext secret!"
    );

    // 3. Switching via `/model` works using the hot-reloaded `credentials_path`, and closing old session works.
    run_command(&mut app, "model");
    let models_view = screen(&app);
    assert!(models_view.contains("custom-gw"), "{models_view}");
    assert!(
        models_view.contains("+ Add model provider (/provider)"),
        "{models_view}"
    );
    // Select `+ Add model provider (/provider)` (index 1) from Panel::Models jumps to Panel::Provider.
    app.event(key(KeyCode::Down));
    app.event(key(KeyCode::Down));
    app.event(key(KeyCode::Enter));
    assert!(matches!(app.panel, Some(Panel::Provider(_))));
    app.event(key(KeyCode::Esc));

    // Close original session A cleanly (proves HostInner.entries stayed intact!).
    app.close(0, ClosePolicy::Save);
    until(&mut app, |a| {
        a.entries[0].session.is_none() && a.jobs.is_empty()
    })
    .await;
    assert!(
        app.entries[0].notice.contains("saved r"),
        "{}",
        app.entries[0].notice
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr10_provider_saved_respects_folder_trust_and_never_bypasses_wizard() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let canonical_ws = ws.canonicalize().unwrap();

    // 1. Untrusted workspace with FolderTrustWizard pending: ProviderSaved MUST NOT auto-open session.
    let mut untrusted = app_with_config(dir.path(), Some(ws.clone()));
    untrusted.options.check_folder_trust = true;
    untrusted.list();
    until(&mut untrusted, |a| !a.listing && a.folder_trust.is_some()).await;
    // Even if ProviderSaved arrives while folder_trust is pending, no session is opened.
    untrusted.open_provider_wizard();
    untrusted.event(key(KeyCode::Enter)); // openai
    untrusted.event(Event::Paste("sk-untrusted-test".into()));
    untrusted.event(key(KeyCode::Enter));
    until(&mut untrusted, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    assert!(untrusted.entries.is_empty());
    assert!(untrusted.folder_trust.is_some());

    // 2. Trusted workspace with no open session: ProviderSaved auto-creates & opens session
    //    bound to `trusted_workspace` with `default_workspace_tools()`.
    let dir2 = tempfile::tempdir().unwrap();
    let mut trusted = app_with_config(dir2.path(), Some(ws.clone()));
    trusted.options.check_folder_trust = true;
    trusted.list();
    until(&mut trusted, |a| !a.listing && a.folder_trust.is_some()).await;
    trusted.advance_clock(CONFIRM_GUARD_WINDOW);
    trusted.event(key(KeyCode::Char('y')));
    until(&mut trusted, |a| {
        a.folder_trust.is_none() && a.jobs.is_empty()
    })
    .await;
    assert_eq!(trusted.trusted_workspace.as_ref(), Some(&canonical_ws));

    trusted.open_provider_wizard();
    for _ in 0..6 {
        trusted.event(key(KeyCode::Down)); // ollama (prefills "ollama")
    }
    trusted.event(key(KeyCode::Enter));
    trusted.event(key(KeyCode::Enter));
    until(&mut trusted, |a| {
        a.panel.is_none() && !a.entries.is_empty() && a.entries[0].session.is_some()
    })
    .await;

    let def = trusted.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(def.workspace_root.as_ref(), Some(&canonical_ws));
    assert_eq!(def.credential, CredentialRef::Store("ollama".into()));
    assert_eq!(def.tools, trusted.default_workspace_tools());

    // 3. Counterexample: symlink config.toml fails closed, keeps Panel::Provider open, never leaks secret.
    #[cfg(unix)]
    {
        let dir3 = tempfile::tempdir().unwrap();
        let target = dir3.path().join("real.toml");
        std::fs::write(&target, "version = 1\n").unwrap();
        std::os::unix::fs::symlink(&target, dir3.path().join("config.toml")).unwrap();
        let mut sym_app = app_with_config(dir3.path(), None);
        sym_app.open_provider_wizard();
        sym_app.event(key(KeyCode::Enter));
        let secret = "sk-symlink-must-not-leak-999";
        sym_app.event(Event::Paste(secret.into()));
        sym_app.event(key(KeyCode::Enter));
        until(
            &mut sym_app,
            |a| matches!(&a.panel, Some(Panel::Provider(w)) if !w.saving && !w.error.is_empty()),
        )
        .await;
        if let Some(Panel::Provider(wizard)) = &sym_app.panel {
            assert!(!wizard.error.contains(secret));
            assert!(wizard.api_key.is_empty());
        } else {
            panic!("panel must remain open on error");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr11_new_form_workspace_prefill_selectors_anthropic_max_tokens_and_status_badges() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    let ws = ws.canonicalize().unwrap();
    let creds_path = dir.path().join("credentials.toml");
    CredentialsStore::set(&creds_path, "work", "sk-hidden-badge-secret-777").unwrap();

    // 1. Opening `/new` with workspace resolves `Workspace root` and `Tools`.
    let mut app = app_with_config(dir.path(), Some(ws.clone()));
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        !matches!(a.credential_store_cache, CredentialStoreCache::Unknown)
    })
    .await;

    let form = app.form.as_mut().unwrap();
    assert_eq!(FIELD_NAMES[3], "Credential (store name, or env:VAR)");
    assert_eq!(form.fields[3].lines().join(""), "openai-chat");
    assert_eq!(form.fields[5].lines().join(""), ws.display().to_string());
    assert!(!form.fields[6].lines().join("").is_empty());

    // Counterexample: `with_workspace_defaults(None, ...)` leaves BOTH workspace and tools empty!
    let empty_ws_form = Form::default().with_workspace_defaults(None, &[WorkspaceTool::ReadFile]);
    assert_eq!(empty_ws_form.fields[5].lines().join(""), "");
    assert_eq!(empty_ws_form.fields[6].lines().join(""), "");

    // Counterexample: preset with explicit workspace & tools is NEVER overwritten.
    let mut preset = Form::default();
    preset.fields[1] = TextArea::from(["m"]);
    preset.fields[5] = TextArea::from([ws.display().to_string()]);
    preset.fields[6] = TextArea::from(["read_file"]);
    let preset_def = preset.definition().unwrap();
    let preserved = Form::new(Arc::new(Config::default()), None, Some(&preset_def))
        .with_workspace_defaults(Some(dir.path().to_path_buf()), &[WorkspaceTool::WriteFile]);
    assert_eq!(
        preserved.fields[5].lines().join(""),
        ws.display().to_string()
    );
    assert_eq!(preserved.fields[6].lines().join(""), "read_file");

    // 2. Pure `◀ value ▶` selectors on fields 0, 7, 9, 10 ignore typing, backspace, and paste.
    let form = app.form.as_mut().unwrap();
    form.selected = 0; // Provider
    let rendered = screen(&app);
    assert!(rendered.contains("◀ openai-chat ▶"), "{rendered}");
    assert!(rendered.contains("◀ ask ▶"), "{rendered}");
    assert!(rendered.contains("◀ on ▶"), "{rendered}");
    assert!(rendered.contains("◀ off ▶"), "{rendered}");

    app.event(key(KeyCode::Char('x')));
    app.event(key(KeyCode::Backspace));
    app.event(Event::Paste("bad-paste".into()));
    assert_eq!(
        app.form.as_ref().unwrap().fields[0].lines().join(""),
        "openai-chat"
    );

    // Cycle Field 0: openai-chat -> openai-responses -> anthropic (sets 8192 & syncs credential).
    app.event(key(KeyCode::Right));
    assert_eq!(
        app.form.as_ref().unwrap().fields[0].lines().join(""),
        "openai-responses"
    );
    assert_eq!(
        app.form.as_ref().unwrap().fields[3].lines().join(""),
        "openai-responses"
    );
    app.event(key(KeyCode::Char(' ')));
    assert_eq!(
        app.form.as_ref().unwrap().fields[0].lines().join(""),
        "anthropic"
    );
    assert_eq!(
        app.form.as_ref().unwrap().fields[3].lines().join(""),
        "anthropic"
    );
    assert_eq!(app.form.as_ref().unwrap().generation.0, Some(8192));
    assert!(app.form.as_ref().unwrap().auto_max_tokens);

    // Cycle back to openai-chat restores generation.0 = None.
    app.event(key(KeyCode::Right));
    assert_eq!(
        app.form.as_ref().unwrap().fields[0].lines().join(""),
        "openai-chat"
    );
    assert_eq!(app.form.as_ref().unwrap().generation.0, None);

    // Also verify fields 7, 9, 10 ignore paste & typing, and cycle on Space/Left/Right.
    for (idx, expected_after_right) in [(7, "accept-edits"), (9, "off"), (10, "on")] {
        let f = app.form.as_mut().unwrap();
        f.selected = idx;
        let before = f.fields[idx].lines().join("");
        app.event(key(KeyCode::Char('z')));
        app.event(Event::Paste("nope".into()));
        assert_eq!(
            app.form.as_ref().unwrap().fields[idx].lines().join(""),
            before
        );
        app.event(key(KeyCode::Right));
        assert_eq!(
            app.form.as_ref().unwrap().fields[idx].lines().join(""),
            expected_after_right
        );
    }

    // 3. Live credential status badges & env:/store parsing without secret echo.
    let f = app.form.as_mut().unwrap();
    f.fields[1] = TextArea::from(["fixture-model"]);

    // Missing store entry.
    f.fields[3] = TextArea::from(["missing_cred"]);
    assert_eq!(
        f.definition().unwrap().credential,
        CredentialRef::Store("missing_cred".into())
    );
    let missing_view = screen(&app);
    assert!(missing_view.contains("[missing]"), "{missing_view}");

    // Present store entry (`work`).
    app.form.as_mut().unwrap().fields[3] = TextArea::from(["work"]);
    let stored_buf = render_buf(&app, 100, 50);
    let stored_view: String = stored_buf.content().iter().map(|c| c.symbol()).collect();
    assert!(stored_view.contains("[stored]"), "{stored_view}");
    assert!(!stored_view.contains("sk-hidden-badge-secret-777"));
    let stored_check = stored_buf
        .content()
        .iter()
        .find(|c| c.symbol() == "✓")
        .expect("green check for stored");
    assert_eq!(stored_check.fg, Color::Green);

    // `env:PATH` -> `[env set]`, `env:TARS_MISSING_ENV_XYZ` -> `[env unset]`.
    app.form.as_mut().unwrap().fields[3] = TextArea::from(["env:PATH"]);
    assert_eq!(
        app.form.as_ref().unwrap().definition().unwrap().credential,
        CredentialRef::Environment("PATH".into())
    );
    let env_set_view = screen(&app);
    assert!(env_set_view.contains("[env set]"), "{env_set_view}");

    app.form.as_mut().unwrap().fields[3] = TextArea::from(["env:TARS_MISSING_ENV_XYZ"]);
    let env_unset_view = screen(&app);
    assert!(env_unset_view.contains("[env unset]"), "{env_unset_view}");

    // Insecure 0644 permissions on credentials.toml -> `[store error]` without leaking contents.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        app.form.as_mut().unwrap().fields[3] = TextArea::from(["work"]);
        app.refresh_cached_credential_names();
        until(&mut app, |a| {
            matches!(a.credential_store_cache, CredentialStoreCache::Error)
        })
        .await;
        let err_view = screen(&app);
        assert!(err_view.contains("[store error]"), "{err_view}");
        assert!(!err_view.contains("sk-hidden-badge-secret-777"));
    }
}
