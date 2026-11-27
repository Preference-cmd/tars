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
    let opts = options::parse(args.iter().map(|s| (*s).to_owned()))
        .unwrap()
        .unwrap();
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
    app.complete(crate::app::jobs::Message::Listed(Err(
        tars_core::application::ApplicationError::StoreUnavailable,
    )));
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
    app.complete(crate::app::jobs::Message::Listed(Ok(Vec::new())));
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
