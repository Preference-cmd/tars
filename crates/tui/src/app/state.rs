use super::Clock;
use super::jobs::Message;
use crate::{
    input::{self, Form},
    options::Options,
    transcript::{Rows, Transcript},
};
use ratatui_textarea::TextArea;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tars_core::application::{ManagedSession, MaterialVersion, PreparedCreate, SessionHost};
use tars_core::permission::{
    ApprovalDecision, ApprovalReceiver, ApprovalRequest, ApprovalSubject, PermissionMode,
};
use tars_core::session::WorkRef;
use tokio::task::JoinSet;

// Limits UI-owned in-flight work/results; core separately limits active instances.
pub(super) const MAX_JOBS: usize = 64;

/// How long keys are ignored after sensitive confirmation content appears or
/// changes without a key press, so keys typed for something else do not decide it.
pub(crate) const CONFIRM_GUARD_WINDOW: Duration = Duration::from_millis(500);

/// Armable window that ignores keys until it expires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GuardWindow {
    until: Option<Instant>,
}

impl GuardWindow {
    pub(crate) const fn new() -> Self {
        Self { until: None }
    }

    pub(crate) fn arm(&mut self, now: Instant, duration: Duration) {
        self.until = Some(now + duration);
    }

    pub(crate) fn disarm(&mut self) {
        self.until = None;
    }

    pub(crate) fn is_guarded(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    /// Disarm once expired and report it, so the caller can redraw the hint.
    pub(crate) fn check_expired(&mut self, now: Instant) -> bool {
        if self.until.is_some_and(|until| now >= until) {
            self.until = None;
            true
        } else {
            false
        }
    }
}

static NEXT_APPROVAL_ID: AtomicU64 = AtomicU64::new(1);

fn next_approval_id() -> u64 {
    NEXT_APPROVAL_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub id: String,
    pub generation: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct VisibleApproval {
    pub session_id: String,
    pub request_id: u64,
}

/// Approval request held by the TUI until the reviewer answers.
pub(crate) struct PendingApproval {
    pub(crate) id: u64,
    pub(crate) tool_name: String,
    pub(crate) mode: PermissionMode,
    pub(crate) subject: ApprovalSubject,
    reply: Box<dyn FnOnce(ApprovalDecision) + Send>,
}

impl PendingApproval {
    pub(crate) fn from_request(request: ApprovalRequest) -> Self {
        Self {
            id: next_approval_id(),
            tool_name: request.tool_name,
            mode: request.mode,
            subject: request.subject,
            reply: Box::new(move |decision| {
                let _ = request.reply.send(decision);
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn mock<F>(
        tool_name: String,
        mode: PermissionMode,
        subject: ApprovalSubject,
        on_reply: F,
    ) -> Self
    where
        F: FnOnce(ApprovalDecision) + Send + 'static,
    {
        Self {
            id: next_approval_id(),
            tool_name,
            mode,
            subject,
            reply: Box::new(on_reply),
        }
    }

    pub(crate) fn respond(self, decision: ApprovalDecision) {
        (self.reply)(decision);
    }
}

pub(crate) struct Entry {
    pub(super) key: Key,
    pub title: String,
    pub session: Option<ManagedSession>,
    pub(super) prepared: Option<PreparedCreate>,
    pub busy: Option<&'static str>,
    pub work: Option<WorkRef>,
    pub execution: String,
    pub status: String,
    pub notice: String,
    pub editor: TextArea<'static>,
    pub transcript: Arc<Transcript>,
    pub preview: crate::preview::Preview,
    pub(crate) tools: super::tool_progress::ToolView,
    /// Expanded history before the current Context, lazily loaded.
    pub(crate) history: super::history::HistoryView,
    /// Wrapped rows of the loaded history display.
    pub(crate) history_rows: Option<Rows>,
    /// Scroll position to restore after earlier conversation is inserted above.
    pub(super) history_anchor: Option<Option<usize>>,
    #[cfg(test)]
    pub(super) record_reads: usize,
    pub(super) progress: Option<super::progress::ProgressFeed>,
    pub rows: Option<Rows>,
    pub scroll: Option<usize>,
    pub(super) material_version: Option<MaterialVersion>,
    pub(super) requested_material_version: Option<MaterialVersion>,
    pub(super) material_retry_after: Option<tokio::time::Instant>,
    pub(super) repair_job: Option<causa::kernel::BlockId>,
    #[cfg(test)]
    pub(super) material_reads: usize,
    pub(super) refreshing: bool,
    pub(super) wrapping: bool,
    pub(crate) approvals_rx: Option<ApprovalReceiver>,
    pub(crate) approval_queue: VecDeque<PendingApproval>,
    pub(crate) approval_reason: Option<String>,
    pub(crate) approval_scroll: usize,
}
impl Entry {
    pub(super) fn new(id: String, title: String) -> Self {
        Self {
            key: Key { id, generation: 0 },
            title,
            session: None,
            prepared: None,
            busy: None,
            work: None,
            execution: "No work in this instance".into(),
            status: "Stored; F3 to open".into(),
            notice: String::new(),
            editor: input::editor(),
            transcript: Arc::new(Transcript::default()),
            preview: crate::preview::Preview::default(),
            tools: super::tool_progress::ToolView::default(),
            history: super::history::HistoryView::new(),
            history_rows: None,
            history_anchor: None,
            #[cfg(test)]
            record_reads: 0,
            progress: None,
            rows: None,
            scroll: None,
            material_version: None,
            requested_material_version: None,
            material_retry_after: None,
            repair_job: None,
            #[cfg(test)]
            material_reads: 0,
            refreshing: false,
            wrapping: false,
            approvals_rx: None,
            approval_queue: VecDeque::new(),
            approval_reason: None,
            approval_scroll: 0,
        }
    }
    pub fn id(&self) -> &str {
        &self.key.id
    }
    pub fn has_draft(&self) -> bool {
        self.editor.lines().iter().any(|line| !line.is_empty())
    }
}
pub(crate) enum Confirmation {
    Quit,
    Close,
    Discard,
}
pub(crate) struct App {
    pub host: SessionHost,
    pub options: Options,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub form: Option<Form>,
    pub confirmation: Option<Confirmation>,
    pub panel: Option<super::commands::Panel>,
    pub viewport_height: usize,
    pub notice: String,
    pub quitting: bool,
    pub exit_reports: Vec<String>,
    pub(super) jobs: JoinSet<Message>,
    pub(super) listing: bool,
    pub(super) view_width: u16,
    pub(super) view_height: u16,
    pub(crate) too_small: bool,
    pub(super) dirty: bool,
    pub(super) done: bool,
    pub(super) progress_cursor: usize,
    pub(crate) clock: Clock,
    pub(crate) last_visible_approval: Option<VisibleApproval>,
    pub(crate) approval_guard: GuardWindow,
    /// The startup environment check; `None` once the main screen is reached.
    pub(crate) startup: Option<super::startup::Startup>,
    /// The checks and the installation the startup splash draws from.
    pub(crate) startup_backend: Arc<dyn super::startup::StartupBackend>,
    /// Shortened by tests so the timeout path needs no real wait.
    #[cfg(test)]
    pub(crate) test_startup_timeout: Duration,
    /// The probed sandbox, kept for `/status`.
    pub(crate) sandbox: Option<super::startup::SandboxCheck>,
    /// The resolved external tools bound to every session host this TUI opens.
    pub(crate) external_tools: Option<tars_core::external_tools::ExternalTools>,
    /// Why search cannot work, when the startup check found a reason.
    pub(crate) search_reason: Option<String>,
}
impl App {
    pub fn new(options: Options) -> Self {
        let host = SessionHost::new(Some(options.store.clone()));
        let startup_backend: Arc<dyn super::startup::StartupBackend> =
            Arc::new(super::startup::ProcessBackend::new(options.config.clone()));
        let notice = if let Some(warning) = options.config.warnings().first() {
            warning.chars().take(120).collect()
        } else {
            options
                .config_path
                .as_ref()
                .map(|path| format!("Config: {} · F2 new / F3 open", path.display()))
                .unwrap_or_else(|| "F2 creates a session. F3 opens the selection.".into())
        };
        Self {
            host,
            options,
            entries: Vec::new(),
            selected: 0,
            form: None,
            confirmation: None,
            panel: None,
            viewport_height: 12,
            notice,
            quitting: false,
            exit_reports: Vec::new(),
            jobs: JoinSet::new(),
            listing: false,
            view_width: 80,
            view_height: 24,
            too_small: false,
            dirty: true,
            done: false,
            progress_cursor: 0,
            clock: Clock::real(),
            last_visible_approval: None,
            approval_guard: GuardWindow::new(),
            startup: None,
            startup_backend,
            #[cfg(test)]
            test_startup_timeout: super::startup::STARTUP_TIMEOUT,
            sandbox: None,
            external_tools: None,
            search_reason: None,
        }
    }
    pub(super) fn entry_mut(&mut self, key: &Key) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.key == *key)
    }
    /// A pending confirmation needs room for its title, body and hint; below
    /// that size its decisions are not offered at all.
    pub(super) fn refresh_size_flags(&mut self) {
        let width = self.view_width.saturating_add(4);
        let pending = self
            .entries
            .get(self.selected)
            .is_some_and(|entry| !entry.approval_queue.is_empty());
        let approval_visible =
            pending && self.panel.is_none() && self.form.is_none() && self.confirmation.is_none();
        let minimum_height = if approval_visible { 19 } else { 12 };
        self.too_small = width < 30 || self.view_height < minimum_height;
    }
    pub(super) fn capacity(&mut self) -> bool {
        if self.jobs.len() >= MAX_JOBS {
            self.notice = "Background operation capacity reached; retry after completion".into();
            false
        } else {
            true
        }
    }
    /// The approval actually on screen, or `None` when it is obscured.
    pub(crate) fn current_visible_approval(&self) -> Option<VisibleApproval> {
        if self.panel.is_some()
            || self.form.is_some()
            || self.confirmation.is_some()
            || self.quitting
        {
            return None;
        }
        if self.too_small || self.view_height < 19 || self.view_width.saturating_add(4) < 30 {
            return None;
        }
        let entry = self.entries.get(self.selected)?;
        let front = entry.approval_queue.front()?;
        Some(VisibleApproval {
            session_id: entry.id().to_string(),
            request_id: front.id,
        })
    }
    /// Re-arm the 500 ms window whenever the visible approval changes, and mark
    /// the UI dirty when it starts or expires so the hint is redrawn.
    pub(crate) fn update_approval_guard(&mut self) {
        let current = self.current_visible_approval();
        if current != self.last_visible_approval {
            self.last_visible_approval = current;
            if self.last_visible_approval.is_some() {
                self.approval_guard
                    .arm(self.clock.now(), CONFIRM_GUARD_WINDOW);
                self.dirty = true;
            } else {
                self.approval_guard.disarm();
            }
        } else if self.approval_guard.check_expired(self.clock.now()) {
            self.dirty = true;
        }
    }
    pub(crate) fn is_approval_guarded(&self) -> bool {
        let current = self.current_visible_approval();
        if current.is_none() {
            return false;
        }
        if current != self.last_visible_approval {
            return true;
        }
        self.approval_guard.is_guarded(self.clock.now())
    }
    /// Mark the UI dirty when the repair guard expires so the hint is redrawn.
    pub(crate) fn update_repair_guard(&mut self) {
        if let Some(super::Panel::Repair(panel)) = self.panel.as_mut()
            && panel.guard.check_expired(self.clock.now())
        {
            self.dirty = true;
        }
    }
    pub(crate) fn is_repair_guarded(&self) -> bool {
        match &self.panel {
            Some(super::Panel::Repair(panel)) if panel.plan.is_some() => {
                panel.guard.is_guarded(self.clock.now())
            }
            _ => false,
        }
    }
    #[cfg(test)]
    pub(crate) fn advance_clock(&mut self, duration: Duration) {
        self.clock.advance(duration);
        self.update_approval_guard();
        self.update_repair_guard();
        self.update_startup_guard();
    }
}
