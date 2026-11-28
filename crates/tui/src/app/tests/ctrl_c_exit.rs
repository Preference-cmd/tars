use super::support::{
    Server, app, app_with_folder_trust, create, create_with_definition, ctrl, fixture_definition,
    key, until,
};
use crate::{
    app::{
        App, CONFIRM_GUARD_WINDOW, CTRL_C_EXIT_GRACE_WINDOW, Confirmation, InfoKind, Panel,
        startup::{SandboxCheck, StartupBackend, ToolCheck},
        state::PendingApproval,
    },
    render,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tars_core::{
    config::{Config, ConfigSource},
    definition::CommandNetwork,
    external_tools::{ExternalTools, ToolEnvironment},
    permission::{ApprovalDecision, ApprovalSubject, CommandTarget, PermissionMode},
};
use tokio::sync::Notify;

fn ctrl_repeat(letter: char) -> Event {
    Event::Key(KeyEvent {
        code: KeyCode::Char(letter),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Repeat,
        state: crossterm::event::KeyEventState::NONE,
    })
}

fn screen(app: &App, width: u16, height: u16) -> String {
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

struct BlockingGate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl BlockingGate {
    fn new() -> Self {
        Self {
            open: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn wait(&self) {
        let mut guard = self.open.lock().unwrap();
        while !*guard {
            guard = self.cv.wait(guard).unwrap();
        }
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

struct BlockingStartupBackend {
    tools: ToolCheck,
    release_checks: Arc<BlockingGate>,
    release_install: Arc<Notify>,
    install_called: Arc<AtomicBool>,
}

impl BlockingStartupBackend {
    fn new(dir: &Path) -> Self {
        let cfg_path = dir.join("managed.toml");
        std::fs::write(
            &cfg_path,
            "version = 1\n[external_tools]\nrg = \"managed\"\n",
        )
        .unwrap();
        let config = Config::load(ConfigSource::File(cfg_path)).unwrap();
        let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
        Self {
            tools: ToolCheck {
                status: tools.rg().clone(),
                tools,
            },
            release_checks: Arc::new(BlockingGate::new()),
            release_install: Arc::new(Notify::new()),
            install_called: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl StartupBackend for BlockingStartupBackend {
    fn sandbox(&self) -> SandboxCheck {
        SandboxCheck {
            available: true,
            detail: "fixture sandbox".into(),
        }
    }

    fn tools(&self) -> ToolCheck {
        self.tools.clone()
    }

    fn install(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.install_called.store(true, Ordering::SeqCst);
        let notify = self.release_install.clone();
        Box::pin(async move {
            notify.notified().await;
            Ok(())
        })
    }
}

struct SlowChecksBackend {
    inner: BlockingStartupBackend,
}

impl StartupBackend for SlowChecksBackend {
    fn sandbox(&self) -> SandboxCheck {
        self.inner.release_checks.wait();
        self.inner.sandbox()
    }

    fn tools(&self) -> ToolCheck {
        self.inner.tools()
    }

    fn install(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        self.inner.install()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft06_startup_splash_ctrl_c_bypasses_guard_and_quits_across_all_phases() {
    // 1. Checking phase: Ctrl+C exits immediately before checks complete.
    {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(SlowChecksBackend {
            inner: BlockingStartupBackend::new(dir.path()),
        });
        let mut app = app(dir.path());
        app.set_startup_backend(backend.clone());
        app.begin_startup();
        assert!(app.startup.is_some());

        app.event(ctrl('c'));
        assert!(app.quitting);
        assert!(app.startup.is_none());
        backend.inner.release_checks.release();
        until(&mut app, |a| a.done).await;
    }

    // 2. Asking phase inside 500ms guard window: y/n/Esc are ignored, Ctrl+C quits immediately.
    if tars_core::external_tools::ripgrep_asset().is_some() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingStartupBackend::new(dir.path()));
        let mut app = app(dir.path());
        app.set_startup_backend(backend.clone());
        app.begin_startup();
        until(&mut app, |a| a.startup.as_ref().is_some_and(|s| s.asking())).await;

        let splash = screen(&app, 100, 30);
        assert!(splash.contains("Ctrl-Q / Ctrl-C quits"), "{splash}");

        // Guard blocks y/n/Esc.
        app.event(key(KeyCode::Char('y')));
        app.event(key(KeyCode::Char('n')));
        app.event(key(KeyCode::Esc));
        assert!(app.startup.is_some());
        assert!(!app.quitting);
        assert!(!backend.install_called.load(Ordering::SeqCst));

        // Ctrl+C repeat is swallowed; Ctrl+C press quits even inside guard window.
        app.event(ctrl_repeat('c'));
        assert!(!app.quitting);
        app.event(ctrl('c'));
        assert!(app.quitting);
        assert!(app.startup.is_none());
        until(&mut app, |a| a.done).await;
    }

    // 3. Downloading phase: Ctrl+C aborts download and exits cleanly.
    if tars_core::external_tools::ripgrep_asset().is_some() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(BlockingStartupBackend::new(dir.path()));
        let mut app = app(dir.path());
        app.set_startup_backend(backend.clone());
        app.begin_startup();
        until(&mut app, |a| a.startup.as_ref().is_some_and(|s| s.asking())).await;
        app.advance_clock(CONFIRM_GUARD_WINDOW);
        app.event(key(KeyCode::Char('y')));
        assert!(app.startup.as_ref().is_some_and(|s| s.downloading()));
        let splash = screen(&app, 100, 30);
        assert!(splash.contains("Ctrl-Q / Ctrl-C quits"), "{splash}");

        app.event(ctrl('c'));
        assert!(app.quitting);
        assert!(app.startup.is_none());
        until(&mut app, |a| a.done).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft06_confirmation_modal_priority_and_non_destructive_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // 1. Confirmation::Quit shows quit hint and confirms on Ctrl+C (even when too_small).
    app.entries[0].editor.insert_str("unsent draft");
    app.event(ctrl('c'));
    assert!(matches!(
        app.confirmation,
        Some(Confirmation::Quit {
            has_drafts: true,
            ..
        })
    ));
    let confirm_view = screen(&app, 100, 30);
    assert!(
        confirm_view.contains("y or Ctrl-C quit · n / Esc cancel"),
        "{confirm_view}"
    );
    app.too_small = true;
    app.event(ctrl('c'));
    assert!(app.quitting);
    until(&mut app, |a| a.done).await;

    // 2. Confirmation::{Close, Discard, DeleteSession} cancel modal on Ctrl+C (never confirm).
    let mut app2 = app_with_folder_trust(dir.path(), None);
    app2.options.check_folder_trust = false;
    create(&mut app2, &server.endpoint).await;
    let id = app2.entries[0].id().to_owned();

    for modal in [
        Confirmation::Close {
            has_draft: true,
            has_queue: false,
        },
        Confirmation::Discard,
        Confirmation::DeleteSession {
            id: id.clone(),
            label: id.clone(),
        },
    ] {
        app2.confirmation = Some(modal);
        app2.event(ctrl('c'));
        assert!(app2.confirmation.is_none());
        assert!(!app2.quitting);
        assert!(app2.host.lookup(&id.as_str().into()).is_some());
    }

    // 3. When quitting == true, Ctrl+C is ignored.
    app2.quitting = true;
    app2.event(ctrl('c'));
    assert!(app2.quitting);
    assert!(app2.confirmation.is_none());
    until(&mut app2, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft06_folder_trust_wizard_ctrl_c_exits_cleanly_in_guard_saving_and_too_small() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();

    for (too_small, saving) in [(false, false), (true, false), (false, true)] {
        let mut app = app_with_folder_trust(dir.path(), Some(ws.clone()));
        app.list();
        until(&mut app, |a| !a.listing && a.folder_trust.is_some()).await;
        assert!(app.is_folder_trust_guarded());
        app.too_small = too_small;
        if saving {
            app.folder_trust.as_mut().unwrap().saving = true;
        }
        app.event(ctrl('c'));
        assert!(app.quitting);
        assert!(!app.host.is_workspace_folder_trusted(&ws).await.unwrap());
        assert!(!dir.path().join("config.toml").exists());
        until(&mut app, |a| a.done).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft06_idle_surfaces_new_form_repair_guard_and_too_small_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();

    // 1. Empty main screen and too_small idle exit immediately without confirmation.
    {
        let mut app = app(dir.path());
        app.too_small = true;
        app.event(ctrl('c'));
        assert!(app.quitting);
        assert!(app.confirmation.is_none());
    }

    // 2. Non-approval panels (including Panel::Repair inside 500ms guard) exit consistently.
    for make_panel in [
        || Panel::Commands {
            query: "help".into(),
            selected: 0,
        },
        || Panel::Sessions {
            query: String::new(),
            selected: 0,
        },
        || Panel::Models {
            query: String::new(),
            selected: 0,
        },
        || Panel::Info {
            title: "Info".into(),
            kind: InfoKind::Plain,
            lines: vec!["hello".into()],
            scroll: 0,
        },
        || Panel::ModeSelect { selected: 1 },
        || Panel::Agents { selected: 0 },
    ] {
        let mut app = app(dir.path());
        app.panel = Some(make_panel());
        app.event(ctrl('c'));
        assert!(app.panel.is_none());
        assert!(app.quitting);
    }

    // Panel::Repair inside 500ms guard window: Ctrl+C bypasses is_repair_guarded().
    {
        let mut app = app(dir.path());
        let context = causa::kernel::Context::from_blocks(vec![
            tars_core::application::text_input("old input"),
            causa::kernel::ContextBlock::new(
                causa::runtime::new_block_id(),
                causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
                    "read_file",
                    serde_json::json!({"path": "old"}),
                )),
                causa::kernel::BlockMeta::default().with_provider_call_id("old-unanswered"),
            ),
        ])
        .unwrap();
        let session = app
            .host
            .create(tars_core::application::PreparedCreate::persistent(
                super::support::history_definition(None),
                context,
            ))
            .await
            .unwrap();
        let mut entry = crate::app::Entry::new(
            session.target().conversation_id().as_str().into(),
            "fixture".into(),
        );
        entry.session = Some(session);
        app.entries.push(entry);
        let plan = app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .plan_repair(tars_core::application::RepairMode::RemoveUnpaired)
            .unwrap();
        let mut repair_panel = crate::app::repair::RepairPanel::for_test(vec!["plan".into()]);
        let now = app.clock.now();
        repair_panel.plan = Some(plan);
        repair_panel.guard.arm(now, CONFIRM_GUARD_WINDOW);
        app.panel = Some(Panel::Repair(Box::new(repair_panel)));
        assert!(app.is_repair_guarded());
        app.event(ctrl('c'));
        assert!(app.panel.is_none());
        assert!(app.quitting);
        until(&mut app, |a| a.done).await;
    }

    // 3. Scratch draft open with Panel::Provider: Ctrl+C clears panel (and wipes secret) and opens Confirmation::Quit.
    {
        let mut app = app(dir.path());
        app.event(Event::Paste("unsent draft".into()));
        app.open_provider_wizard();
        app.event(key(KeyCode::Enter));
        app.event(Event::Paste("sk-secret-to-wipe".into()));
        app.event(ctrl('c'));
        assert!(app.panel.is_none());
        assert!(!app.quitting);
        assert!(matches!(
            app.confirmation,
            Some(Confirmation::Quit {
                has_drafts: true,
                ..
            })
        ));
        // Esc closes Confirmation::Quit and keeps scratch draft intact.
        app.event(key(KeyCode::Esc));
        assert!(app.confirmation.is_none());
        assert!(app.scratch_has_text_draft());
        // Second sequence of Ctrl+C + Ctrl+C confirms quit.
        app.event(ctrl('c'));
        assert!(matches!(app.confirmation, Some(Confirmation::Quit { .. })));
        app.event(ctrl('c'));
        assert!(app.quitting);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft07_pending_approval_and_workspace_trust_ctrl_c_priorities() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    create(&mut app, &server.endpoint).await;

    // 1. Pending tool approval inside 500ms guard, < 30x19 too_small, and even with Panel::Info open:
    //    Ctrl+C clears approvals and cancels, records last_ctrl_c_cancel_at, never approves, never quits.
    let decisions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let decisions_cb = decisions.clone();
    app.entries[0]
        .approval_queue
        .push_back(PendingApproval::mock(
            "run_command".into(),
            PermissionMode::Ask,
            ApprovalSubject::Command {
                command: "echo hi".into(),
                workdir: ".".into(),
                timeout_secs: 30,
                target: CommandTarget::Sandboxed {
                    network: CommandNetwork::None,
                },
                suggested_prefixes: vec![],
                can_save_workspace_rule: false,
            },
            move |d: ApprovalDecision| {
                decisions_cb.lock().unwrap().push(d);
            },
        ));
    app.update_approval_guard();
    assert!(app.is_approval_guarded());
    app.too_small = true;
    app.panel = Some(Panel::Info {
        title: "Obscuring".into(),
        kind: InfoKind::Plain,
        lines: vec!["x".into()],
        scroll: 0,
    });
    let tiny = screen(&app, 28, 18);
    assert!(!tiny.is_empty());

    app.event(ctrl('c'));
    assert!(!app.quitting);
    assert!(app.entries[0].approval_queue.is_empty());
    assert!(app.last_ctrl_c_cancel_at.is_some());
    assert!(!app.ctrl_c_exit_armed);
    assert!(decisions.lock().unwrap().is_empty());

    // Check approval too_small message when approval is visible.
    app.panel = None;
    app.entries[0]
        .approval_queue
        .push_back(PendingApproval::mock(
            "read_file".into(),
            PermissionMode::Ask,
            ApprovalSubject::FileWrite {
                path: "a.txt".into(),
                action: tars_core::permission::WriteAction::Created,
                diff: "+a".into(),
            },
            |_| {},
        ));
    let small_approval = screen(&app, 120, 18)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        small_approval.contains("Esc / Ctrl-C cancels this turn. Ctrl-Q quits."),
        "{small_approval}"
    );
    app.event(ctrl('c'));
    assert!(app.entries[0].approval_queue.is_empty());
    app.too_small = false;

    // 2. Pending real .tars rules and agents trust prompts: Ctrl+C declines (never trusts, never quits).
    let ws = dir.path().join("ws-trust");
    std::fs::create_dir_all(ws.join(".tars/agents")).unwrap();
    std::fs::write(
        ws.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join(".tars/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Review changes\n---\nReview code carefully.\n",
    )
    .unwrap();
    let canonical_ws = ws.canonicalize().unwrap();
    let dir_trust = tempfile::tempdir().unwrap();
    let mut app_trust = super::support::app(dir_trust.path());
    let mut def_trust = fixture_definition("openai-chat", &server.endpoint);
    def_trust.workspace_root = Some(canonical_ws);
    create_with_definition(&mut app_trust, def_trust).await;
    assert!(app_trust.entries[0].workspace_rules_trust.is_some());
    assert!(app_trust.entries[0].workspace_agents_trust.is_some());

    app_trust.update_workspace_trust_guard();
    assert!(app_trust.is_workspace_trust_guarded());
    let trust_view = screen(&app_trust, 100, 30);
    assert!(
        trust_view.contains("n / Esc / Ctrl-C decline"),
        "{trust_view}"
    );

    // Even inside guard, too_small, and with a panel open, Ctrl+C declines rules trust.
    app_trust.too_small = true;
    app_trust.panel = Some(Panel::Commands {
        query: String::new(),
        selected: 0,
    });
    app_trust.event(ctrl('c'));
    assert!(app_trust.entries[0].workspace_rules_trust.is_none());
    assert!(!app_trust.quitting);
    assert!(
        !app_trust.entries[0]
            .session
            .as_ref()
            .unwrap()
            .workspace_rules_trusted()
    );
    app_trust.panel = None;
    app_trust.too_small = false;

    // Next, workspace agents trust prompt is visible and Ctrl+C declines it too.
    app_trust.update_workspace_trust_guard();
    let agents_view = screen(&app_trust, 100, 30);
    assert!(
        agents_view.contains("n / Esc / Ctrl-C decline"),
        "{agents_view}"
    );
    app_trust.event(ctrl('c'));
    assert!(app_trust.entries[0].workspace_agents_trust.is_none());
    assert!(!app_trust.quitting);
    assert!(
        !app_trust.entries[0]
            .session
            .as_ref()
            .unwrap()
            .open_report()
            .workspace_agents
            .trusted
    );

    // After declining both prompts, idle Ctrl+C exits cleanly.
    app_trust.event(ctrl('c'));
    assert!(app_trust.quitting);
    until(&mut app_trust, |a| a.done).await;

    // Expire grace from earlier approval cancel on `app` so idle Ctrl+C exits cleanly.
    app.advance_clock(CTRL_C_EXIT_GRACE_WINDOW);
    app.event(ctrl('c'));
    assert!(app.quitting);
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft07_active_work_priority_completed_turn_exit_and_1500ms_grace_window() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app1 = app(dir.path());
    create(&mut app1, &server.endpoint).await;

    // 1. Completed turn retains entry.work = Some(...) for /repair, but entry_has_active_work() MUST be false.
    app1.event(Event::Paste("first completed turn".into()));
    app1.event(key(KeyCode::Enter));
    until(&mut app1, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app1, |a| {
        a.entries[0].session.is_some()
            && a.entries[0].execution == "Completed"
            && !a.entry_has_active_work()
    })
    .await;
    assert!(app1.entries[0].work.is_some());
    assert!(!app1.entry_has_active_work());

    // Idle Ctrl+C after completed turn exits cleanly without getting stuck on entry.work.
    app1.event(ctrl('c'));
    assert!(app1.quitting);
    until(&mut app1, |a| a.done).await;

    // 2. Active turn: Ctrl+C cancels (even with Form/Panel/too_small), enters 1500ms grace window.
    let server2 = Server::new().await;
    let mut app2 = app(dir.path());
    create(&mut app2, &server2.endpoint).await;
    app2.event(Event::Paste("turn to cancel".into()));
    app2.event(key(KeyCode::Enter));
    until(&mut app2, |_| server2.requests.lock().unwrap().len() == 1).await;
    assert!(app2.entry_has_active_work());

    // Open Panel::Provider + too_small while turn is running: active work still wins priority over panel/idle.
    app2.open_provider_wizard();
    app2.too_small = true;
    app2.event(ctrl('c'));
    assert!(!app2.quitting);
    assert!(app2.confirmation.is_none());
    assert_eq!(
        app2.entries[0].notice,
        "Cancellation requested; press Ctrl-C again once idle to exit"
    );
    assert!(app2.last_ctrl_c_cancel_at.is_some());
    assert!(!app2.ctrl_c_exit_armed);
    app2.panel = None;
    app2.too_small = false;

    until(&mut app2, |a| {
        a.entries[0].execution == "Cancelled" && !a.entry_has_active_work()
    })
    .await;

    // Within 1500ms grace window: first idle Ctrl+C arms exit instead of quitting.
    app2.advance_clock(Duration::from_millis(500));
    app2.event(ctrl('c'));
    assert!(!app2.quitting);
    assert!(app2.ctrl_c_exit_armed);
    assert_eq!(app2.entries[0].notice, "Press Ctrl-C again to exit");

    // Non-Ctrl+C user action resets ctrl_c_exit_armed, while grace still holds until 1500ms.
    app2.event(key(KeyCode::Esc));
    assert!(!app2.ctrl_c_exit_armed);
    app2.advance_clock(Duration::from_millis(500)); // total 1000ms < 1500ms
    app2.event(ctrl('c'));
    assert!(!app2.quitting);
    assert!(app2.ctrl_c_exit_armed);

    // Second Ctrl+C while armed triggers exit.
    app2.event(ctrl('c'));
    assert!(app2.quitting);
    until(&mut app2, |a| a.done).await;

    // 3. After 1500ms grace window expires, single idle Ctrl+C exits immediately.
    let server3 = Server::new().await;
    let mut app3 = app(dir.path());
    create(&mut app3, &server3.endpoint).await;
    app3.event(Event::Paste("turn 3".into()));
    app3.event(key(KeyCode::Enter));
    until(&mut app3, |_| server3.requests.lock().unwrap().len() == 1).await;
    app3.event(ctrl('c'));
    until(&mut app3, |a| {
        a.entries[0].execution == "Cancelled" && !a.entry_has_active_work()
    })
    .await;
    app3.advance_clock(CTRL_C_EXIT_GRACE_WINDOW);
    assert!(app3.last_ctrl_c_cancel_at.is_none());
    app3.event(ctrl('c'));
    assert!(app3.quitting);
    until(&mut app3, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ft06_ctrl_repeat_never_inserts_char_or_double_confirms_and_esc_never_exits() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    // 1. Holding Ctrl+C (KeyEventKind::Repeat) never inserts 'c' into scratch editor.
    app.event(ctrl_repeat('c'));
    assert_eq!(app.scratch_editor.lines(), &[""]);
    assert!(!app.quitting);

    // 2. Panel::Provider Step 2 swallows Ctrl Repeat events; Esc returns to Step 1 then closes without quitting.
    app.open_provider_wizard();
    app.event(key(KeyCode::Enter));
    app.event(ctrl_repeat('c'));
    assert!(!app.quitting);
    assert!(app.confirmation.is_none());
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert!(wizard.api_key.is_empty());
    } else {
        panic!("expected Panel::Provider");
    }

    // Esc returns to Step 1 then closes Panel::Provider, never exits App.
    app.event(key(KeyCode::Esc));
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    assert!(!app.quitting);

    // 3. Panel search inputs never receive 'c' on Ctrl+C Repeat.
    app.panel = Some(Panel::Commands {
        query: String::new(),
        selected: 0,
    });
    app.event(ctrl_repeat('c'));
    assert!(matches!(
        &app.panel,
        Some(Panel::Commands { query, .. }) if query.is_empty()
    ));
    assert!(!app.quitting);
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    assert!(!app.quitting);

    // 4. Draft + Ctrl+C press opens Confirmation::Quit, followed by Ctrl+C Repeat MUST NOT confirm quit.
    app.event(Event::Paste("keep me".into()));
    app.event(ctrl('c'));
    assert!(matches!(app.confirmation, Some(Confirmation::Quit { .. })));
    app.event(ctrl_repeat('c'));
    assert!(!app.quitting);
    assert!(app.confirmation.is_some());

    // Esc cancels Confirmation::Quit, never exits.
    app.event(key(KeyCode::Esc));
    assert!(app.confirmation.is_none());
    assert!(!app.quitting);
    assert_eq!(app.scratch_editor.lines(), &["keep me"]);

    // `/quit` still triggers Confirmation::Quit with draft, and Ctrl+Q still works.
    app.event(ctrl('q'));
    assert!(matches!(app.confirmation, Some(Confirmation::Quit { .. })));
    app.event(ctrl('c'));
    assert!(app.quitting);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr10_ctrl_c_on_provider_wizard_respects_active_work_and_clears_secret_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = super::support::app(dir.path());
    app.options.config_path = Some(dir.path().join("config.toml"));
    app.options.credentials_path = Some(dir.path().join("credentials.toml"));

    // 1. Active work running while Panel::Provider Step 2 has a secret: Ctrl+C cancels work first!
    super::support::create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("active turn".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    assert!(app.entry_has_active_work());

    app.open_provider_wizard();
    app.event(key(KeyCode::Enter)); // openai Step 2
    app.event(Event::Paste("sk-secret-during-work".into()));
    app.event(ctrl('c'));
    assert!(!app.quitting);
    assert!(matches!(app.panel, Some(Panel::Provider(_))));
    assert_eq!(
        app.entries[0].notice,
        "Cancellation requested; press Ctrl-C again once idle to exit"
    );
    until(&mut app, |a| {
        a.entries[0].execution == "Cancelled" && !a.entry_has_active_work()
    })
    .await;

    // 2. When idle (after grace expires), Ctrl+C on Panel::Provider clears secret buffer, closes panel, and quits.
    app.advance_clock(CTRL_C_EXIT_GRACE_WINDOW);
    if let Some(Panel::Provider(wizard)) = &app.panel {
        assert_eq!(wizard.api_key, "sk-secret-during-work");
    } else {
        panic!("expected Panel::Provider still open");
    }
    app.event(ctrl('c'));
    assert!(app.panel.is_none());
    assert!(app.quitting);
    until(&mut app, |a| a.done).await;
}
