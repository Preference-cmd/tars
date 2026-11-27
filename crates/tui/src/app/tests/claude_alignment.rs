use super::support::{app, history_definition, key, until};
use crate::{
    app::{App, Clock, Panel},
    options::{self, StartupAction},
    render,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use std::path::{Path, PathBuf};
use tars_core::{
    application::{ClosePolicy, PreparedCreate},
    config::default_tools_for_sandbox,
};

fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn run_command(app: &mut App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for ch in text.chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
}

fn write_profiles_config(dir: &Path, ws: &Path, unset_cred: bool) -> PathBuf {
    let cfg_path = dir.join("config.toml");
    let cred = if unset_cred {
        "TARS_DEFINITELY_MISSING_CREDENTIAL_FOR_TEST"
    } else {
        "PATH"
    };
    std::fs::write(
        &cfg_path,
        format!(
            "version = 1\n\
             store = '{}'\n\
             default_profile = 'default'\n\
             [profiles.default]\n\
             provider = 'openai-chat'\n\
             model = 'default-model'\n\
             api_key_env = '{cred}'\n\
             [profiles.fast]\n\
             provider = 'openai-responses'\n\
             model = 'fast-model'\n\
             api_key_env = 'PATH'\n\
             workspace_root = '{}'\n\
             tools = ['read_file']\n",
            dir.join("store").display(),
            ws.display()
        ),
    )
    .unwrap();
    cfg_path
}

fn app_from_args(args: &[&str]) -> App {
    let mut opts = options::parse(args.iter().map(|s| (*s).to_owned()))
        .unwrap()
        .unwrap();
    // Existing CLI-flag alignment tests exercise post-trust startup/session flows;
    // folder-trust gating is tested explicitly with `check_folder_trust = true`.
    opts.check_folder_trust = false;
    let mut app = App::new(opts);
    app.clock = Clock::manual(std::time::Instant::now());
    app
}

async fn seed_session(
    host: &tars_core::application::SessionHost,
    model: &str,
    workspace_root: Option<PathBuf>,
    title: Option<&str>,
) -> String {
    let mut def = history_definition(None);
    def.model = model.to_owned();
    def.workspace_root = workspace_root;
    let prepared = PreparedCreate::prepare_persistent(def, None).unwrap();
    let id = prepared.conversation_id().as_str().to_owned();
    let session = host.create(prepared).await.unwrap();
    let target = session.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();
    if let Some(title) = title {
        host.rename(id.clone().into(), Some(title.to_owned()))
            .await
            .unwrap();
    }
    id
}

#[test]
fn options_parse_supports_continue_resume_workspace_and_rejects_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // -c and --continue
    for flag in ["-c", "--continue"] {
        let opts = options::parse(
            ["--no-config", "--store", store.to_str().unwrap(), flag]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(opts.startup_action, StartupAction::ContinueLatest);
    }

    // -r / --resume without selector or followed by another flag -> OpenSessionPicker
    for args in [
        vec!["--no-config", "--store", store.to_str().unwrap(), "-r"],
        vec![
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--resume",
        ],
        vec!["-r", "--no-config", "--store", store.to_str().unwrap()],
        vec![
            "--resume",
            "--store",
            store.to_str().unwrap(),
            "--no-config",
        ],
    ] {
        let opts = options::parse(args.into_iter().map(str::to_owned))
            .unwrap()
            .unwrap();
        assert_eq!(opts.startup_action, StartupAction::OpenSessionPicker);
    }

    // -r <prefix> / --resume <prefix> + --workspace DIR
    for flag in ["-r", "--resume"] {
        let opts = options::parse(
            [
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                flag,
                "ab12cd",
                "--workspace",
                ws.to_str().unwrap(),
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            opts.startup_action,
            StartupAction::Resume("ab12cd".to_owned())
        );
        assert_eq!(opts.workspace.as_deref(), Some(ws.as_path()));
    }

    // Conflicts and duplicates
    for (args, expected) in [
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "-c",
                "-r",
            ],
            "mutually exclusive",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--continue",
                "--resume",
                "abc",
            ],
            "mutually exclusive",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "-c",
                "-c",
            ],
            "-c specified twice",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--continue",
                "--continue",
            ],
            "--continue specified twice",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "-c",
                "--continue",
            ],
            "mutually exclusive",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "-r",
                "-r",
            ],
            "-r specified twice",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--resume",
                "a",
                "--resume",
                "b",
            ],
            "--resume specified twice",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "-r",
                "--resume",
            ],
            "mutually exclusive",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
            ],
            "--workspace specified twice",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--workspace",
            ],
            "--workspace requires a value",
        ),
        (
            vec![
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--workspace",
                "--store",
            ],
            "--workspace requires a value",
        ),
        (
            vec!["--no-config", "--store", store.to_str().unwrap(), "-r", ""],
            "-r selector must not be empty",
        ),
    ] {
        let err = options::parse(args.into_iter().map(str::to_owned))
            .err()
            .expect("expected parse error");
        assert!(err.contains(expected), "expected '{expected}' in '{err}'");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc02_default_startup_auto_creates_workspace_bound_session_and_f5_never_retriggers() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("project");
    std::fs::create_dir_all(&ws).unwrap();
    let cfg_path = write_profiles_config(dir.path(), &ws, false);

    let mut app = app_from_args(&[
        "--config",
        cfg_path.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ]);
    app.sandbox = Some(crate::app::startup::SandboxCheck {
        available: true,
        detail: "sandbox ok".into(),
    });

    app.list();
    until(&mut app, |a| {
        !a.listing && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;

    assert!(!app.startup_action_pending);
    let def = app.entries[0].session.as_ref().unwrap().definition();
    let expected_ws = ws.canonicalize().unwrap();
    assert_eq!(def.model, "default-model");
    assert_eq!(def.workspace_root.as_ref(), Some(&expected_ws));
    assert_eq!(def.tools, default_tools_for_sandbox(true));
    assert_eq!(app.entries[0].workspace_root.as_ref(), Some(&expected_ws));

    // Pressing F5 refreshes list WITHOUT creating another session!
    app.event(key(KeyCode::F(5)));
    until(&mut app, |a| !a.listing).await;
    assert_eq!(app.entries.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc02_default_startup_missing_credential_degrades_gracefully_and_no_config_stays_empty() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("project");
    std::fs::create_dir_all(&ws).unwrap();

    // 1. Definition/preparation failure (e.g. missing `--workspace` path): no panic, clear notice.
    let valid_cfg = write_profiles_config(dir.path(), &ws, false);
    let missing_ws = dir.path().join("does-not-exist");
    let mut bad_ws_app = app_from_args(&[
        "--config",
        valid_cfg.to_str().unwrap(),
        "--workspace",
        missing_ws.to_str().unwrap(),
    ]);
    bad_ws_app.list();
    until(&mut bad_ws_app, |a| !a.listing).await;
    assert!(bad_ws_app.entries.is_empty());
    assert!(
        bad_ws_app
            .notice
            .contains("Could not start default session")
            && bad_ws_app.notice.contains("Run /doctor or /new"),
        "notice={}",
        bad_ws_app.notice
    );

    // 2. Missing credential on default preset: open fails gracefully without crashing.
    let cfg_path = write_profiles_config(dir.path(), &ws, true);
    let mut app = app_from_args(&[
        "--config",
        cfg_path.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ]);
    app.list();
    until(&mut app, |a| {
        !a.listing && a.entries.iter().all(|e| e.busy.is_none())
    })
    .await;
    assert_eq!(app.entries.len(), 1);
    assert!(app.entries[0].session.is_none());
    assert!(
        app.entries[0].notice.contains("Open failed"),
        "entry.notice={}",
        app.entries[0].notice
    );

    // 3. --no-config never auto-creates a session.
    let store = dir.path().join("isolated-store");
    let mut isolated = app_from_args(&["--no-config", "--store", store.to_str().unwrap()]);
    isolated.list();
    until(&mut isolated, |a| !a.listing).await;
    assert!(isolated.entries.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc02_continue_resume_and_picker_startup_actions_and_counterexamples() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // 1. `-c` on empty store gives clear notice and stays on main screen.
    let mut empty_continue =
        app_from_args(&["--no-config", "--store", store.to_str().unwrap(), "-c"]);
    empty_continue.list();
    until(&mut empty_continue, |a| !a.listing).await;
    assert_eq!(
        empty_continue.notice,
        "No saved sessions to continue; use /new to start"
    );
    assert!(empty_continue.entries.is_empty());

    // Seed two sessions with distinct timestamps / prefixes.
    let host = tars_core::application::SessionHost::new(Some(store.clone()));
    let id1 = seed_session(&host, "model-one", Some(ws.clone()), Some("first")).await;
    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    let id2 = seed_session(&host, "model-two", Some(ws.clone()), Some("latest-one")).await;

    // 2. `-c` opens the latest session (`id2`) and populates `workspace_root`, and F5 won't reopen.
    let mut app_c = app_from_args(&["--no-config", "--store", store.to_str().unwrap(), "-c"]);
    app_c.list();
    until(&mut app_c, |a| {
        !a.listing
            && a.entries
                .get(a.selected)
                .is_some_and(|e| e.session.is_some())
    })
    .await;
    assert_eq!(app_c.entries[app_c.selected].id(), id2);
    assert_eq!(
        app_c.entries[app_c.selected].workspace_root.as_ref(),
        Some(&ws)
    );
    let target = app_c.entries[app_c.selected]
        .session
        .as_ref()
        .unwrap()
        .target();
    app_c.host.close(&target, ClosePolicy::Save).await.unwrap();
    app_c.entries[app_c.selected].session = None;
    app_c.event(key(KeyCode::F(5)));
    until(&mut app_c, |a| !a.listing).await;
    assert!(app_c.entries[app_c.selected].session.is_none());

    // 3. `-r <unique-prefix>` opens matching session (`id1`).
    let unique_len = (1..=id1.len())
        .find(|&n| !id2.starts_with(&id1[..n]))
        .unwrap();
    let unique_prefix = &id1[..unique_len];
    let mut app_r_hit = app_from_args(&[
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "-r",
        unique_prefix,
    ]);
    app_r_hit.list();
    until(&mut app_r_hit, |a| {
        !a.listing
            && a.entries
                .get(a.selected)
                .is_some_and(|e| e.session.is_some())
    })
    .await;
    assert_eq!(app_r_hit.entries[app_r_hit.selected].id(), id1);
    let target = app_r_hit.entries[app_r_hit.selected]
        .session
        .as_ref()
        .unwrap()
        .target();
    app_r_hit
        .host
        .close(&target, ClosePolicy::Save)
        .await
        .unwrap();

    // 4. `-r <not-found>` sets notice and opens pre-filtered `Panel::Sessions`.
    let mut app_r_miss = app_from_args(&[
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "-r",
        "zzzz-no-match",
    ]);
    app_r_miss.list();
    until(&mut app_r_miss, |a| !a.listing).await;
    assert!(
        app_r_miss.notice.contains("session not found"),
        "{}",
        app_r_miss.notice
    );
    assert!(matches!(
        &app_r_miss.panel,
        Some(Panel::Sessions { query, selected: 0 }) if query == "zzzz-no-match"
    ));

    // 5. `-r <ambiguous>` (if common prefix exists, or shared prefix) -> notice + pre-filtered `Panel::Sessions`.
    let common_len = id1
        .bytes()
        .zip(id2.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    if common_len > 0 {
        let ambig_prefix = &id1[..common_len];
        let mut app_r_ambig = app_from_args(&[
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--resume",
            ambig_prefix,
        ]);
        app_r_ambig.list();
        until(&mut app_r_ambig, |a| !a.listing).await;
        assert!(
            app_r_ambig.notice.contains("ambiguous"),
            "{}",
            app_r_ambig.notice
        );
        assert!(matches!(
            &app_r_ambig.panel,
            Some(Panel::Sessions { query, selected: 0 }) if query == ambig_prefix
        ));
    }

    // 6. `-r` without selector opens `Panel::Sessions` with empty query.
    let mut app_r_picker =
        app_from_args(&["--no-config", "--store", store.to_str().unwrap(), "-r"]);
    app_r_picker.list();
    until(&mut app_r_picker, |a| !a.listing).await;
    assert!(matches!(
        &app_r_picker.panel,
        Some(Panel::Sessions { query, .. }) if query.is_empty()
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc03_clear_creates_fresh_session_and_preserves_history_and_errors_without_preset() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // Without preset (`--no-config`), `/clear` refuses cleanly.
    let mut no_preset = app(dir.path());
    run_command(&mut no_preset, "clear");
    assert_eq!(
        no_preset.notice,
        "No default model preset configured; use /new"
    );
    assert!(no_preset.entries.is_empty());

    // With preset, startup opens session #1, then `/clear` creates & opens session #2 inheriting workspace.
    let cfg_path = write_profiles_config(dir.path(), &ws, false);
    let mut app = app_from_args(&[
        "--config",
        cfg_path.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ]);
    app.list();
    until(&mut app, |a| {
        !a.listing && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    let first_id = app.entries[0].id().to_owned();
    let first_ws = app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .definition()
        .workspace_root;
    // Simulate viewing a child agent on session #1; `/clear` still creates a fresh root session.
    app.entries[0].viewed_agent_id = Some("child-agent".into());

    run_command(&mut app, "clear");
    until(&mut app, |a| {
        a.entries.len() == 2 && a.entries[1].session.is_some()
    })
    .await;
    assert_eq!(app.selected, 1);
    assert_ne!(app.entries[1].id(), first_id);
    assert!(app.entries[1].viewed_agent_id.is_none());
    assert_eq!(
        app.entries[1]
            .session
            .as_ref()
            .unwrap()
            .definition()
            .workspace_root,
        first_ws
    );
    // Previous session remains open and preserved in `entries`.
    assert!(app.entries[0].session.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc03_resume_command_refreshes_store_and_switches_or_opens_without_stale_cache() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let mut app = app_from_args(&["--no-config", "--store", store.to_str().unwrap()]);
    app.list();
    until(&mut app, |a| !a.listing).await;
    assert!(app.entries.is_empty());

    // `/resume` without args opens `Panel::Sessions`.
    run_command(&mut app, "resume");
    until(&mut app, |a| !a.listing).await;
    assert!(matches!(app.panel, Some(Panel::Sessions { .. })));
    app.event(key(KeyCode::Esc));

    // Create a brand-new session directly in the store AFTER initial startup list!
    let created_id =
        seed_session(&app.host, "fresh-model", Some(ws.clone()), Some("new-one")).await;
    let short = created_id[..8].to_owned();

    // `/resume <prefix>` triggers fresh list() and opens the newly discovered session.
    run_command(&mut app, &format!("resume {short}"));
    until(&mut app, |a| {
        !a.listing && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    assert_eq!(app.entries[0].id(), created_id);

    // Seed and open another session using a unique prefix, then `/resume <first-prefix>` switches without reopening.
    let second_id = seed_session(&app.host, "second-model", Some(ws), None).await;
    let second_unique_len = (1..=second_id.len())
        .find(|&n| !created_id.starts_with(&second_id[..n]))
        .unwrap();
    run_command(
        &mut app,
        &format!("resume {}", &second_id[..second_unique_len]),
    );
    until(&mut app, |a| {
        !a.listing && a.entries.len() == 2 && a.entries[1].session.is_some()
    })
    .await;
    assert_eq!(app.selected, 1);

    let first_unique_len = (1..=created_id.len())
        .find(|&n| !second_id.starts_with(&created_id[..n]))
        .unwrap();
    let first_unique = &created_id[..first_unique_len];
    let first_short = &created_id[..8];
    run_command(&mut app, &format!("resume {first_unique}"));
    until(&mut app, |a| !a.listing).await;
    assert_eq!(app.selected, 0);
    assert!(
        app.notice
            .contains(&format!("Switched to session {first_short}")),
        "{}",
        app.notice
    );

    // Counterexample: a failed list() clears `pending_resume_selector` so F5 never leaks it.
    app.pending_resume_selector = Some("never-leak".into());
    app.complete(crate::app::jobs::Message::Listed(
        0,
        Err(tars_core::application::ApplicationError::StoreUnavailable),
    ));
    assert!(app.pending_resume_selector.is_none());
    app.event(key(KeyCode::F(5)));
    until(&mut app, |a| !a.listing).await;
    assert!(app.panel.is_none());

    // Counterexample: no-arg `/resume` clears any stale `pending_resume_selector`
    // and modal/quitting guards block late `Message::Listed` from installing `Panel::Sessions`.
    app.pending_resume_selector = Some("stale".into());
    run_command(&mut app, "resume");
    assert!(app.pending_resume_selector.is_none());
    until(&mut app, |a| !a.listing).await;
    assert!(matches!(app.panel, Some(Panel::Sessions { .. })));
    app.event(key(KeyCode::Esc));

    app.pending_resume_selector = Some("missing-behind-modal".into());
    app.confirmation = Some(crate::app::Confirmation::Discard);
    app.complete(crate::app::jobs::Message::Listed(0, Ok(Vec::new())));
    assert!(app.pending_resume_selector.is_none());
    assert!(app.panel.is_none());
    app.confirmation = None;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc03_model_panel_direct_switch_tab_completion_and_definition_immutability() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    // Empty profiles -> helpful notice.
    let mut no_profiles = app(dir.path());
    run_command(&mut no_profiles, "model");
    assert_eq!(
        no_profiles.notice,
        "No model profiles configured; use /new or add [profiles.<name>] to config.toml"
    );

    let cfg_path = write_profiles_config(dir.path(), &ws, false);
    let mut app = app_from_args(&[
        "--config",
        cfg_path.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ]);
    app.list();
    until(&mut app, |a| {
        !a.listing && a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    let orig_def = app.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(orig_def.model, "default-model");

    // Tab completion for `/model fa` -> `/model fast`.
    app.event(Event::Paste("/model fa".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/model fast"]);
    app.entries[0].editor = crate::input::editor();

    // `/model` opens `Panel::Models` showing profiles, active marker, and Custom settings item.
    run_command(&mut app, "model");
    assert!(matches!(app.panel, Some(Panel::Models { .. })));
    let view = screen(&app);
    assert!(view.contains("Models  /"), "{view}");
    assert!(view.contains("default"), "{view}");
    assert!(view.contains("[active]"), "{view}");
    assert!(view.contains("fast"), "{view}");
    assert!(view.contains("fast-model"), "{view}");
    assert!(view.contains("Custom settings (/new)..."), "{view}");

    // Filtering narrows the list; selecting "Custom settings (/new)..." opens Form.
    app.event(Event::Paste("zzz-none".into()));
    app.event(key(KeyCode::Enter));
    assert!(app.panel.is_none());
    assert!(app.form.is_some());
    app.event(key(KeyCode::Esc));

    // Re-open `/model`, filter to `fast`, press Enter -> creates NEW session with `fast` profile!
    run_command(&mut app, "model");
    app.event(Event::Paste("fast".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.entries.len() == 2 && a.entries[1].session.is_some()
    })
    .await;

    assert_eq!(app.selected, 1);
    assert!(
        app.notice
            .contains("Switched to profile fast (fast-model) in new session"),
        "{}",
        app.notice
    );
    let fast_def = app.entries[1].session.as_ref().unwrap().definition();
    assert_eq!(fast_def.model, "fast-model");
    assert_eq!(fast_def.workspace_root, orig_def.workspace_root);
    // Original session's `SessionDefinition` was NOT mutated.
    assert_eq!(
        app.entries[0].session.as_ref().unwrap().definition(),
        orig_def
    );

    // Subsequent `/clear` inherits the newly chosen `fast` profile!
    run_command(&mut app, "clear");
    until(&mut app, |a| {
        a.entries.len() == 3 && a.entries[2].session.is_some()
    })
    .await;
    assert_eq!(
        app.entries[2].session.as_ref().unwrap().definition().model,
        "fast-model"
    );

    // Direct `/model default` switches back (default has NO explicit tools in config.toml),
    // and subsequent `/clear` must still inject default workspace tools.
    app.sandbox = Some(crate::app::startup::SandboxCheck {
        available: true,
        detail: "sandbox ok".into(),
    });
    run_command(&mut app, "model default");
    until(&mut app, |a| {
        a.entries.len() == 4 && a.entries[3].session.is_some()
    })
    .await;
    let switched_default_def = app.entries[3].session.as_ref().unwrap().definition();
    assert_eq!(switched_default_def.model, "default-model");
    assert_eq!(switched_default_def.tools, default_tools_for_sandbox(true));
    assert!(app.options.preset.as_ref().unwrap().tools.is_empty());

    // Flip sandbox availability before `/clear` to prove `/clear` re-derives dynamic tools
    // instead of reusing baked-in tools from `options.preset`.
    app.sandbox = Some(crate::app::startup::SandboxCheck {
        available: false,
        detail: "sandbox off".into(),
    });
    run_command(&mut app, "clear");
    until(&mut app, |a| {
        a.entries.len() == 5 && a.entries[4].session.is_some()
    })
    .await;
    let cleared_default_def = app.entries[4].session.as_ref().unwrap().definition();
    assert_eq!(cleared_default_def.model, "default-model");
    assert!(!cleared_default_def.tools.is_empty());
    assert_eq!(cleared_default_def.tools, default_tools_for_sandbox(false));

    run_command(&mut app, "model unknown-profile");
    assert!(
        app.notice.contains("Profile 'unknown-profile' not found"),
        "{}",
        app.notice
    );
    assert_eq!(app.entries.len(), 5);
}

#[test]
fn cc04_theme_uses_cyan_accent_and_ctrl_o_opens_tools_with_repeat_guard() {
    use crate::app::{InfoKind, state::Entry};
    use crate::render::theme;
    use causa::kernel::{
        BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload, ToolOutput,
        ToolResultPayload, ToolResultStatus,
    };
    use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::style::Color;
    use std::sync::Arc;

    assert_eq!(theme::ACCENT, Color::Cyan);
    assert_eq!(theme::MUTED, Color::DarkGray);
    assert_eq!(theme::OK, Color::Green);
    assert_eq!(theme::ERR, Color::Red);
    assert_eq!(theme::WARN, Color::Yellow);

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let call_id = causa::runtime::new_block_id();
    let ctx = Context::from_blocks(vec![
        ContextBlock::new(
            call_id,
            BlockContent::ToolCall(ToolCallPayload::new(
                "read_file",
                serde_json::json!({"path": "src/main.rs"}),
            )),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                call_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(
                    serde_json::json!({"path": "src/main.rs", "text": "fn main() {}\n"}),
                ),
            )),
            BlockMeta::default(),
        ),
    ])
    .unwrap();
    let mut entry = Entry::new("ctrl-o-sess".into(), "model".into());
    let transcript = Arc::new(crate::transcript::Transcript::from_materials(&ctx, &[]));
    entry.rows = Some(crate::transcript::Rows::wrap(transcript.clone(), 80));
    entry.transcript = transcript;
    app.entries.push(entry);

    let main_screen = screen(&app);
    assert!(
        main_screen.contains("▸ Read  src/main.rs · 1 line"),
        "{main_screen}"
    );
    assert!(
        !main_screen.contains(&call_id.0.to_string()),
        "{main_screen}"
    );

    // Holding Ctrl+O (Repeat) must never insert 'o' into the composer.
    app.event(Event::Key(KeyEvent::new_with_kind(
        KeyCode::Char('o'),
        KeyModifiers::CONTROL,
        KeyEventKind::Repeat,
    )));
    assert_eq!(app.entries[0].editor.lines(), &[""]);
    assert!(app.panel.is_none());

    // Pressing Ctrl+O opens `/tools` with full UUID and raw JSON.
    app.event(super::support::ctrl('o'));
    assert!(matches!(
        &app.panel,
        Some(Panel::Info {
            kind: InfoKind::Tools(_),
            ..
        })
    ));
    let tools_screen = screen(&app);
    assert!(
        tools_screen.contains(&call_id.0.to_string()),
        "{tools_screen}"
    );
    assert!(tools_screen.contains("fn main() {}"), "{tools_screen}");
    app.event(key(KeyCode::Esc));

    // `/help` documents Ctrl-O / F6.
    run_command(&mut app, "help");
    let help_screen = screen(&app);
    assert!(
        help_screen.contains("Ctrl-O / F6 / /tools"),
        "{help_screen}"
    );
}

#[test]
fn cc05_live_tool_view_spinner_clock_elapsed_freeze_and_colors() {
    use crate::app::tool_progress::ToolView;
    use crate::render::theme;
    use causa::kernel::{InvocationId, RoundId, ToolResultStatus, TurnId};
    use std::time::{Duration, Instant};
    use tars_core::permission::{
        PermissionDecision, PermissionDecisionKind, PermissionMode, RuleScope, RuleSource,
    };
    use tars_core::session::{ToolPhase, ToolProgress, ToolProgressSnapshot};

    let t0 = Instant::now();
    let inv = InvocationId {
        turn_id: TurnId::new("t1"),
        round_id: RoundId(0),
    };
    let read_id = causa::runtime::new_block_id();
    let bash_id = causa::runtime::new_block_id();
    let mut view = ToolView::default();

    let mut read_call = ToolProgress {
        invocation_id: inv.clone(),
        call_block_id: read_id,
        tool_name: "read_file".into(),
        arguments_summary: "{\"path\":\"src/lib.rs\",\"start_line\":1,\"max_lines\":10}".into(),
        phase: ToolPhase::Waiting,
        status: None,
        operation_not_started: false,
        permission_decision: None,
    };
    view.apply(&read_call, t0);

    let t1 = t0 + Duration::from_millis(1200);
    let lines_f0 = view.styled_lines(t1, 0);
    let lines_f1 = view.styled_lines(t1, 1);
    assert_eq!(
        lines_f0[0].spans[0].content,
        format!("{} ", theme::SPINNER_FRAMES[0])
    );
    assert_eq!(lines_f0[0].spans[0].style.fg, Some(theme::ACCENT));
    assert_eq!(
        lines_f1[0].spans[0].content,
        format!("{} ", theme::SPINNER_FRAMES[1])
    );
    let text_f0: String = lines_f0[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(text_f0.contains("Read  src/lib.rs:1-10"), "{text_f0}");
    assert!(text_f0.contains("Waiting"), "{text_f0}");
    assert!(text_f0.contains("1.2s"), "{text_f0}");
    assert!(!text_f0.contains(&read_id.0.to_string()), "{text_f0}");

    // Transitioning to Running preserves started_at (`t0`).
    read_call.phase = ToolPhase::Running;
    view.apply(&read_call, t1);
    let t2 = t0 + Duration::from_millis(2500);
    let running_text: String = view.styled_lines(t2, 12)[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        running_text.starts_with(theme::SPINNER_FRAMES[2]),
        "{running_text}"
    );
    assert!(running_text.contains("Running · 2.5s"), "{running_text}");

    // Committed freezes elapsed time at `t2` even when queried later at `t3`.
    read_call.phase = ToolPhase::Committed;
    read_call.status = Some(ToolResultStatus::Succeeded);
    view.apply(&read_call, t2);
    let t3 = t2 + Duration::from_secs(5);
    let committed_lines = view.styled_lines(t3, 0);
    assert_eq!(
        committed_lines[0].spans[0].content,
        format!("{} ", theme::TOOL_DONE_GLYPH)
    );
    assert_eq!(committed_lines[0].spans[0].style.fg, Some(theme::OK));
    let committed_text: String = committed_lines[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        committed_text.contains("2.5s"),
        "elapsed should freeze on commit: {committed_text}"
    );
    assert!(!committed_text.contains("7.5s"), "{committed_text}");

    // Failed tool with provenance renders ERR glyph/color + friendly Bash title.
    let bash_call = ToolProgress {
        invocation_id: inv,
        call_block_id: bash_id,
        tool_name: "run_command".into(),
        arguments_summary: "{\"command\":\"cargo test\"}".into(),
        phase: ToolPhase::Committed,
        status: Some(ToolResultStatus::Failed),
        operation_not_started: false,
        permission_decision: Some(PermissionDecision {
            mode: PermissionMode::Ask,
            revision: 1,
            kind: PermissionDecisionKind::AskedAndAllowed,
            rule: Some(RuleSource {
                scope: RuleScope::Session,
                prefix: vec!["cargo".into()],
            }),
        }),
    };
    view.reconcile(
        &ToolProgressSnapshot {
            calls: vec![read_call, bash_call],
            truncated: false,
        },
        t3,
    );
    let final_lines = view.styled_lines(t3 + Duration::from_secs(3), 0);
    assert_eq!(final_lines.len(), 2);
    assert_eq!(
        final_lines[1].spans[0].content,
        format!("{} ", theme::TOOL_ERR_GLYPH)
    );
    assert_eq!(final_lines[1].spans[0].style.fg, Some(theme::ERR));
    let bash_text: String = final_lines[1]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(bash_text.contains("Bash  cargo test"), "{bash_text}");
    assert!(
        bash_text.contains("asked by session rule `cargo`"),
        "{bash_text}"
    );
    assert!(!bash_text.contains(&bash_id.0.to_string()), "{bash_text}");
}

#[tokio::test]
async fn cc06_tick_animation_idle_zero_redraw_and_activity_strip_variants() {
    use crate::app::Entry;
    use crate::render::theme;
    use causa::kernel::{InvocationId, RoundId, TurnId};
    use std::sync::Arc;
    use std::time::Duration;
    use tars_core::session::{ProgressUpdate, ToolPhase, ToolProgress, WorkProgress, WorkRef};
    use tokio::sync::broadcast;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.entries.push(Entry::new("s1".into(), "model".into()));

    // Idle state: poll() never advances spinner_tick and never marks dirty.
    app.dirty = false;
    let initial_tick = app.spinner_tick;
    app.poll();
    app.poll();
    app.poll();
    assert_eq!(app.spinner_tick, initial_tick);
    assert!(!app.dirty);
    assert_eq!(render::activity_bar_height(&app), 0);

    // Active turn: spinner_tick advances every 2nd 50ms poll (100ms), marking dirty.
    let work = WorkRef {
        conversation_id: "s1".into(),
        turn_id: TurnId::new("t1"),
    };
    let (tx, rx) = broadcast::channel(32);
    app.entries[0].work = Some(work.clone());
    app.entries[0].turn_in_flight = true;
    app.entries[0].turn_started_at = Some(app.clock.now());
    app.entries[0].execution = "Running".into();
    app.entries[0].bind_progress(rx);

    app.dirty = false;
    app.poll();
    assert_eq!(app.spinner_tick, initial_tick);
    app.poll();
    assert_eq!(app.spinner_tick, initial_tick + 1);
    assert!(app.dirty);

    app.advance_clock(Duration::from_millis(1400));
    let rendered = screen(&app);
    assert!(rendered.contains("Generating · 1.4s"), "{rendered}");
    assert!(rendered.contains("Esc to interrupt"), "{rendered}");
    assert_eq!(render::activity_bar_height(&app), 1);

    // Reasoning before body text renders `◇ Thinking · Xs  ·  Esc to interrupt`.
    let inv = InvocationId {
        turn_id: work.turn_id.clone(),
        round_id: RoundId(0),
    };
    tx.send(Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: inv.clone(),
        update: ProgressUpdate::ModelStarted,
    }))
    .unwrap();
    tx.send(Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: inv.clone(),
        update: ProgressUpdate::Reasoning {
            delta: "analyzing".into(),
            truncated: false,
        },
    }))
    .unwrap();
    app.poll();
    let reasoning_screen = screen(&app);
    assert!(
        reasoning_screen.contains(&format!("{} Thinking · 1.4s", theme::THINKING_GLYPH)),
        "{reasoning_screen}"
    );

    // Single active running tool renders friendly name & target.
    let call_1 = ToolProgress {
        invocation_id: inv.clone(),
        call_block_id: causa::runtime::new_block_id(),
        tool_name: "run_command".into(),
        arguments_summary: "{\"command\":\"cargo test\"}".into(),
        phase: ToolPhase::Running,
        status: None,
        operation_not_started: false,
        permission_decision: None,
    };
    tx.send(Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: inv.clone(),
        update: ProgressUpdate::Tool(call_1),
    }))
    .unwrap();
    app.poll();
    let one_tool_screen = screen(&app);
    assert!(
        one_tool_screen.contains("Running Bash (cargo test) · 1.4s"),
        "{one_tool_screen}"
    );

    // Multiple active running tools render `Running 2 tools · Xs`.
    let call_2 = ToolProgress {
        invocation_id: inv.clone(),
        call_block_id: causa::runtime::new_block_id(),
        tool_name: "read_file".into(),
        arguments_summary: "{\"path\":\"src/lib.rs\"}".into(),
        phase: ToolPhase::Running,
        status: None,
        operation_not_started: false,
        permission_decision: None,
    };
    tx.send(Arc::new(WorkProgress {
        work,
        invocation_id: inv,
        update: ProgressUpdate::Tool(call_2),
    }))
    .unwrap();
    app.poll();
    let multi_tool_screen = screen(&app);
    assert!(
        multi_tool_screen.contains("Running 2 tools · 1.4s"),
        "{multi_tool_screen}"
    );

    // Clear turn; pure busy shows `compacting… · Xs`.
    app.entries[0].turn_in_flight = false;
    app.entries[0].turn_started_at = None;
    app.entries[0].clear_progress();
    app.entries[0].tools = Default::default();
    app.entries[0].execution = "Completed".into();
    let busy_t0 = app.clock.now();
    app.entries[0].set_busy("compacting", busy_t0);
    app.advance_clock(Duration::from_millis(500));
    let busy_screen = screen(&app);
    assert!(busy_screen.contains("compacting… · 0.5s"), "{busy_screen}");

    // Pure background task state omits bogus `0.0s`.
    app.entries[0].clear_busy();
    app.entries[0].active_bg_count = 2;
    let bg_screen = screen(&app);
    assert!(
        bg_screen.contains("2 background tasks running"),
        "{bg_screen}"
    );
    assert!(!bg_screen.contains("0.0s"), "{bg_screen}");

    // Missing `turn_started_at` while preview active also omits `0.0s`.
    app.entries[0].active_bg_count = 0;
    app.entries[0].preview.start(InvocationId {
        turn_id: TurnId::new("t2"),
        round_id: RoundId(0),
    });
    let no_ts_screen = screen(&app);
    assert!(no_ts_screen.contains("Generating"), "{no_ts_screen}");
    assert!(!no_ts_screen.contains("0.0s"), "{no_ts_screen}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc06_minimal_chrome_workspace_shortening_status_colors_and_minimum_sizes() {
    use super::support::{Server, create, ctrl};
    use crate::render::theme;
    use ratatui::style::{Color, Modifier};
    use tars_core::permission::{ApprovalSubject, PermissionMode};

    // 1. `format_workspace_label_with_home`: `~` substitution, tail-preserving ellipsis, sanitization.
    let home = Path::new("/Users/alice");
    assert_eq!(
        render::format_workspace_label_with_home(home, Some(home), 40),
        "~"
    );
    assert_eq!(
        render::format_workspace_label_with_home(
            Path::new("/Users/alice/tars-agent"),
            Some(home),
            40
        ),
        "~/tars-agent"
    );
    assert_eq!(
        render::format_workspace_label_with_home(
            Path::new("/Users/alice/work/projects/tars-agent"),
            Some(home),
            18
        ),
        "~/…/tars-agent"
    );
    assert_eq!(
        render::format_workspace_label_with_home(
            Path::new("/opt/builds/company/services/tars-agent"),
            None,
            15
        ),
        "…/tars-agent"
    );
    let sanitized =
        render::format_workspace_label_with_home(Path::new("/tmp/evil\x1b[31m\nrepo"), None, 40);
    assert!(!sanitized.contains('\x1b'));
    assert!(!sanitized.contains('\n'));
    assert!(sanitized.ends_with("repo"), "{sanitized}");
    assert!(!render::format_workspace_label(Path::new("/tmp/tars-agent"), 20).is_empty());

    // CJK/emoji workspace path shortening and span truncation respect display width and grapheme boundaries.
    use unicode_width::UnicodeWidthStr;
    let cjk_ws = render::format_workspace_label_with_home(
        Path::new("/Users/alice/工作/🚀项目/tars-代理"),
        Some(home),
        16,
    );
    assert!(cjk_ws.width() <= 16, "{cjk_ws}");
    assert!(cjk_ws.contains("tars-代理"), "{cjk_ws}");
    let tight_cjk = render::format_workspace_label_with_home(
        Path::new("/Users/alice/工作/🚀项目/tars-代理"),
        Some(home),
        6,
    );
    assert!(tight_cjk.width() <= 6, "{tight_cjk}");
    let truncated_spans = render::truncate_spans_display_width(
        vec![
            ratatui::text::Span::raw("你好👩‍💻"),
            ratatui::text::Span::raw(" · 🚀test"),
        ],
        7,
    );
    let joined: String = truncated_spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(joined.width() <= 7, "{joined}");
    assert!(joined.ends_with('…'), "{joined}");

    // 2. Layout height formulas.
    assert_eq!(render::header_height(12), 1);
    assert_eq!(render::header_height(14), 1);
    assert_eq!(render::header_height(15), 2);
    assert_eq!(render::footer_base_height(), 4);

    // 3. Header shows shortened workspace_root and 80-col status bar highlights Running & permission modes.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("deep/nested/workspace-alpha");
    std::fs::create_dir_all(&ws).unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    create(&mut app, &server.endpoint).await;
    app.entries[0].workspace_root = Some(ws.clone());
    app.entries[0].execution = "Running".into();
    app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();

    let mut term80 = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term80.draw(|f| render::draw(f, &app)).unwrap();
    let buf80 = term80.backend().buffer();
    let row0: String = (0..80)
        .map(|x| buf80.cell((x, 0)).unwrap().symbol())
        .collect();
    assert!(row0.contains("TARS v"), "{row0}");
    assert!(row0.contains("workspace-alpha"), "{row0}");
    assert!(row0.contains("Session "), "{row0}");

    // Locate `Running` and `accept-edits` cells in footer status row (row 21 = 24 - 3).
    let status_y = 21u16;
    let status_row: String = (0..80)
        .map(|x| buf80.cell((x, status_y)).unwrap().symbol())
        .collect();
    assert!(status_row.contains("Running"), "{status_row}");
    assert!(status_row.contains("mode accept-edits"), "{status_row}");
    let run_x = status_row.find("Running").unwrap() as u16;
    let run_cell = buf80.cell((run_x, status_y)).unwrap();
    assert_eq!(run_cell.fg, theme::ACCENT);
    assert!(run_cell.modifier.contains(Modifier::BOLD));
    let mode_x = status_row.find("accept-edits").unwrap() as u16;
    assert_eq!(buf80.cell((mode_x, status_y)).unwrap().fg, theme::ACCENT);

    // FullAccess highlights in LightRed + BOLD.
    app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();
    let mut term80_full = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term80_full.draw(|f| render::draw(f, &app)).unwrap();
    let buf_full = term80_full.backend().buffer();
    let status_row_full: String = (0..80)
        .map(|x| buf_full.cell((x, status_y)).unwrap().symbol())
        .collect();
    let full_x = status_row_full.find("full-access").unwrap() as u16;
    let full_cell = buf_full.cell((full_x, status_y)).unwrap();
    assert_eq!(full_cell.fg, Color::LightRed);
    assert!(full_cell.modifier.contains(Modifier::BOLD));

    // 4. Minimum size 30 x 12 with ALL optional bars + multi-line draft stays panic-free and conv >= 1.
    app.entries[0].turn_in_flight = true;
    app.entries[0].turn_started_at = Some(app.clock.now());
    app.entries[0].scroll = Some(0);
    app.entries[0]
        .queued_prompts
        .push_back(crate::app::QueuedPrompt::from("queued follow-up"));
    let img_path = dir.path().join("dot.png");
    std::fs::write(
        &img_path,
        [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
    .unwrap();
    let prepared_img = tars_core::media::prepare_image_from_path(&img_path).unwrap();
    app.entries[0].pending_images.push(prepared_img);
    app.entries[0].editor.insert_str("l1\nl2\nl3\nl4\nl5\nl6");
    app.event(Event::Resize(30, 12));
    assert!(render::conversation_height(12, 30, &app) >= 1);
    let mut term30 = Terminal::new(TestBackend::new(30, 12)).unwrap();
    term30.draw(|f| render::draw(f, &app)).unwrap();

    // 5. Minimum approval size 30 x 19 renders cleanly without panic.
    let (tx, _rx) = tokio::sync::oneshot::channel();
    app.entries[0]
        .approval_queue
        .push_back(crate::app::state::PendingApproval::mock(
            "edit_file".into(),
            PermissionMode::Ask,
            ApprovalSubject::FileWrite {
                path: "src/lib.rs".into(),
                action: tars_core::permission::WriteAction::Updated,
                diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
            },
            move |d| {
                let _ = tx.send(d);
            },
        ));
    app.event(Event::Resize(30, 19));
    let mut term30_19 = Terminal::new(TestBackend::new(30, 19)).unwrap();
    term30_19.draw(|f| render::draw(f, &app)).unwrap();

    app.entries[0].approval_queue.clear();
    app.event(ctrl('q'));
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc07_scroll_protection_unread_badge_and_tail_resume() {
    use super::support::Server;
    use crate::transcript::{Rows, Transcript};
    use causa::kernel::{InvocationId, RoundId, TurnId};
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::sync::Arc;
    use tars_core::session::{ProgressUpdate, WorkProgress, WorkRef};
    use tokio::sync::broadcast;

    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    super::support::create(&mut app, &server.endpoint).await;
    app.event(Event::Resize(80, 20));

    // 1. Streaming delta while scrolled up preserves scroll and marks unread output.
    let long_text: String = (0..80).map(|i| format!("committed row {i:02}\n")).collect();
    app.entries[0].transcript = Arc::new(Transcript {
        text: long_text,
        ..Transcript::default()
    });
    app.entries[0].rows = Some(Rows::wrap(
        app.entries[0].transcript.clone(),
        app.view_width,
    ));

    let work = WorkRef {
        conversation_id: app.entries[0].id().into(),
        turn_id: TurnId::new("turn-scroll-prot"),
    };
    let (tx, rx) = broadcast::channel(32);
    app.entries[0].work = Some(work.clone());
    app.entries[0].turn_in_flight = true;
    app.entries[0].turn_started_at = Some(app.clock.now());
    app.entries[0].execution = "Running".into();
    app.entries[0].bind_progress(rx);

    app.event(key(KeyCode::PageUp));
    let pinned_scroll = app.entries[0].scroll.expect("must be scrolled up");
    assert!(!app.entries[0].has_unread_output);

    tx.send(Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: InvocationId {
            turn_id: work.turn_id.clone(),
            round_id: RoundId(0),
        },
        update: ProgressUpdate::Text {
            delta: "new streaming output".into(),
            truncated: false,
        },
    }))
    .unwrap();
    app.poll();
    assert_eq!(app.entries[0].scroll, Some(pinned_scroll));
    assert!(app.entries[0].has_unread_output);
    let rendered = screen(&app);
    assert!(rendered.contains("↓ New output · End/PgDn"), "{rendered}");

    // Reset synthetic progress and run a real Server turn while scrolled up.
    app.entries[0].turn_in_flight = false;
    app.entries[0].turn_started_at = None;
    app.entries[0].work = None;
    app.entries[0].execution = "Completed".into();
    app.event(Event::Key(KeyEvent::new(
        KeyCode::End,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);

    // Seed enough real committed rows via multiple prompt turns? No, start a real turn,
    // wait for it to finish once to populate transcript, or submit a long multi-line prompt!
    let multi_line_prompt = (0..40)
        .map(|i| format!("prompt row {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.entries[0].editor.insert_str(&multi_line_prompt);
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

    // Scroll up while the real turn is still running on the server.
    app.event(key(KeyCode::PageUp));
    let real_pinned_scroll = app.entries[0].scroll.expect("must stay scrolled up");
    assert!(!app.entries[0].has_unread_output);

    // Release server -> real Message::Finished + Message::Materials run.
    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].execution == "Completed").await;
    assert_eq!(app.entries[0].scroll, Some(real_pinned_scroll));
    assert!(app.entries[0].has_unread_output);
    assert!(screen(&app).contains("↓ New output · End/PgDn"));

    // Jumping to bottom via Ctrl+End clears scroll and has_unread_output.
    app.event(Event::Key(KeyEvent::new(
        KeyCode::End,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);
    assert_eq!(render::scroll_bar_height(&app), 0);
    assert!(!screen(&app).contains("↓ New output"));

    // Scroll up again and submit a new prompt -> resets scroll and unread.
    app.event(key(KeyCode::PageUp));
    app.entries[0].has_unread_output = true;
    app.entries[0].editor.insert_str("next turn");
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);
    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].execution == "Completed").await;

    // Initial baseline Message::Materials (material_version == None) does not falsely mark unread.
    let session = app.entries[0].session.clone().unwrap();
    let baseline_view = crate::app::jobs::MaterialView::read(&session).unwrap();
    app.entries[0].material_version = None;
    app.entries[0].scroll = Some(0);
    app.entries[0].has_unread_output = false;
    app.complete(crate::app::jobs::Message::Materials(
        app.entries[0].key.clone(),
        Ok(baseline_view),
    ));
    assert!(!app.entries[0].has_unread_output);

    // Switching to a child agent clears `has_unread_output`.
    server.release.add_permits(2);
    let root_id = app.entries[0].id().to_owned();
    let tars_core::application::SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(tars_core::application::SpawnAgentRequest {
            caller_agent_id: root_id,
            caller_operation_id: None,
            source_block_id: None,
            name: "child".into(),
            preset_name: Some("explore".into()),
            role_prompt: None,
            prompt: "check".into(),
            narrowing: tars_core::definition::ChildNarrowingRequest::default(),
            interaction: tars_core::events::AgentInteraction::Reusable,
            execution_mode: tars_core::events::OperationExecutionMode::Background,
            on_parent_complete: tars_core::events::OnParentComplete::Continue,
            timeout_secs: None,
            max_rounds: None,
            coalesce_key: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected background spawn");
    };
    session.wait_settled().await.unwrap();
    app.entries[0].scroll = Some(0);
    app.entries[0].has_unread_output = true;
    app.switch_viewed_agent(&spawn.agent_id);
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);
}

#[test]
fn cc08_fine_grained_keyboard_scroll_and_editor_home_end_separation() {
    use crate::app::Entry;
    use crate::transcript::{Rows, Transcript};
    use crossterm::event::{KeyEvent, KeyModifiers};
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(80);
    app.view_height = 24;
    app.viewport_height = 15;

    let mut entry = Entry::new("scroll-keys".into(), "model".into());
    let text: String = (0..100).map(|i| format!("line-{i:03}\n")).collect();
    entry.transcript = Arc::new(Transcript {
        text,
        ..Transcript::default()
    });
    entry.rows = Some(Rows::wrap(entry.transcript.clone(), app.view_width));
    app.entries.push(entry);

    let end = app.entries[0].viewport(app.viewport_height).end;
    let page = app.entries[0].viewport(app.viewport_height).page;
    assert!(end > 20);

    // Alt+Up and Shift+Up step by 3 lines; Alt+Down and Shift+Down step down by 3 lines.
    app.event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));
    assert_eq!(app.entries[0].scroll, Some(end - 3));
    app.event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
    assert_eq!(app.entries[0].scroll, Some(end - 6));
    app.event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT)));
    assert_eq!(app.entries[0].scroll, Some(end - 3));
    app.event(Event::Key(KeyEvent::new(
        KeyCode::Down,
        KeyModifiers::SHIFT,
    )));
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);

    // Alt+U / Alt+D step by half page `(page / 2).max(1)`.
    let half = (page / 2).max(1);
    app.event(Event::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::ALT,
    )));
    assert_eq!(app.entries[0].scroll, Some(end - half));
    app.event(Event::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::ALT,
    )));
    assert_eq!(app.entries[0].scroll, None);

    // Ctrl+Home jumps to top Some(0); Ctrl+End jumps to bottom None.
    app.entries[0].has_unread_output = false;
    app.event(Event::Key(KeyEvent::new(
        KeyCode::Home,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.entries[0].scroll, Some(0));
    app.entries[0].has_unread_output = true;
    app.event(Event::Key(KeyEvent::new(
        KeyCode::End,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(app.entries[0].scroll, None);
    assert!(!app.entries[0].has_unread_output);

    // Plain Home / End when editor is EMPTY jumps to top / bottom.
    assert!(!app.entries[0].has_text_draft());
    app.event(key(KeyCode::Home));
    assert_eq!(app.entries[0].scroll, Some(0));
    app.event(key(KeyCode::End));
    assert_eq!(app.entries[0].scroll, None);

    // Plain Home / End when editor HAS text moves editor cursor without changing scroll.
    app.entries[0].scroll = Some(10);
    app.entries[0].editor.insert_str("hello");
    assert!(app.entries[0].has_text_draft());
    assert_eq!(app.entries[0].editor.cursor(), (0, 5));
    app.event(key(KeyCode::Home));
    assert_eq!(app.entries[0].editor.cursor(), (0, 0));
    assert_eq!(app.entries[0].scroll, Some(10));
    app.event(key(KeyCode::End));
    assert_eq!(app.entries[0].editor.cursor(), (0, 5));
    assert_eq!(app.entries[0].scroll, Some(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc06_cc08_stale_listed_after_delete_never_resurrects_session() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_profiles_config(dir.path(), dir.path(), false);
    let cfg_str = cfg_path.to_str().unwrap();
    let mut app = app_from_args(&["--config", cfg_str]);

    let keep_id = seed_session(&app.host, "keep-model", None, Some("Keep")).await;
    let del_id = seed_session(&app.host, "del-model", None, Some("DeleteMe")).await;

    // Capture a stale listing that still contains both sessions.
    let stale_listing = app.host.list().await.unwrap();
    assert_eq!(stale_listing.len(), 2);

    // Initial list populates both entries.
    app.startup_action_pending = false;
    app.complete(crate::app::jobs::Message::Listed(
        0,
        Ok(stale_listing.clone()),
    ));
    assert!(app.entries.iter().any(|e| e.id() == keep_id));
    assert!(app.entries.iter().any(|e| e.id() == del_id));

    // Delete `del_id` via App::delete_session.
    let gen_before_delete = app.list_generation;
    app.delete_session(del_id.clone());
    assert!(app.list_generation > gen_before_delete);
    until(&mut app, |a| a.entries.iter().all(|e| e.id() != del_id)).await;
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].id(), keep_id);

    // A stale in-flight Message::Listed from before the delete arrives late:
    // it MUST NOT resurrect `del_id`, and `/resume <del_id>` must not resolve it.
    app.pending_resume_selector = Some(del_id.clone());
    app.complete(crate::app::jobs::Message::Listed(
        gen_before_delete,
        Ok(stale_listing),
    ));
    assert!(
        app.entries.iter().all(|e| e.id() != del_id),
        "deleted session must never be resurrected by stale list()"
    );
    assert!(matches!(app.panel, Some(Panel::Sessions { .. })));
    app.panel = None;

    // A fresh list() spawned after delete succeeds and clears `recently_deleted`.
    app.list();
    until(&mut app, |a| !a.listing).await;
    assert!(app.recently_deleted.is_empty());
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].id(), keep_id);

    // Failed delete or panicked delete removes the ID from `recently_deleted`.
    app.recently_deleted.insert(keep_id.clone());
    app.complete(crate::app::jobs::Message::Deleted(
        keep_id.clone(),
        Err(tars_core::application::ApplicationError::StoreUnavailable),
    ));
    assert!(!app.recently_deleted.contains(&keep_id));

    app.recently_deleted.insert(keep_id.clone());
    app.complete(crate::app::jobs::Message::JobPanicked(
        Some(app.entries[0].key.clone()),
        crate::app::jobs::JobKind::Delete,
        "simulated panic".into(),
    ));
    assert!(!app.recently_deleted.contains(&keep_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cc09_copy_command_modes_child_agent_fallback_errors_and_tab_completion() {
    use super::support::{Server, create};
    use crate::app::{COMMANDS, Entry};
    use crate::input::slash_command_takes_args;
    use crate::terminal::{MAX_OSC52_RAW_BYTES, write_osc52_clipboard_with_tmux};
    use crate::transcript::{Transcript, TranscriptSegment};
    use causa::kernel::{BlockContent, BlockMeta, ContentPart, ContextBlock, Role, SealedPart};
    use std::sync::{Arc, Mutex};
    use tars_core::application::SpawnAgentRequest;
    use tars_core::definition::ChildNarrowingRequest;
    use tars_core::events::{AgentInteraction, OnParentComplete, OperationExecutionMode};

    assert!(COMMANDS.iter().any(|(name, _)| *name == "copy"));
    assert!(slash_command_takes_args("copy"));

    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());

    // Capture OSC 52 output via injectable clipboard sink without touching a real TTY.
    let captured_raw: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_wire: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let raw_clone = captured_raw.clone();
    let wire_clone = captured_wire.clone();
    app.clipboard_sink = Some(Box::new(move |bytes: &[u8]| {
        let mut wire = Vec::new();
        write_osc52_clipboard_with_tmux(&mut wire, bytes, false)?;
        raw_clone.lock().unwrap().push(bytes.to_vec());
        wire_clone.lock().unwrap().push(wire);
        Ok(())
    }));

    // 0. No entries yet -> clean error notices without writing clipboard.
    run_command(&mut app, "copy");
    assert!(app.notice.contains("No active session"), "{}", app.notice);
    assert!(captured_raw.lock().unwrap().is_empty());

    // Help panel lists /copy.
    run_command(&mut app, "help");
    match &app.panel {
        Some(Panel::Info { lines, .. }) => {
            assert!(
                lines.iter().any(|l| l.contains("/copy [code [N] | id]")),
                "{lines:?}"
            );
        }
        _ => panic!("expected help panel"),
    }
    app.panel = None;

    create(&mut app, &server.endpoint).await;
    let conv_id = app.entries[0].id().to_owned();

    // No assistant reply yet -> error notice.
    run_command(&mut app, "copy");
    assert!(
        app.entries[0].notice.contains("No assistant reply"),
        "{}",
        app.entries[0].notice
    );
    assert!(captured_raw.lock().unwrap().is_empty());

    // Populate materials:
    // 1) assistant message with 2 fenced code blocks (1 tagged rust, 1 untagged)
    // 2) assistant message with prose only (no code blocks)
    // 3) assistant message with ONLY ContentPart::Sealed (must be skipped!)
    let session = app.entries[0].session.clone().unwrap();
    let snap = session.read_materials().unwrap();
    let mut blocks = snap.context.blocks().to_vec();
    let reply_with_code = concat!(
        "Here are two snippets:\n\n",
        "```rust\n",
        "fn first() {\n",
        "    println!(\"one\");\n",
        "}\n",
        "```\n\n",
        "And an untagged block:\n\n",
        "```\n",
        "echo two\n",
        "```\n",
    );
    let reply_prose_only = "Final plain prose reply without any fenced code.";
    blocks.push(ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::Assistant, reply_with_code),
        BlockMeta::from_source("assistant"),
    ));
    blocks.push(ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::Assistant, reply_prose_only),
        BlockMeta::from_source("assistant"),
    ));
    blocks.push(ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![ContentPart::Sealed(SealedPart::new(
                "anthropic_messages",
                serde_json::json!({"thinking": "internal"}),
            ))],
        },
        BlockMeta::from_source("assistant"),
    ));
    session.edit(snap.version, blocks).unwrap();

    // 1. `/copy` copies the last non-empty assistant reply (skipping Sealed-only block) + OSC 52 framing.
    run_command(&mut app, "copy");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        reply_prose_only
    );
    let wire_bytes = captured_wire.lock().unwrap().pop().unwrap();
    assert!(wire_bytes.starts_with(b"\x1b]52;c;"));
    assert!(wire_bytes.ends_with(b"\x07"));
    assert!(
        app.entries[0].notice.contains(&format!(
            "Copied last reply ({} chars)",
            reply_prose_only.chars().count()
        )),
        "{}",
        app.entries[0].notice
    );

    // 2. `/copy code` walks backward from prose-only reply to reply_with_code and copies its latest block (#2).
    run_command(&mut app, "copy code");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        "echo two"
    );
    assert!(
        app.entries[0]
            .notice
            .contains("Copied code block #2 (1 lines, 8 chars) to clipboard via OSC 52"),
        "{}",
        app.entries[0].notice
    );

    // 3. `/copy code 1` copies the 1st fenced block (tagged `rust`) with language + line/char counts.
    run_command(&mut app, "copy code 1");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        "fn first() {\n    println!(\"one\");\n}"
    );
    assert!(
        app.entries[0]
            .notice
            .contains("Copied code block #1 (rust, 3 lines, 35 chars) to clipboard via OSC 52"),
        "{}",
        app.entries[0].notice
    );

    // 4. Counterexamples: `/copy code 0`, `/copy code 99`, `/copy foo`, `/copy code abc`.
    run_command(&mut app, "copy code 0");
    assert!(
        app.entries[0].notice.contains("#0 is invalid"),
        "{}",
        app.entries[0].notice
    );
    run_command(&mut app, "copy code 99");
    assert_eq!(
        app.entries[0].notice,
        "Code block #99 not found (2 available)"
    );
    run_command(&mut app, "copy foo");
    assert_eq!(app.entries[0].notice, "Usage: /copy [code [N] | id]");
    run_command(&mut app, "copy code abc");
    assert_eq!(app.entries[0].notice, "Usage: /copy [code [N] | id]");
    assert!(captured_raw.lock().unwrap().is_empty());

    // 5. `/copy id` copies full conversation_id even when session is unopened/closed.
    run_command(&mut app, "copy id");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        conv_id
    );
    assert!(
        app.entries[0]
            .notice
            .contains(&format!("Copied session ID {}", &conv_id[..8])),
        "{}",
        app.entries[0].notice
    );

    // 6. Read-only child-agent view copies child agent's assistant reply, not root's.
    server.release.add_permits(2);
    let tars_core::application::SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(SpawnAgentRequest {
            caller_agent_id: conv_id.clone(),
            caller_operation_id: None,
            source_block_id: None,
            name: "child-copy".into(),
            preset_name: Some("explore".into()),
            role_prompt: None,
            prompt: "inspect".into(),
            narrowing: ChildNarrowingRequest::default(),
            interaction: AgentInteraction::Reusable,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            timeout_secs: None,
            max_rounds: None,
            coalesce_key: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected background spawn");
    };
    session.wait_settled().await.unwrap();
    app.switch_viewed_agent(&spawn.agent_id);
    assert!(app.entries[0].viewed_agent_id.is_some());
    run_command(&mut app, "copy");
    let child_copied = String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap();
    assert_eq!(child_copied, "你好，terminal 👩‍💻");

    // 7. Transcript fallback (unopened entry): merges multi-segment assistant turn split by media badge.
    let mut closed_entry = Entry::new("closed-fallback-id-123456".into(), "fixture".into());
    closed_entry.transcript = Arc::new(Transcript {
        segments: vec![
            TranscriptSegment::UserHeader,
            TranscriptSegment::PlainBody("user prompt".into()),
            TranscriptSegment::RoleHeader("● assistant".into()),
            TranscriptSegment::MarkdownBody("Part one before media".into()),
            TranscriptSegment::PlainBody("[media image/png shot.png (media:sha256:0000)]".into()),
            TranscriptSegment::MarkdownBody("Part two:\n```sh\nls -la\n```".into()),
        ],
        ..Transcript::default()
    });
    app.entries.push(closed_entry);
    app.selected = app.entries.len() - 1;

    run_command(&mut app, "copy");
    let merged = String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap();
    assert!(merged.contains("Part one before media"), "{merged}");
    assert!(merged.contains("Part two:"), "{merged}");

    run_command(&mut app, "copy code");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        "ls -la"
    );

    run_command(&mut app, "copy id");
    assert_eq!(
        String::from_utf8(captured_raw.lock().unwrap().pop().unwrap()).unwrap(),
        "closed-fallback-id-123456"
    );

    // 8. Oversized payload (> 1 MiB) fails cleanly without writing clipboard.
    app.entries[app.selected].transcript = Arc::new(Transcript {
        segments: vec![
            TranscriptSegment::RoleHeader("● assistant".into()),
            TranscriptSegment::MarkdownBody("x".repeat(MAX_OSC52_RAW_BYTES + 1)),
        ],
        ..Transcript::default()
    });
    run_command(&mut app, "copy");
    assert!(
        app.entries[app.selected].notice.contains("1 MiB"),
        "{}",
        app.entries[app.selected].notice
    );
    assert!(captured_raw.lock().unwrap().is_empty());

    // 9. No fenced code block anywhere -> clear error notice.
    app.entries[app.selected].transcript = Arc::new(Transcript {
        segments: vec![
            TranscriptSegment::RoleHeader("● assistant".into()),
            TranscriptSegment::MarkdownBody("plain text only".into()),
        ],
        ..Transcript::default()
    });
    run_command(&mut app, "copy code");
    assert!(
        app.entries[app.selected]
            .notice
            .contains("No fenced code block"),
        "{}",
        app.entries[app.selected].notice
    );
    assert!(captured_raw.lock().unwrap().is_empty());

    // 10. Tab completion: `/cop` -> `/copy `, `/copy ` -> `Copy: code, id`, `/copy c` -> `/copy code`, `/copy i` -> `/copy id`.
    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("/cop".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/copy "]);

    app.event(key(KeyCode::Tab));
    assert!(
        app.entries[app.selected].notice.contains("code")
            && app.entries[app.selected].notice.contains("id"),
        "{}",
        app.entries[app.selected].notice
    );

    app.event(Event::Paste("c".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/copy code"]);

    app.entries[app.selected].editor = crate::input::editor();
    app.event(Event::Paste("/copy i".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[app.selected].editor.lines(), &["/copy id"]);

    // Tab completion inside Panel::Commands also completes `copy c` -> `copy code`.
    app.event(key(KeyCode::F(1)));
    for ch in "copy c".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Tab));
    match &app.panel {
        Some(Panel::Commands { query, .. }) => assert_eq!(query, "copy code"),
        _ => panic!("expected commands panel"),
    }
}

fn render_buffer(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn find_prompt_row(buf: &ratatui::buffer::Buffer) -> Option<(u16, u16)> {
    // Search bottom-up so the composer prompt `❯` is found even if a list row
    // above also has the highlight symbol `❯`.
    for y in (0..buf.area.height).rev() {
        for x in 0..buf.area.width {
            if buf[(x, y)].symbol() == "❯" {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn empty_entries_scratch_editor_accepts_typing_paste_completion_and_commands() {
    use crossterm::event::KeyModifiers;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    assert!(app.entries.is_empty());

    // 1. Typing directly into the composer with no entries populates scratch_editor.
    for ch in "hello".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.scratch_editor.lines(), &["hello"]);
    assert!(app.has_active_text_draft());

    // Typing `/` when scratch_editor already has text inserts `/` instead of opening Commands.
    app.event(key(KeyCode::Char('/')));
    assert!(app.panel.is_none());
    assert_eq!(app.scratch_editor.lines(), &["hello/"]);

    // Backspace, Home, End, and Ctrl-J multiline editing work on scratch_editor.
    app.event(key(KeyCode::Backspace));
    assert_eq!(app.scratch_editor.lines(), &["hello"]);
    app.event(key(KeyCode::Home));
    assert_eq!(app.scratch_editor.cursor(), (0, 0));
    app.event(key(KeyCode::End));
    assert_eq!(app.scratch_editor.cursor(), (0, 5));
    app.event(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
    )));
    app.event(Event::Paste("world".into()));
    assert_eq!(app.scratch_editor.lines(), &["hello", "world"]);

    // Rendered bottom composer displays the scratch draft.
    let buf = render_buffer(&app, 80, 24);
    let all: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
    assert!(all.contains("hello"), "{all}");
    assert!(all.contains("world"), "{all}");

    // Pressing Enter on plain text with no active session sets helpful notice.
    app.event(key(KeyCode::Enter));
    assert_eq!(
        app.notice,
        "No active session — type /new (or F2) to create one, or /sessions to resume"
    );
    assert_eq!(app.scratch_editor.lines(), &["hello", "world"]);

    // Ctrl-Q respects scratch draft in request_quit confirmation.
    app.event(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL,
    )));
    assert!(matches!(
        app.confirmation,
        Some(crate::app::Confirmation::Quit {
            has_drafts: true,
            ..
        })
    ));
    app.event(key(KeyCode::Char('n')));
    assert!(app.confirmation.is_none());

    // 2. Tab completion works in scratch_editor and clears stale `"Copy: "` notice.
    app.scratch_editor = crate::input::editor();
    app.event(Event::Paste("/cop".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.scratch_editor.lines(), &["/copy "]);
    app.event(key(KeyCode::Tab));
    assert!(app.notice.starts_with("Copy: "), "{}", app.notice);
    app.event(Event::Paste("c".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.scratch_editor.lines(), &["/copy code"]);
    assert!(
        app.notice.is_empty(),
        "stale notice not cleared: {}",
        app.notice
    );

    // 3. Typing `/help` directly into scratch_editor and pressing Enter executes and clears.
    app.scratch_editor = crate::input::editor();
    app.event(Event::Paste("/help".into()));
    app.event(key(KeyCode::Enter));
    assert!(matches!(
        &app.panel,
        Some(Panel::Info { title, .. }) if title == "Keyboard shortcuts"
    ));
    assert!(!app.scratch_has_text_draft());
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());

    // 4. Typing `/image` when entries is empty dispatches cleanly and clears scratch_editor.
    app.event(Event::Paste("/image".into()));
    app.event(key(KeyCode::Enter));
    assert!(!app.scratch_has_text_draft());
    assert_eq!(app.notice, "Open or select a session first");

    // 5. Typing `/new` into scratch_editor and pressing Enter opens the form and clears.
    app.event(Event::Paste("/new".into()));
    app.event(key(KeyCode::Enter));
    assert!(app.form.is_some());
    assert!(!app.scratch_has_text_draft());
    app.event(key(KeyCode::Esc));
    assert!(app.form.is_none());

    // 6. Unopened session entry: `/help` dispatches, plain text sets helpful notice.
    app.entries
        .push(crate::app::Entry::new("stored-only".into(), "model".into()));
    app.event(Event::Paste("/help".into()));
    app.event(key(KeyCode::Enter));
    assert!(matches!(
        &app.panel,
        Some(Panel::Info { title, .. }) if title == "Keyboard shortcuts"
    ));
    assert!(!app.entries[0].has_text_draft());
    app.event(key(KeyCode::Esc));

    app.event(Event::Paste("hello stored".into()));
    app.event(key(KeyCode::Enter));
    assert_eq!(
        app.entries[0].notice,
        "Session is not open — press /open (or F3) first, or /new to create one"
    );
}

#[tokio::test]
async fn slash_popup_keeps_input_box_fixed_shows_cursor_and_handles_backspace_and_tiny_terminal() {
    use ratatui::style::Modifier;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    // 1. Empty composer shows reversed cursor cell at column 0 before placeholder.
    let before_buf = render_buffer(&app, 80, 24);
    let (prompt_x, prompt_y) = find_prompt_row(&before_buf).expect("bottom composer prompt");
    let cursor_cell = &before_buf[(prompt_x + 2, prompt_y)];
    assert_eq!(cursor_cell.symbol(), " ");
    assert!(
        cursor_cell.modifier.contains(Modifier::REVERSED),
        "empty composer cursor cell must be reversed: {cursor_cell:?}"
    );
    assert!(row_text(&before_buf, prompt_y).contains("/new to start"));

    // 2. Pressing `/` keeps the bottom composer at the EXACT same `y` row, renders
    //    `❯ /` + cursor there, opens the floating popup immediately above it, and
    //    preserves the welcome content above the popup.
    app.event(key(KeyCode::Char('/')));
    assert!(matches!(
        &app.panel,
        Some(Panel::Commands { query, .. }) if query.is_empty()
    ));
    let slash_buf = render_buffer(&app, 80, 24);
    let (slash_x, slash_y) = find_prompt_row(&slash_buf).expect("composer prompt with /");
    assert_eq!(slash_x, prompt_x);
    assert_eq!(
        slash_y, prompt_y,
        "bottom input box must not lift when pressing `/`"
    );
    let slash_row = row_text(&slash_buf, slash_y);
    assert!(slash_row.contains("❯ /"), "{slash_row}");
    assert!(slash_row.contains("type a command"), "{slash_row}");
    assert_eq!(slash_buf[(slash_x + 2, slash_y)].symbol(), "/");
    assert!(
        slash_buf[(slash_x + 3, slash_y)]
            .modifier
            .contains(Modifier::REVERSED),
        "cursor after `/` must be reversed"
    );

    // Floating popup header is anchored above the input box's top border, never at row 2.
    let input_top_border_y = slash_y - 1;
    let popup_header_y = (0..input_top_border_y)
        .find(|&y| row_text(&slash_buf, y).contains("Commands"))
        .expect("Commands popup header above input box");
    assert!(
        popup_header_y > 2,
        "popup must float above composer rather than lifting to top row 2 (got y={popup_header_y})"
    );
    assert!(
        row_text(&slash_buf, input_top_border_y - 1).contains('/'),
        "bottom of popup must sit flush above input box"
    );
    let above_popup: String = (2..popup_header_y)
        .map(|y| row_text(&slash_buf, y))
        .collect();
    assert!(
        above_popup.contains("Welcome to TARS"),
        "welcome must stay visible above popup: {above_popup}"
    );

    // 3. Typing filters query, Backspace on non-empty pops one char and stays open,
    //    Backspace on empty closes Panel::Commands.
    app.event(key(KeyCode::Char('h')));
    app.event(key(KeyCode::Char('e')));
    assert!(matches!(
        &app.panel,
        Some(Panel::Commands { query, .. }) if query == "he"
    ));
    app.event(key(KeyCode::Backspace));
    assert!(matches!(
        &app.panel,
        Some(Panel::Commands { query, .. }) if query == "h"
    ));
    app.event(key(KeyCode::Backspace));
    assert!(matches!(
        &app.panel,
        Some(Panel::Commands { query, .. }) if query.is_empty()
    ));
    app.event(key(KeyCode::Backspace));
    assert!(
        app.panel.is_none(),
        "Backspace on empty Panel::Commands must close popup"
    );

    // Counterexample: Backspace on empty Panel::Sessions stays open.
    app.event(key(KeyCode::F(1)));
    for ch in "sessions".chars() {
        app.event(key(KeyCode::Char(ch)));
    }
    app.event(key(KeyCode::Enter));
    assert!(matches!(
        &app.panel,
        Some(Panel::Sessions { query, .. }) if query.is_empty()
    ));
    app.event(key(KeyCode::Backspace));
    assert!(matches!(app.panel, Some(Panel::Sessions { .. })));
    app.event(key(KeyCode::Esc));

    // 4. With an active entry having scroll/activity bars, `/` does not shift input `y`.
    let mut entry = crate::app::Entry::new("active-bars".into(), "model".into());
    entry.scroll = Some(0);
    entry.turn_in_flight = true;
    app.entries.push(entry);
    let bars_before = render_buffer(&app, 80, 24);
    let (_, bars_y_before) = find_prompt_row(&bars_before).unwrap();
    app.event(key(KeyCode::Char('/')));
    let bars_after = render_buffer(&app, 80, 24);
    let (_, bars_y_after) = find_prompt_row(&bars_after).unwrap();
    assert_eq!(bars_y_after, bars_y_before);
    app.event(key(KeyCode::Esc));
    app.entries.clear();

    // 5. Tiny terminal (30x12) with `/` open renders cleanly without panic and keeps composer intact.
    let tiny_before = render_buffer(&app, 30, 12);
    let (_, tiny_y_before) = find_prompt_row(&tiny_before).unwrap();

    app.event(key(KeyCode::Char('/')));
    let tiny_empty = render_buffer(&app, 30, 12);
    let (_, tiny_y_empty) = find_prompt_row(&tiny_empty).unwrap();
    assert_eq!(tiny_y_empty, tiny_y_before);
    assert!(row_text(&tiny_empty, tiny_y_empty).contains("❯ /"));

    // Long query tail-slices cleanly and keeps cursor visible at right edge.
    app.event(Event::Paste("a_very_long_command_query_string".into()));
    let tiny_long = render_buffer(&app, 30, 12);
    let (tiny_x_long, tiny_y_long) = find_prompt_row(&tiny_long).unwrap();
    assert_eq!(tiny_y_long, tiny_y_before);
    let right_edge = 30 - 2 - 1;
    assert!(
        tiny_long[(right_edge, tiny_y_long)]
            .modifier
            .contains(Modifier::REVERSED),
        "tail-sliced query must keep reversed cursor at right edge (prompt x={tiny_x_long})"
    );
    app.event(key(KeyCode::Esc));

    // 6. Narrowing `/` to 1 match ("quit") and 0 matches ("zzz") at 80x24 and 30x12
    //    keeps the "Commands" top border visible with zero blank gap and fixed input y.
    for &(width, height) in &[(80u16, 24u16), (30u16, 12u16)] {
        let base = render_buffer(&app, width, height);
        let (_, base_y) = find_prompt_row(&base).unwrap();
        let top_border_y = base_y - 1;

        app.event(key(KeyCode::Char('/')));
        app.event(Event::Paste("quit".into()));
        let one_buf = render_buffer(&app, width, height);
        let (_, one_y) = find_prompt_row(&one_buf).unwrap();
        assert_eq!(one_y, base_y, "{width}x{height} 1-match shifted input y");
        assert!(
            row_text(&one_buf, top_border_y - 1).contains("❯ /quit"),
            "{width}x{height} 1-match body missing: {}",
            row_text(&one_buf, top_border_y - 1)
        );
        assert!(
            row_text(&one_buf, top_border_y - 2).contains("Commands"),
            "{width}x{height} 1-match header missing: {}",
            row_text(&one_buf, top_border_y - 2)
        );
        app.event(key(KeyCode::Esc));

        app.event(key(KeyCode::Char('/')));
        app.event(Event::Paste("zzz".into()));
        let zero_buf = render_buffer(&app, width, height);
        let (_, zero_y) = find_prompt_row(&zero_buf).unwrap();
        assert_eq!(zero_y, base_y, "{width}x{height} 0-match shifted input y");
        let empty_rows = if width - 4 < 33 { 2 } else { 1 };
        let header_y = top_border_y - 1 - empty_rows;
        assert!(
            row_text(&zero_buf, header_y).contains("Commands"),
            "{width}x{height} 0-match header missing at y={header_y}: {}",
            row_text(&zero_buf, header_y)
        );
        let body_text: String = ((header_y + 1)..top_border_y)
            .map(|y| row_text(&zero_buf, y))
            .collect();
        assert!(
            body_text.contains("No matching commands"),
            "{width}x{height} 0-match body missing: {body_text}"
        );
        app.event(key(KeyCode::Esc));
    }

    // 7. Pressing F1 while a multiline draft exists renders cleanly at 80x24 and 30x12
    //    and Esc restores the multiline draft intact.
    app.event(Event::Paste(
        "line1\nline2\nline3\nline4\nline5\nline6".into(),
    ));
    assert_eq!(app.scratch_editor.lines().len(), 6);
    app.event(key(KeyCode::F(1)));
    assert!(matches!(app.panel, Some(Panel::Commands { .. })));
    for &(width, height) in &[(80u16, 24u16), (30u16, 12u16)] {
        let multi_buf = render_buffer(&app, width, height);
        let (_, multi_y) = find_prompt_row(&multi_buf).unwrap();
        assert!(row_text(&multi_buf, multi_y).contains("❯ /"));
    }
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    assert_eq!(
        app.scratch_editor.lines(),
        &["line1", "line2", "line3", "line4", "line5", "line6"]
    );
}

#[tokio::test]
async fn scratch_draft_never_triggers_ghost_quit_confirmation_after_session_added() {
    use crossterm::event::KeyModifiers;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = write_profiles_config(dir.path(), dir.path(), false);
    let mut app = app_from_args(&["--config", cfg_path.to_str().unwrap()]);

    // 1. Type into scratch_editor when entries is empty.
    app.event(Event::Paste("stray scratch draft".into()));
    assert!(app.scratch_has_text_draft());

    // 2. If an entry exists with an empty editor, Ctrl-Q quits immediately without
    //    a false confirmation from the unreachable scratch_editor.
    app.entries
        .push(crate::app::Entry::new("clean-entry".into(), "model".into()));
    app.event(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL,
    )));
    assert!(
        app.confirmation.is_none(),
        "unreachable scratch_editor must not trigger quit confirmation"
    );
    assert!(app.quitting);

    // 3. Real session creation via prepare_and_open_definition also clears scratch_editor
    //    so a later delete cannot resurrect an old scratch draft.
    let mut app2 = app_from_args(&["--config", cfg_path.to_str().unwrap()]);
    app2.event(Event::Paste("stale before create".into()));
    assert!(app2.scratch_has_text_draft());
    let def = app2
        .options
        .config
        .definition(None, Default::default())
        .unwrap();
    app2.prepare_and_open_definition(def).unwrap();
    assert!(
        !app2.scratch_has_text_draft(),
        "prepare_and_open_definition must clear scratch_editor"
    );
    app2.entries.clear();
    app2.event(Event::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL,
    )));
    assert!(app2.confirmation.is_none());
    assert!(app2.quitting);
}
