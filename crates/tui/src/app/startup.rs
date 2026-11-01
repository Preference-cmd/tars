//! Startup environment check shown before the conversation screen.
//!
//! The checks themselves belong to core: this module only asks for them in
//! blocking tasks, shows their progress, and collects the answer to the one
//! question that needs consent. The sandbox probe runs negative test scripts
//! and the tool check hashes and executes a binary, so both run under a bounded
//! deadline: a slow probe can delay startup, but it can never hold it open.
//!
//! Readiness and the decision whether a download is offered keep core's
//! vocabulary: a resolved [`ToolStatus`] and the core's own sandbox report are
//! displayed as they are, and a download is offered only for the managed
//! source that `install_managed_rg` can replace.

use super::{
    App,
    jobs::{JobKind, Message, blocking},
    state::{CONFIRM_GUARD_WINDOW, GuardWindow},
};
use crate::input;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tars_core::{
    config::Config,
    external_tools::{ExternalTools, RIPGREP_VERSION, ToolSource, ToolStatus, install_managed_rg},
    sandbox::{Sandbox, SandboxStatus},
};
use tokio::time::Instant as TokioInstant;

/// How long the startup checks may run before the splash gives up.
///
/// The bound covers the sandbox probe (several `sandbox-exec` runs), the
/// managed hash check and `rg --version`. It is generous for a healthy
/// machine and finite so startup cannot hang on a stuck probe.
pub(super) const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// The sandbox probe as the splash presents it.
///
/// Only readiness and core's own short report ([`SandboxStatus::brief`]) are
/// kept, so the TUI neither interprets the backend vocabulary nor needs a
/// platform backend to exercise the startup states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxCheck {
    /// Whether the backend can enforce a policy.
    pub(crate) available: bool,
    /// Core's short report or unavailable reason.
    pub(crate) detail: String,
}

impl SandboxCheck {
    fn of(status: &SandboxStatus) -> Self {
        Self {
            available: status.is_available(),
            detail: status.brief(),
        }
    }
}

/// One external-tool check: the status the splash displays and decides on, and
/// the process-wide check sessions bind for `search_text`.
#[derive(Clone, Debug)]
pub(crate) struct ToolCheck {
    pub(crate) status: ToolStatus,
    pub(crate) tools: ExternalTools,
}

/// The blocking checks and the consent-gated installation, behind one seam.
///
/// The production implementation probes the platform and resolves the
/// configured tools. Tests inject a fixture so every startup state can be
/// exercised without a platform backend, an installed `rg` or the network.
pub(crate) trait StartupBackend: Send + Sync {
    /// Probe the platform sandbox. Blocking; called from a blocking task.
    fn sandbox(&self) -> SandboxCheck;
    /// Resolve the external tools. Blocking; called from a blocking task.
    fn tools(&self) -> ToolCheck;
    /// Download and install the managed tool after an explicit yes.
    fn install(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

/// The checks a real process performs.
pub(crate) struct ProcessBackend {
    config: Arc<Config>,
}

impl ProcessBackend {
    pub(crate) fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl StartupBackend for ProcessBackend {
    fn sandbox(&self) -> SandboxCheck {
        let status = match Sandbox::open() {
            Ok(sandbox) => SandboxStatus::Available(sandbox.report().clone()),
            Err(error) => SandboxStatus::Unavailable(error),
        };
        SandboxCheck::of(&status)
    }

    fn tools(&self) -> ToolCheck {
        let tools = ExternalTools::resolve(&self.config);
        ToolCheck {
            status: tools.rg().clone(),
            tools,
        }
    }

    fn install(&self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async move {
            install_managed_rg(&self.config)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
    }
}

/// What the splash is waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Both checks are outstanding.
    Checking,
    /// The managed `rg` is missing; the user is asked whether to download it.
    Asking,
    /// A download is running.
    Downloading,
    /// The download finished; the installed tool is being re-resolved.
    Rechecking,
}

/// The splash state: the outstanding checks, the consent question and the
/// installation that may follow it.
pub(crate) struct Startup {
    phase: Phase,
    sandbox: Option<SandboxCheck>,
    tools: Option<ToolCheck>,
    guard: GuardWindow,
    deadline: TokioInstant,
    timeout: Duration,
}

impl Startup {
    fn new(timeout: Duration) -> Self {
        Self {
            phase: Phase::Checking,
            sandbox: None,
            tools: None,
            guard: GuardWindow::new(),
            deadline: TokioInstant::now() + timeout,
            timeout,
        }
    }

    /// Whether the download question is on screen.
    pub(crate) fn asking(&self) -> bool {
        self.phase == Phase::Asking
    }

    /// Whether a download is running.
    pub(crate) fn downloading(&self) -> bool {
        self.phase == Phase::Downloading
    }

    /// The sandbox row: core's report, or that the check is still running.
    pub(crate) fn sandbox_row(&self) -> String {
        match &self.sandbox {
            Some(check) => check.detail.clone(),
            None => "checking…".to_owned(),
        }
    }

    /// The ripgrep row: core's ready/unavailable reason, or the download state.
    pub(crate) fn tools_row(&self) -> String {
        match self.phase {
            Phase::Downloading => format!("downloading ripgrep {RIPGREP_VERSION} (about 2 MB)…"),
            Phase::Rechecking => "installed; verifying…".to_owned(),
            Phase::Checking | Phase::Asking => match &self.tools {
                None => "checking…".to_owned(),
                Some(check) if check.status.is_ready() => {
                    format!("ready ({})", ready_detail(&check.status))
                }
                Some(check) => format!(
                    "unavailable: {}",
                    check
                        .status
                        .unavailable_reason()
                        .unwrap_or("unknown reason")
                ),
            },
        }
    }

    /// The download question, exactly as the CLI asks it.
    fn question(&self) -> Option<String> {
        self.asking().then(|| {
            format!(
                "Download ripgrep {RIPGREP_VERSION} (about 2 MB from github.com/BurntSushi/ripgrep) and install it for search? [y/N]"
            )
        })
    }

    /// The main line of the splash: what is happening and what to answer.
    pub(crate) fn action(&self) -> String {
        match self.phase {
            Phase::Checking => {
                "Checking the sandbox and the search tool. This can take a few seconds.".to_owned()
            }
            Phase::Asking => self.question().unwrap_or_default(),
            Phase::Downloading => format!("Downloading ripgrep {RIPGREP_VERSION}…"),
            Phase::Rechecking => "Verifying the installed ripgrep…".to_owned(),
        }
    }

    /// The key hint for the current phase, when a key does something.
    pub(crate) fn hint(&self) -> Option<&'static str> {
        match self.phase {
            Phase::Asking => Some("y installs · n or Esc skips · Ctrl-Q quits"),
            Phase::Downloading | Phase::Rechecking => Some("Ctrl-Q quits"),
            Phase::Checking => None,
        }
    }

    fn guarded(&self, now: std::time::Instant) -> bool {
        self.asking() && self.guard.is_guarded(now)
    }
}

impl App {
    /// Start the environment check and the two blocking jobs that feed it.
    ///
    /// Called once by the event loop after terminal initialization; the main
    /// screen is only reachable once the check settles, so it is never shown
    /// with an unresolved environment.
    pub(crate) fn begin_startup(&mut self) {
        if self.startup.is_some() {
            return;
        }
        self.startup = Some(Startup::new(self.startup_timeout()));
        self.dirty = true;
        let backend = self.startup_backend.clone();
        self.spawn_job(None, JobKind::Startup, async move {
            Message::StartupSandbox(
                blocking(move || backend.sandbox())
                    .await
                    .expect("startup check task"),
            )
        });
        self.spawn_tools_check();
    }

    /// Resolve the tools in a blocking task, on the first check and after an
    /// installation.
    fn spawn_tools_check(&mut self) {
        let backend = self.startup_backend.clone();
        self.spawn_job(None, JobKind::Startup, async move {
            Message::StartupTools(
                blocking(move || backend.tools())
                    .await
                    .expect("startup check task"),
            )
        });
    }

    /// The startup timeout; a test may shorten the wait.
    fn startup_timeout(&self) -> Duration {
        #[cfg(test)]
        {
            self.test_startup_timeout
        }
        #[cfg(not(test))]
        {
            STARTUP_TIMEOUT
        }
    }

    #[cfg(test)]
    pub(crate) fn set_startup_backend(&mut self, backend: Arc<dyn StartupBackend>) {
        self.startup_backend = backend;
    }

    #[cfg(test)]
    pub(crate) fn set_startup_timeout(&mut self, timeout: Duration) {
        self.test_startup_timeout = timeout;
    }

    /// Route one terminal event while the splash is on screen.
    pub(super) fn startup_event(&mut self, event: Event) {
        self.update_startup_guard();
        self.dirty = true;
        match event {
            Event::Resize(width, height) => {
                self.view_width = crate::render::transcript_width(width);
                self.view_height = height;
                self.viewport_height = crate::render::conversation_height(height, width, self);
            }
            Event::Key(key) => self.startup_key(key),
            _ => {}
        }
    }

    /// Keys during startup: quit, or answer the one question. Everything else
    /// is ignored, including every key inside the 0.5 s window after the
    /// question appears.
    fn startup_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let Some(startup) = self.startup.as_ref() else {
            return;
        };
        if startup.guarded(self.clock.now()) {
            return;
        }
        let phase = startup.phase;
        if input::control(key, 'q') {
            self.quit_startup();
            return;
        }
        if phase != Phase::Asking {
            return;
        }
        match key.code {
            KeyCode::Char('y' | 'Y')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.install_ripgrep();
            }
            KeyCode::Char('n' | 'N')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.skip_install();
            }
            KeyCode::Esc => self.skip_install(),
            _ => {}
        }
    }

    /// Leave without touching a session: nothing has been created yet.
    fn quit_startup(&mut self) {
        self.jobs.abort_all();
        self.startup = None;
        self.quitting = true;
        self.dirty = true;
    }

    /// Answer yes: install in the background and keep the download visible.
    fn install_ripgrep(&mut self) {
        let Some(startup) = self.startup.as_mut() else {
            return;
        };
        if startup.phase != Phase::Asking {
            return;
        }
        startup.phase = Phase::Downloading;
        startup.guard.disarm();
        self.dirty = true;
        let backend = self.startup_backend.clone();
        self.spawn_job(None, JobKind::StartupInstall, async move {
            Message::StartupInstalled(backend.install().await)
        });
    }

    /// Answer no (or Esc): enter the main screen with the tool's own reason.
    fn skip_install(&mut self) {
        let reason = self.startup_tool_reason();
        self.settle_startup(reason);
    }

    /// The sandbox check finished.
    pub(super) fn startup_sandbox(&mut self, check: SandboxCheck) {
        let Some(startup) = self.startup.as_mut() else {
            return;
        };
        startup.sandbox = Some(check);
        self.startup_decide();
    }

    /// The tool check finished, either the first check or the re-resolve after
    /// an installation.
    pub(super) fn startup_tools(&mut self, check: ToolCheck) {
        let Some(startup) = self.startup.as_mut() else {
            return;
        };
        let rechecking = startup.phase == Phase::Rechecking;
        startup.tools = Some(check);
        if rechecking {
            self.startup_rechecked();
        } else {
            self.startup_decide();
        }
    }

    /// The installation job finished.
    pub(super) fn startup_installed(&mut self, result: Result<(), String>) {
        let Some(startup) = self.startup.as_ref() else {
            return;
        };
        if startup.phase != Phase::Downloading {
            return;
        }
        match result {
            Ok(()) => {
                let deadline = TokioInstant::now() + startup.timeout;
                if let Some(startup) = self.startup.as_mut() {
                    startup.phase = Phase::Rechecking;
                    startup.deadline = deadline;
                }
                self.dirty = true;
                self.spawn_tools_check();
            }
            Err(error) => {
                self.settle_startup(Some(format!("ripgrep could not be installed: {error}")));
            }
        }
    }

    /// A startup job panicked: enter with the failure instead of waiting for a
    /// check that will never report.
    pub(super) fn startup_failed(&mut self, detail: &str) {
        if self.startup.is_none() {
            return;
        }
        self.settle_startup(Some(format!("the startup check failed: {detail}")));
    }

    /// Both first checks reported: enter, ask, or enter with the reason.
    fn startup_decide(&mut self) {
        let Some(startup) = self.startup.as_ref() else {
            return;
        };
        let (Some(sandbox), Some(tools)) = (&startup.sandbox, &startup.tools) else {
            return;
        };
        if !sandbox.available {
            // Search needs both halves, so an unusable backend is reported
            // before asking about a tool that could not make search work.
            let reason = sandbox.detail.clone();
            self.settle_startup(Some(reason));
            return;
        }
        if tools.status.is_ready() {
            self.settle_startup(None);
            return;
        }
        if matches!(tools.status.source, ToolSource::Managed) {
            // Only the managed copy can be installed; a system or configured
            // path is the user's own file and is never downloaded over.
            if let Some(startup) = self.startup.as_mut() {
                startup.phase = Phase::Asking;
                startup.guard.arm(self.clock.now(), CONFIRM_GUARD_WINDOW);
            }
            self.dirty = true;
            return;
        }
        let reason = tools.status.unavailable_reason().map(str::to_owned);
        self.settle_startup(reason);
    }

    /// The re-resolve after an installation reported: next phase is the main
    /// screen either way.
    fn startup_rechecked(&mut self) {
        let reason = self.startup_tool_reason();
        self.settle_startup(reason);
    }

    /// The unavailable reason of the current tool check, if it has one.
    fn startup_tool_reason(&self) -> Option<String> {
        self.startup
            .as_ref()
            .and_then(|startup| startup.tools.as_ref())
            .filter(|check| !check.status.is_ready())
            .and_then(|check| check.status.unavailable_reason().map(str::to_owned))
    }

    /// Give up an outstanding check after the bounded wait.
    pub(super) fn poll_startup(&mut self) {
        self.update_startup_guard();
        let expired = match self.startup.as_ref() {
            Some(startup) => {
                matches!(startup.phase, Phase::Checking | Phase::Rechecking)
                    && TokioInstant::now() >= startup.deadline
            }
            None => return,
        };
        if expired {
            self.settle_startup(Some("the startup check timed out".to_owned()));
        }
    }

    /// Re-arm or expire the ignored-key window of the download question.
    pub(crate) fn update_startup_guard(&mut self) {
        let now = self.clock.now();
        let expired = self
            .startup
            .as_mut()
            .is_some_and(|startup| startup.guard.check_expired(now));
        if expired {
            self.dirty = true;
        }
    }

    /// Enter the main screen: record the checks, bind the resolved tools to the
    /// session host every later session uses, and keep the reason search is
    /// unavailable when it is.
    fn settle_startup(&mut self, reason: Option<String>) {
        let Some(startup) = self.startup.take() else {
            return;
        };
        self.sandbox = startup.sandbox;
        if let Some(ToolCheck { tools, .. }) = startup.tools {
            self.host = tars_core::application::SessionHost::new(Some(self.options.store.clone()))
                .with_external_tools(tools.clone());
            self.external_tools = Some(tools);
        }
        self.search_reason = reason;
        self.dirty = true;
        self.list();
    }

    /// The `/status` line for search, in core's own words.
    pub(super) fn search_status_line(&self) -> Option<String> {
        if let Some(tools) = &self.external_tools {
            let rg = tools.rg();
            if rg.is_ready() {
                return Some(format!("Search: ready ({})", ready_detail(rg)));
            }
        }
        self.search_reason
            .as_ref()
            .map(|reason| format!("Search: unavailable: {reason}"))
    }

    /// The persistent footer line while search cannot work.
    pub(crate) fn search_unavailable_notice(&self) -> Option<String> {
        self.search_reason
            .as_ref()
            .map(|reason| format!("search unavailable: {reason}"))
    }
}

/// Version and path of a ready tool, without inventing another status word.
fn ready_detail(status: &ToolStatus) -> String {
    let version = status.version.as_deref().unwrap_or(RIPGREP_VERSION);
    match status.path.as_deref() {
        Some(path) => format!("rg {version} at {}", path.display()),
        None => format!("rg {version}"),
    }
}
