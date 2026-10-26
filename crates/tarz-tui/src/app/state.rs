use super::jobs::Message;
use crate::{
    input::{self, Form},
    options::Options,
    transcript::{Rows, Transcript},
};
use ratatui_textarea::TextArea;
use std::collections::VecDeque;
use std::sync::Arc;
use tarz_core::application::{ManagedSession, MaterialVersion, PreparedCreate, SessionHost};
use tarz_core::permission::{
    ApprovalDecision, ApprovalReceiver, ApprovalRequest, ApprovalSubject, PermissionMode,
};
use tarz_core::session::WorkRef;
use tokio::task::JoinSet;

// Limits UI-owned in-flight work/results; core separately limits active instances.
pub(super) const MAX_JOBS: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub id: String,
    pub generation: u64,
}

/// Pending approval request held by the TUI for reviewer confirmation.
pub(crate) struct PendingApproval {
    pub(crate) tool_name: String,
    pub(crate) mode: PermissionMode,
    pub(crate) subject: ApprovalSubject,
    reply: Box<dyn FnOnce(ApprovalDecision) + Send>,
}

impl PendingApproval {
    pub(crate) fn from_request(request: ApprovalRequest) -> Self {
        Self {
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
}
impl App {
    pub fn new(options: Options) -> Self {
        let host = SessionHost::new(Some(options.store.clone()));
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
}
