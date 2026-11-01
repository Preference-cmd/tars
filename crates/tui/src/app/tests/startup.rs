//! Startup splash: environment checks, the download question and the timeout.
//!
//! The fake backend keeps every test hermetic: no platform sandbox probe, no
//! network and no installed `rg` is required. The ready-tool fixture needs a
//! runnable script, so those tests are Unix-only; the remaining states run on
//! every platform that can build the TUI.

use super::support::{app, ctrl, key, until};
use crate::{
    app::{
        App,
        startup::{SandboxCheck, StartupBackend, ToolCheck},
    },
    render,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tars_core::{
    config::{Config, ConfigSource},
    external_tools::{ExternalTools, ToolEnvironment},
};
use tokio::sync::Semaphore;

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

fn asking(app: &App) -> bool {
    app.startup.as_ref().is_some_and(|startup| startup.asking())
}

fn sandbox_ready() -> SandboxCheck {
    SandboxCheck {
        available: true,
        detail: "fixture backend: all dimensions full".to_owned(),
    }
}

fn sandbox_unavailable() -> SandboxCheck {
    SandboxCheck {
        available: false,
        detail: "fixture backend is unavailable: no probe".to_owned(),
    }
}

fn config_file(dir: &Path, name: &str, rg: &str) -> Config {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("version = 1\n[external_tools]\nrg = \"{rg}\"\n"),
    )
    .unwrap();
    Config::load(ConfigSource::File(path)).unwrap()
}

/// A managed source with no data directory: deterministic on every platform
/// and independent of the machine's real `$XDG_DATA_HOME` / `$HOME`.
fn managed_unavailable(dir: &Path) -> ToolCheck {
    let config = config_file(dir, "managed.toml", "managed");
    let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
    assert!(
        matches!(
            tools.rg().source,
            tars_core::external_tools::ToolSource::Managed
        ) && tools.rg().unavailable_reason().is_some()
    );
    ToolCheck {
        status: tools.rg().clone(),
        tools,
    }
}

/// A `system` source checked with no `PATH`: unavailable and never installable
/// by the TUI.
fn system_unavailable(dir: &Path) -> ToolCheck {
    let config = config_file(dir, "system.toml", "system");
    let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
    assert_eq!(
        tools.rg().source,
        tars_core::external_tools::ToolSource::System
    );
    assert!(tools.rg().unavailable_reason().is_some());
    ToolCheck {
        status: tools.rg().clone(),
        tools,
    }
}

/// A ready tool from a fixture script resolved through core, so the check is a
/// real one: the source is a configured absolute path and the tool runs.
#[cfg(unix)]
fn ready_tools(dir: &Path) -> ToolCheck {
    use std::os::unix::fs::PermissionsExt;
    let rg = dir.join("fixture-rg");
    std::fs::write(&rg, "#!/bin/sh\necho 'ripgrep 15.2.0'\n").unwrap();
    let mut permissions = std::fs::metadata(&rg).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&rg, permissions).unwrap();
    let config = config_file(dir, "ready.toml", &rg.display().to_string());
    let tools = ExternalTools::resolve_with(&config, &ToolEnvironment::default());
    assert!(tools.rg().is_ready(), "{:?}", tools.rg());
    ToolCheck {
        status: tools.rg().clone(),
        tools,
    }
}

struct Fake {
    sandbox: SandboxCheck,
    initial: ToolCheck,
    recheck: Mutex<Option<ToolCheck>>,
    install_result: Mutex<Result<(), String>>,
    gate: Mutex<Option<Arc<Semaphore>>>,
    check_delay: Mutex<Duration>,
    checks: AtomicUsize,
    installs: AtomicUsize,
}

impl Fake {
    fn new(sandbox: SandboxCheck, initial: ToolCheck) -> Arc<Self> {
        Arc::new(Self {
            sandbox,
            initial,
            recheck: Mutex::new(None),
            install_result: Mutex::new(Ok(())),
            gate: Mutex::new(None),
            check_delay: Mutex::new(Duration::ZERO),
            checks: AtomicUsize::new(0),
            installs: AtomicUsize::new(0),
        })
    }

    fn recheck_with(&self, check: ToolCheck) {
        *self.recheck.lock().unwrap() = Some(check);
    }

    fn fail_install(&self, reason: &str) {
        *self.install_result.lock().unwrap() = Err(reason.to_owned());
    }

    /// Keep the download job running until the test releases it.
    fn hold_install(&self) {
        *self.gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    }

    fn release_install(&self) {
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.add_permits(1);
        }
    }

    fn slow_checks(&self, delay: Duration) {
        *self.check_delay.lock().unwrap() = delay;
    }

    fn install_calls(&self) -> usize {
        self.installs.load(Ordering::Relaxed)
    }
}

struct FakeBackend {
    fake: Arc<Fake>,
}

impl StartupBackend for FakeBackend {
    fn sandbox(&self) -> SandboxCheck {
        let delay = *self.fake.check_delay.lock().unwrap();
        std::thread::sleep(delay);
        self.fake.sandbox.clone()
    }

    fn tools(&self) -> ToolCheck {
        let delay = *self.fake.check_delay.lock().unwrap();
        std::thread::sleep(delay);
        let first = self.fake.checks.fetch_add(1, Ordering::Relaxed) == 0;
        if !first && let Some(recheck) = self.fake.recheck.lock().unwrap().clone() {
            return recheck;
        }
        self.fake.initial.clone()
    }

    fn install(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let fake = self.fake.clone();
        Box::pin(async move {
            fake.installs.fetch_add(1, Ordering::Relaxed);
            let gate = fake.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                let _permit = gate.acquire().await.expect("fixture gate");
            }
            fake.install_result.lock().unwrap().clone()
        })
    }
}

fn test_app(dir: &Path, fake: &Arc<Fake>) -> App {
    let mut app = app(dir);
    app.set_startup_backend(Arc::new(FakeBackend { fake: fake.clone() }));
    app
}

#[tokio::test]
async fn the_splash_shows_both_checks_while_they_are_outstanding() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    let splash = screen(&app);
    assert!(
        splash.contains("Checking the sandbox and the search tool"),
        "{splash}"
    );
    assert_eq!(splash.matches("checking…").count(), 2, "{splash}");
    assert!(splash.contains("Sandbox"), "{splash}");
    assert!(splash.contains("ripgrep"), "{splash}");
}

#[cfg(unix)]
#[tokio::test]
async fn ready_checks_enter_the_main_screen_without_asking() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), ready_tools(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, |app| app.startup.is_none()).await;
    assert!(app.search_reason.is_none());
    assert!(app.external_tools.is_some());
    assert_eq!(fake.install_calls(), 0, "ready tools are never downloaded");
    let main = screen(&app);
    assert!(main.contains("Welcome to TARS"), "{main}");
    assert!(!main.contains("Download ripgrep"), "{main}");
    assert!(
        app.search_status_line()
            .unwrap()
            .starts_with("Search: ready")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn missing_managed_rg_asks_and_a_confirmed_download_enters_ready() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    fake.recheck_with(ready_tools(dir.path()));
    fake.hold_install();
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, asking).await;
    let splash = screen(&app);
    assert!(
        splash.contains(
            "Download ripgrep 15.2.0 (about 2 MB from github.com/BurntSushi/ripgrep) and install it for search? [y/N]"
        ),
        "{splash}"
    );
    // The 0.5 s ignored-key window starts when the question appears.
    app.event(key(KeyCode::Char('y')));
    assert_eq!(fake.install_calls(), 0);
    assert!(asking(&app), "a key inside the window must not answer");
    // The same key answers once the window has passed.
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    assert!(app.startup.as_ref().is_some_and(|s| s.downloading()));
    let downloading = screen(&app);
    assert!(downloading.contains("Downloading ripgrep"), "{downloading}");
    fake.release_install();
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 1);
    assert!(app.search_reason.is_none());
    assert!(app.external_tools.is_some());
    let main = screen(&app);
    assert!(main.contains("Welcome to TARS"), "{main}");
    assert!(!main.contains("search unavailable"), "{main}");
}

#[tokio::test]
async fn declining_the_download_enters_with_search_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, asking).await;
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('n')));
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 0);
    assert!(app.search_reason.is_some());
    let main = screen(&app);
    assert!(main.contains("search unavailable"), "{main}");
    let status = app.search_status_line().unwrap();
    assert!(status.starts_with("Search: unavailable:"), "{status}");
    // `/status` shows the same line inside the session details panel.
    app.event(key(KeyCode::Char('/')));
    app.event(Event::Paste("status".into()));
    app.event(key(KeyCode::Enter));
    let Some(crate::app::Panel::Info { lines, .. }) = &app.panel else {
        panic!("status panel missing");
    };
    assert!(lines.iter().any(|line| line == &status), "{lines:?}");
}

#[tokio::test]
async fn escape_skips_the_download_and_enters_with_search_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, asking).await;
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Esc));
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 0);
    assert!(screen(&app).contains("search unavailable"));
}

#[tokio::test]
async fn a_failed_download_enters_with_the_failure_reason() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    fake.fail_install("fixture download failed");
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, asking).await;
    app.advance_clock(crate::app::state::CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 1);
    let reason = app.search_reason.as_deref().unwrap();
    assert!(reason.contains("fixture download failed"), "{reason}");
    assert!(screen(&app).contains("search unavailable"));
}

#[tokio::test]
async fn a_system_rg_that_is_unavailable_is_never_offered_for_download() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), system_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 0);
    let reason = app.search_reason.as_deref().unwrap();
    assert!(reason.contains("PATH"), "{reason}");
    assert!(screen(&app).contains("search unavailable"));
}

#[tokio::test]
async fn an_unavailable_sandbox_enters_without_offering_the_download() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_unavailable(), managed_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(fake.install_calls(), 0);
    assert_eq!(
        app.search_reason.as_deref(),
        Some("fixture backend is unavailable: no probe")
    );
    assert!(screen(&app).contains("search unavailable"));
}

#[tokio::test]
async fn a_slow_check_enters_with_the_timeout_reason_instead_of_hanging() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    fake.slow_checks(Duration::from_millis(300));
    let mut app = test_app(dir.path(), &fake);
    app.set_startup_timeout(Duration::from_millis(20));
    app.begin_startup();
    until(&mut app, |app| app.startup.is_none()).await;
    assert_eq!(
        app.search_reason.as_deref(),
        Some("the startup check timed out")
    );
    assert!(app.external_tools.is_none());
    assert!(screen(&app).contains("search unavailable"));
    // A late check result must not reopen a settled startup.
    tokio::time::sleep(Duration::from_millis(350)).await;
    while let Some(joined) = app.jobs.try_join_next() {
        if let Ok(message) = joined {
            app.complete(message);
        }
    }
    assert!(app.startup.is_none());
    assert_eq!(
        app.search_reason.as_deref(),
        Some("the startup check timed out")
    );
}

#[tokio::test]
async fn quit_during_the_splash_stops_the_checks_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(sandbox_ready(), managed_unavailable(dir.path()));
    let mut app = test_app(dir.path(), &fake);
    app.begin_startup();
    app.event(ctrl('q'));
    assert!(app.quitting);
    until(&mut app, |app| app.done).await;
    assert!(app.startup.is_none());
}
