use super::support::*;
use crate::{
    app::{
        App, COMMANDS, CONFIRM_GUARD_WINDOW, Panel,
        jobs::{JobKind, Message},
        matching_commands,
        provider::{CredentialStoreCache, ModelFetchStatus, ProviderFieldKind, ProviderWizardStep},
    },
    options::{Options, StartupAction},
    render,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tars_core::{
    application::ClosePolicy,
    config::{Config, ConfigSource, CredentialsStore, PROVIDER_PRESETS},
    definition::{CredentialRef, Provider},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
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
        "Disabled under --no-config; omit --no-config or pass --config FILE"
    );
    assert!(!dir.path().join("config.toml").exists());
    assert!(!dir.path().join("credentials.toml").exists());

    // Empty `/model` under `--no-config` shows disabled notice instead of opening wizard.
    run_command(&mut app, "model");
    assert!(app.panel.is_none());
    assert_eq!(
        app.notice,
        "No model profiles configured (--no-config); omit --no-config or pass --config FILE"
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
    assert!(
        step1_view.contains("Add model provider (1/3"),
        "{step1_view}"
    );
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
        assert_eq!(wizard.suggested_model.as_deref(), Some("qwen3:8b"));
        assert_eq!(wizard.api_key, "ollama");
        assert!(wizard.ollama_default_hint);
        assert_eq!(
            wizard.fields(),
            vec![ProviderFieldKind::ProfileName, ProviderFieldKind::ApiKey]
        );
        assert_eq!(wizard.active_field, 0);
        assert_eq!(wizard.current_field(), Some(ProviderFieldKind::ProfileName));
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

    // Select OpenAI (index 0): initial focus is ApiKey (index 1), paste API key, verify masking & zero plaintext in view.
    for _ in 0..6 {
        app.event(key(KeyCode::Char('k')));
    }
    app.event(key(KeyCode::Enter));
    let secret = "sk-paste-never-echo-98765";
    app.event(Event::Paste(format!("{secret}\r\n\x1b")));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.active_field, 1);
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

    // Open `/provider`, choose Custom (index 7), configure protocol & fields in Step 2.
    run_command(&mut app, "provider");
    for _ in 0..7 {
        app.event(key(KeyCode::Down));
    }
    app.event(key(KeyCode::Enter));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(wizard.is_custom);
        assert_eq!(
            wizard.fields(),
            vec![
                ProviderFieldKind::Protocol,
                ProviderFieldKind::Endpoint,
                ProviderFieldKind::ProfileName,
                ProviderFieldKind::ApiKey,
                ProviderFieldKind::MaxTokens,
            ]
        );
        assert_eq!(wizard.active_field, 0);
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

    // Enter custom endpoint, profile name, and API key via paste.
    app.event(key(KeyCode::Tab)); // Endpoint
    app.event(Event::Paste(server.endpoint.clone()));
    app.event(key(KeyCode::Tab)); // ProfileName ("custom" -> append "-gw")
    app.event(Event::Paste("-gw".into()));
    app.event(key(KeyCode::Tab)); // ApiKey
    let secret = "sk-custom-store-secret-4242";
    app.event(Event::Paste(secret.into()));

    // Ctrl+S on Custom with no model selected yet advances to Step 3 (SelectModels) instead of saving empty model.
    app.event(ctrl('s'));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::SelectModels);
        assert_eq!(wizard.api_key, secret);
    } else {
        panic!("expected Step 3 SelectModels");
    }
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if !matches!(w.model_fetch_status, ModelFetchStatus::Fetching)),
    )
    .await;

    // Type custom model in Step 3 and press Enter to save.
    app.event(Event::Paste("custom-model-1".into()));
    app.event(key(KeyCode::Enter));
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

    // 2. credentials.toml exists with 0600 permissions and secret; config.toml has NO plaintext secret and omits `models` for single model.
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
        !config_raw.contains("models ="),
        "single-model profile must omit models key: {config_raw}"
    );
    assert!(
        !config_raw.contains(secret),
        "config.toml leaked plaintext secret!"
    );

    // 3. Switching via `/model` works using the hot-reloaded `credentials_path`, and closing old session works.
    run_command(&mut app, "model");
    let models_view = screen(&app);
    assert!(
        models_view.contains("custom-gw · custom-model-1"),
        "{models_view}"
    );
    assert!(
        models_view.contains("+ Add / configure model provider (/provider)"),
        "{models_view}"
    );
    assert!(!models_view.contains("Custom settings (/new)..."));
    // Select `+ Add / configure model provider (/provider)` (index 1) from Panel::Models jumps to Panel::Provider.
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
    untrusted.event(ctrl('s')); // quick-save preset with suggested_model
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
    trusted.event(ctrl('s'));
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
        sym_app.event(ctrl('s'));
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

struct ModelsDiscoveryServer {
    endpoint: String,
    auth_headers: Arc<Mutex<Vec<Option<String>>>>,
    mode: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl ModelsDiscoveryServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let auth_headers = Arc::new(Mutex::new(Vec::new()));
        let mode = Arc::new(AtomicUsize::new(0));
        let auth_clone = Arc::clone(&auth_headers);
        let mode_clone = Arc::clone(&mode);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let auth_clone = Arc::clone(&auth_clone);
                let mode_clone = Arc::clone(&mode_clone);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(n) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&buf);
                    let auth = req.lines().find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_owned())
                    });
                    auth_clone.lock().unwrap().push(auth);

                    let current_mode = mode_clone.load(Ordering::SeqCst);
                    let (status, body) = if current_mode == 1 {
                        (
                            "500 Internal Server Error",
                            serde_json::json!({"error": {"message": "upstream\x1b[31m down"}})
                                .to_string(),
                        )
                    } else {
                        (
                            "200 OK",
                            serde_json::json!({
                                "data": [
                                    {"id": "gpt-5.4", "display_name": "GPT 5.4\x1b[31m", "owned_by": "openai"},
                                    {"id": "gpt-5-mini", "owned_by": "openai"},
                                    {"id": "gpt-4o", "owned_by": "openai"}
                                ]
                            })
                            .to_string(),
                        )
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        Self {
            endpoint,
            auth_headers,
            mode,
            task,
        }
    }
}

impl Drop for ModelsDiscoveryServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d2_provider_3step_models_discovery_multiselect_fallback_reconfigure_and_guards() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let creds_path = dir.path().join("credentials.toml");
    let server = ModelsDiscoveryServer::start().await;

    let mut app = app_with_config(dir.path(), None);

    // 1. Custom provider -> Enter in Step 2 -> Step 3 fetches `/models`, shows spinner, handles stale generation & JobPanicked.
    run_command(&mut app, "provider");
    for _ in 0..7 {
        app.event(key(KeyCode::Down));
    }
    app.event(key(KeyCode::Enter));
    app.event(key(KeyCode::Tab)); // Endpoint
    app.event(Event::Paste(server.endpoint.clone()));
    app.event(key(KeyCode::Tab)); // ProfileName
    app.event(Event::Paste("-multi".into()));
    app.event(key(KeyCode::Tab)); // ApiKey
    let secret = "sk-discovery-secret-111";
    app.event(Event::Paste(secret.into()));
    app.event(key(KeyCode::Enter));

    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::SelectModels);
        assert_eq!(wizard.model_fetch_status, ModelFetchStatus::Fetching);
    } else {
        panic!("expected Step 3 SelectModels");
    }
    let fetching_view = screen(&app);
    assert!(
        fetching_view.contains("Fetching models…"),
        "{fetching_view}"
    );

    // Stale fetch result (wrong generation) must be ignored.
    let current_gen = match &app.panel {
        Some(Panel::Provider(w)) => w.fetch_generation,
        _ => unreachable!(),
    };
    app.complete(Message::ProviderModelsFetched {
        generation: current_gen.wrapping_sub(1),
        result: Err("stale error must be ignored".into()),
    });
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.model_fetch_status, ModelFetchStatus::Fetching);
    }

    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;
    assert_eq!(
        server
            .auth_headers
            .lock()
            .unwrap()
            .last()
            .cloned()
            .flatten(),
        Some(format!("Bearer {secret}"))
    );

    // Esc from Step 3 -> Step 2 preserves entered fields & api_key; re-entering Step 3 bumps generation.
    app.event(key(KeyCode::Esc));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::Configure);
        assert_eq!(wizard.api_key, secret);
        assert_eq!(wizard.endpoint, server.endpoint);
    } else {
        panic!("expected Step 2");
    }
    app.event(key(KeyCode::Enter));
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;

    // Untrusted ANSI escape in display_name is sanitized in Step 3 render.
    let loaded_view = screen(&app);
    assert!(!loaded_view.contains('\x1b'));
    assert!(loaded_view.contains("gpt-5.4"), "{loaded_view}");
    assert!(loaded_view.contains("gpt-5-mini"), "{loaded_view}");

    // Live filter + Backspace + Space multi-select preserving check order: `gpt-5-mini` first, then `gpt-5.4`.
    app.event(Event::Paste("mini\r\n".into()));
    assert_eq!(
        match &app.panel {
            Some(Panel::Provider(w)) => w.visible_model_candidates().len(),
            _ => 0,
        },
        2 // `gpt-5-mini` + `+ Use custom model "mini"`
    );
    app.event(key(KeyCode::Char(' '))); // checks `gpt-5-mini`
    for _ in 0..4 {
        app.event(key(KeyCode::Backspace));
    }
    app.event(key(KeyCode::Char(' '))); // checks `gpt-5.4` (cursor reset to 0)
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.selected_models, vec!["gpt-5-mini", "gpt-5.4"]);
    }
    // Compact 30x12 renders Step 3 cleanly.
    let compact_step3 = screen_sized(&app, 30, 12);
    assert!(compact_step3.contains("Select models"), "{compact_step3}");

    // Confirm -> saves `model = "gpt-5-mini"` and `models = ["gpt-5-mini", "gpt-5.4"]`.
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    let config_raw = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        config_raw.contains("model = \"gpt-5-mini\""),
        "{config_raw}"
    );
    assert!(
        config_raw.contains("models = [\"gpt-5-mini\", \"gpt-5.4\"]"),
        "{config_raw}"
    );
    let profiles = app.options.config.model_profiles();
    assert_eq!(
        profiles.get("custom-multi").unwrap().available_models(),
        vec!["gpt-5-mini", "gpt-5.4"]
    );

    // 2. JobPanicked for JobKind::FetchProviderModels resets status cleanly without looking up entry.
    app.open_provider_wizard();
    app.event(key(KeyCode::Enter));
    app.event(Event::Paste("sk-panic-test".into()));
    app.event(key(KeyCode::Enter));
    app.complete(Message::JobPanicked(
        None,
        JobKind::FetchProviderModels,
        "boom\x1b[31m".into(),
    ));
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(matches!(
            wizard.model_fetch_status,
            ModelFetchStatus::Failed(_)
        ));
    }

    // 3. HTTP 500 failure + manual custom model fallback + Ctrl+R retry.
    server.mode.store(1, Ordering::SeqCst);
    app.event(key(KeyCode::Esc)); // Step 2
    app.event(key(KeyCode::Esc)); // Step 1
    for _ in 0..7 {
        app.event(key(KeyCode::Down));
    }
    app.event(key(KeyCode::Enter));
    app.event(key(KeyCode::Tab)); // Endpoint
    app.event(Event::Paste(server.endpoint.clone()));
    app.event(key(KeyCode::Tab)); // ProfileName
    app.event(Event::Paste("-offline".into()));
    app.event(key(KeyCode::Tab)); // ApiKey
    app.event(Event::Paste("sk-offline-key".into()));
    app.event(key(KeyCode::Enter));
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if matches!(w.model_fetch_status, ModelFetchStatus::Failed(_))),
    )
    .await;
    let failed_view = screen(&app);
    assert!(!failed_view.contains('\x1b'));
    assert!(
        failed_view.contains("Model discovery failed:"),
        "{failed_view}"
    );

    // Type custom model -> `+ Use custom model "offline-v2"` appears and can be confirmed with Enter.
    app.event(Event::Paste("offline-v2".into()));
    let custom_row_view = screen(&app);
    assert!(
        custom_row_view.contains("+ Use custom model \"offline-v2\""),
        "{custom_row_view}"
    );
    // Retry with Ctrl+R after restoring server 200.
    server.mode.store(0, Ordering::SeqCst);
    app.event(ctrl('r'));
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;
    // Clear query and re-type custom model, then toggle with Space and confirm.
    for _ in 0.."offline-v2".len() {
        app.event(key(KeyCode::Backspace));
    }
    app.event(Event::Paste("offline-v2".into()));
    let cands = match &app.panel {
        Some(Panel::Provider(w)) => w.visible_model_candidates(),
        _ => unreachable!(),
    };
    assert_eq!(cands.len(), 1);
    assert!(cands[0].is_custom_entry);
    app.event(key(KeyCode::Char(' '))); // checks offline-v2
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    assert_eq!(
        app.options
            .config
            .model_profiles()
            .get("custom-offline")
            .unwrap()
            .model,
        "offline-v2"
    );

    // 4. Reconfiguring existing preset with CredentialRef::Store reuses stored secret for `/models` and keeps `credential`.
    CredentialsStore::set(&creds_path, "openai", "sk-stored-reconfig-555").unwrap();
    let cfg_with_openai = format!(
        "version = 1\ndefault_profile = \"openai\"\n[profiles.openai]\nprovider = \"openai-responses\"\nmodel = \"gpt-5.4\"\nmodels = [\"gpt-5.4\", \"gpt-4o\"]\nendpoint = \"{}\"\ncredential = \"openai\"\n",
        server.endpoint
    );
    std::fs::write(&cfg_path, cfg_with_openai).unwrap();
    app.options.config = Arc::new(Config::load(ConfigSource::File(cfg_path.clone())).unwrap());
    app.credential_store_cache = CredentialStoreCache::Loaded(vec!["openai".into()]);

    run_command(&mut app, "provider");
    app.event(key(KeyCode::Enter)); // Step 2 for OpenAI preset
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.step, ProviderWizardStep::Configure);
        assert!(wizard.api_key.is_empty());
        assert_eq!(wizard.endpoint, server.endpoint);
        assert_eq!(wizard.selected_models, vec!["gpt-5.4", "gpt-4o"]);
        assert_eq!(
            wizard.existing_credential,
            Some(CredentialRef::Store("openai".into()))
        );
    } else {
        panic!("expected Step 2");
    }
    let reconfig_view = screen(&app);
    assert!(
        reconfig_view.contains("[stored · leave blank to keep]"),
        "{reconfig_view}"
    );
    assert!(!reconfig_view.contains("sk-stored-reconfig-555"));

    let before_count = server.auth_headers.lock().unwrap().len();
    app.event(key(KeyCode::Enter)); // Advances to Step 3 reusing stored secret in background task!
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;
    assert_eq!(
        server
            .auth_headers
            .lock()
            .unwrap()
            .get(before_count)
            .cloned()
            .flatten(),
        Some("Bearer sk-stored-reconfig-555".to_owned())
    );
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    assert_eq!(
        CredentialsStore::load(&creds_path).unwrap().get("openai"),
        Some("sk-stored-reconfig-555")
    );

    // 5. Reconfiguring existing preset with CredentialRef::Environment("PATH"):
    //    allows `/models` fetch with blank key, refuses saving without a new key, and migrates to store once key entered.
    let cfg_env = format!(
        "version = 1\n[profiles.deepseek]\nprovider = \"openai-chat\"\nmodel = \"deepseek-chat\"\nendpoint = \"{}\"\napi_key_env = \"PATH\"\n",
        server.endpoint
    );
    std::fs::write(&cfg_path, cfg_env).unwrap();
    app.options.config = Arc::new(Config::load(ConfigSource::File(cfg_path.clone())).unwrap());

    run_command(&mut app, "provider");
    for _ in 0..3 {
        app.event(key(KeyCode::Down)); // deepseek
    }
    app.event(key(KeyCode::Enter));
    let env_view = screen(&app);
    assert!(
        env_view.contains("[env:PATH · enter key to migrate]"),
        "{env_view}"
    );
    app.event(key(KeyCode::Enter)); // Step 3 using env:PATH
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;
    app.event(key(KeyCode::Enter)); // Saving without new key is refused cleanly
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(wizard.error.contains("env:PATH"), "{}", wizard.error);
    } else {
        panic!("expected error staying on Step 3");
    }
    // Go back to Step 2, enter new key, save -> migrates to credentials.toml!
    app.event(key(KeyCode::Esc));
    app.event(Event::Paste("sk-migrated-from-env".into()));
    app.event(ctrl('s'));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    assert_eq!(
        CredentialsStore::load(&creds_path).unwrap().get("deepseek"),
        Some("sk-migrated-from-env")
    );
    assert_eq!(
        app.options
            .config
            .model_profiles()
            .get("deepseek")
            .unwrap()
            .credential,
        CredentialRef::Store("deepseek".into())
    );

    // 6. Reconfiguring existing preset whose credential is `CredentialRef::Store("shared_cred")`
    //    (distinct from `profile_name`): leaving `api_key` blank reuses `"shared_cred"` for `/models`
    //    and preserves `credential = "shared_cred"` on save without writing a duplicate store entry.
    CredentialsStore::set(&creds_path, "shared_cred", "sk-shared-cred-999").unwrap();
    let cfg_shared = format!(
        "version = 1\n[profiles.openai]\nprovider = \"openai-responses\"\nmodel = \"gpt-5.4\"\nendpoint = \"{}\"\ncredential = \"shared_cred\"\n",
        server.endpoint
    );
    std::fs::write(&cfg_path, cfg_shared).unwrap();
    app.options.config = Arc::new(Config::load(ConfigSource::File(cfg_path.clone())).unwrap());
    app.credential_store_cache = CredentialStoreCache::Loaded(vec!["shared_cred".into()]);

    run_command(&mut app, "provider");
    app.event(key(KeyCode::Enter)); // Step 2 for OpenAI preset
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(wizard.api_key.is_empty());
        assert_eq!(
            wizard.existing_credential,
            Some(CredentialRef::Store("shared_cred".into()))
        );
    } else {
        panic!("expected Step 2");
    }
    let shared_view = screen(&app);
    assert!(
        shared_view.contains("[stored · leave blank to keep]"),
        "{shared_view}"
    );
    assert!(!shared_view.contains("sk-shared-cred-999"));

    let before_shared = server.auth_headers.lock().unwrap().len();
    app.event(key(KeyCode::Enter)); // Step 3 fetches `/models` with `shared_cred` secret
    until(
        &mut app,
        |a| matches!(&a.panel, Some(Panel::Provider(w)) if w.model_fetch_status == ModelFetchStatus::Loaded),
    )
    .await;
    assert_eq!(
        server
            .auth_headers
            .lock()
            .unwrap()
            .get(before_shared)
            .cloned()
            .flatten(),
        Some("Bearer sk-shared-cred-999".to_owned())
    );
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.panel.is_none() && a.jobs.is_empty()).await;
    assert_eq!(
        app.options
            .config
            .model_profiles()
            .get("openai")
            .unwrap()
            .credential,
        CredentialRef::Store("shared_cred".into())
    );
    let saved_cfg = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        saved_cfg.contains("credential = \"shared_cred\""),
        "{saved_cfg}"
    );
    assert_eq!(
        CredentialsStore::load(&creds_path)
            .unwrap()
            .get("shared_cred"),
        Some("sk-shared-cred-999")
    );
}
