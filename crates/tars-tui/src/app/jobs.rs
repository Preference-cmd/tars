use super::state::{App, Entry, Key};
use crate::transcript::{Rows, Transcript};
use std::sync::Arc;
use tars_core::application::{
    ApplicationError, CloseOutcome, ManagedSession, MaterialVersion, SaveStatus, SessionListing,
};
use tars_core::session::{ModelFailure, WorkRef, WorkResult};

pub(super) enum Message {
    RepairInspected(
        Key,
        causa::kernel::BlockId,
        Result<(tars_core::application::RepairInspection, Option<String>), String>,
    ),
    RepairPlanned(
        Key,
        causa::kernel::BlockId,
        Result<tars_core::application::RepairPlan, String>,
    ),
    RepairApplied(
        Key,
        causa::kernel::BlockId,
        bool,
        Result<SaveStatus, String>,
    ),
    Listed(Result<Vec<SessionListing>, ApplicationError>),
    Opened(Key, Result<ManagedSession, ApplicationError>),
    Finished(
        Key,
        WorkRef,
        Result<
            (
                Arc<WorkResult>,
                Arc<tars_core::session::ToolProgressSnapshot>,
            ),
            String,
        >,
    ),
    Materials(Key, Result<(MaterialVersion, Arc<Transcript>), String>),
    Wrapped(Key, Rows),
    Retried(Key, Result<SaveStatus, ApplicationError>),
    Exported(Key, Result<(String, bool), String>),
    Closed(Key, Result<CloseOutcome, ApplicationError>),
    JobPanicked(Option<Key>, JobKind, String),
}

/// What a UI adapter job owns, so a caught panic clears only that state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JobKind {
    List,
    Open,
    Work,
    Close,
    Retry,
    Export,
    Materials,
    Wrapping,
    Repair(causa::kernel::BlockId),
}

pub(super) fn save_label(save: &SaveStatus) -> String {
    match save {
        SaveStatus::NotTracked => "temporary".into(),
        SaveStatus::Saved { revision, .. } => format!("saved r{revision}"),
        SaveStatus::Dirty { .. } => "unsaved / running".into(),
        SaveStatus::Saving { .. } => "saving".into(),
        SaveStatus::SaveBlocked { .. } => {
            "SAVE BLOCKED - Ctrl-R retry / Ctrl-E export / Ctrl-D discard".into()
        }
    }
}
fn result_label(result: &WorkResult) -> &'static str {
    use causa::{
        kernel::ControlError,
        runtime::{TurnInterruption, TurnResult},
    };
    match result {
        WorkResult::Run(TurnResult::Completed { .. }) => "Completed",
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::Cancelled { .. },
        }) => "Cancelled",
        WorkResult::StoppedBeforeRun(ControlError::TimedOut)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::DeadlineExceeded { .. },
        }) => "Timed out",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelMaxTokens { .. },
        }) => "Output reached max_tokens; raise the limit and submit new work",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelContextWindowExceeded { .. },
        }) => "Conversation no longer fits the model's context window",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelRefusal { .. },
        }) => "The model refused the request",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelStopUnrecognized { .. },
        }) => "Provider stopped for an unrecognized reason",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxModelRounds { .. },
        }) => "Model round limit reached; submit new work to continue",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxToolCalls { .. },
        }) => "Tool call limit reached; submit new work to continue",
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelCallFailed { error, .. },
        }) => match ModelFailure::of(error).kind {
            "invalid_request" => {
                "Request rejected as invalid; check model and max_tokens (required by anthropic)"
            }
            "transient" => "Temporary provider failure; retry later or set max_model_attempts",
            "unknown_outcome" => "Response stream ended early; outcome unknown",
            "credential_rejected" => "Credential rejected; check the API key",
            "not_found" => "Model or endpoint not found; check the model name and endpoint URL",
            "undecodable" => "Response could not be read; check the endpoint's protocol",
            _ => "Failed; check model, endpoint and credential configuration",
        },
        WorkResult::Faulted(_) => "Worker fault; materials may be unavailable",
        _ => "Failed",
    }
}
/// Longest notice shown after opening, in bytes.
const MAX_OPENED_NOTICE_BYTES: usize = 256;

/// The one-line notice after opening: a saved-mode downgrade, an unavailable
/// command sandbox and the first MCP server that did not start, with counts,
/// cut to [`MAX_OPENED_NOTICE_BYTES`]. The fixed command clause comes before the
/// free-form MCP reason so truncation cannot hide it. Status text is escaped
/// upstream.
pub(super) fn opened_notice(
    status: &[tars_core::McpServerStatus],
    downgraded: bool,
    command_notice: Option<&str>,
) -> String {
    let prefix = if downgraded {
        "Opened; permission mode reverted from full-access to ask; "
    } else {
        "Opened; "
    };
    let unavailable: Vec<_> = status.iter().filter(|server| !server.ready).collect();
    let skipped: usize = status.iter().map(|server| server.skipped_tools.len()).sum();
    let mut notice = prefix.to_owned();
    if let Some(clause) = command_notice {
        notice.push_str(clause);
        notice.push_str("; ");
    }
    if let Some(first) = unavailable.first() {
        notice.push_str(&format!(
            "MCP server '{}' is unavailable: {}",
            first.id,
            first.reason.as_deref().unwrap_or("unknown reason")
        ));
    } else {
        notice.push_str("only materials are restored, not old work records");
    }
    if unavailable.len() > 1 {
        notice.push_str(&format!(" (+{} more unavailable)", unavailable.len() - 1));
    }
    if skipped > 0 {
        notice.push_str(&format!("; {skipped} MCP tools skipped"));
    }
    if notice.len() > MAX_OPENED_NOTICE_BYTES {
        let mut end = MAX_OPENED_NOTICE_BYTES;
        while !notice.is_char_boundary(end) {
            end -= 1;
        }
        notice.truncate(end);
    }
    notice
}

/// The gateway's description of a failed model call, for the session notice.
fn failure_notice(result: &WorkResult) -> Option<String> {
    use causa::runtime::{TurnInterruption, TurnResult};
    match result {
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelCallFailed { error, .. },
        }) => Some(format!("Model error: {}", ModelFailure::of(error).detail)),
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelStopUnrecognized { output, .. },
        }) => Some(ModelFailure::unrecognized_stop(output).detail),
        _ => None,
    }
}
pub(super) fn close_label(outcome: &CloseOutcome) -> String {
    use tars_core::application::CommitDisposition;
    let confirmation = match &outcome.pending_commit {
        None => "no pending commit".to_owned(),
        Some(CommitDisposition::Applied { .. }) => "pending commit applied".into(),
        Some(CommitDisposition::NotApplied) => "pending commit not applied".into(),
        Some(CommitDisposition::NotSubmitted) => "pending commit not submitted".into(),
        Some(CommitDisposition::Unresolved { error }) => {
            format!("pending commit UNRESOLVED: {error}")
        }
    };
    format!(
        "Closed: {}; {confirmation}; discarded unconfirmed: {}",
        save_label(&outcome.save),
        outcome.discarded_unconfirmed
    )
}

impl App {
    pub(super) fn complete(&mut self, message: Message) {
        self.dirty = true;
        match message {
            Message::RepairInspected(key, request, result) => {
                self.repair_inspected(key, request, result)
            }
            Message::RepairPlanned(key, request, result) => {
                self.repair_planned(key, request, result)
            }
            Message::RepairApplied(key, request, applied, result) => {
                self.repair_applied(key, request, applied, result)
            }
            Message::Listed(result) => {
                self.listing = false;
                match result {
                    Err(error) => self.notice = error.to_string(),
                    Ok(listed) => {
                        for item in listed {
                            let id = item.conversation_id.as_str().to_owned();
                            if self.entries.iter().any(|e| e.id() == id) {
                                continue;
                            }
                            let mut entry = Entry::new(
                                id,
                                item.record
                                    .as_ref()
                                    .map(|r| r.definition.model.clone())
                                    .unwrap_or_else(|_| "invalid record".into()),
                            );
                            entry.notice = match (item.record, item.active) {
                                (Err(error), _) => format!("Invalid record: {error}"),
                                (_, Err(error)) => format!("Lock status unavailable: {error}"),
                                (_, Ok(true)) => "Active in another process".into(),
                                _ => String::new(),
                            };
                            self.entries.push(entry);
                        }
                    }
                }
            }
            Message::Opened(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.busy = None;
                    match result {
                        Ok(session) => {
                            entry.approvals_rx = Some(session.connect_approvals());
                            entry.approval_queue.clear();
                            entry.approval_reason = None;
                            entry.approval_scroll = 0;
                            entry.title = session.definition().model;
                            let command_notice = session
                                .command_status()
                                .filter(|status| !status.is_available())
                                .map(|_| "command sandbox unavailable (see /status)");
                            entry.notice = opened_notice(
                                &session.mcp_status(),
                                session.open_report().permission_downgraded,
                                command_notice,
                            );
                            entry.session = Some(session);
                            entry.prepared = None;
                        }
                        Err(error) => {
                            entry.notice =
                                format!("Open failed: {error}; F3 retries the same operation")
                        }
                    }
                }
            }
            Message::Finished(key, work, result) => {
                if let Some(entry) = self.entry_mut(&key)
                    && entry.work.as_ref() == Some(&work)
                {
                    if let Ok((_, tools)) = &result {
                        entry.tools.reconcile(tools);
                    }
                    entry.clear_progress();
                    entry.approval_queue.clear();
                    entry.approval_reason = None;
                    entry.approval_scroll = 0;
                    entry.scroll = None;
                    entry.execution = result
                        .as_ref()
                        .map(|(result, _)| result_label(result))
                        .unwrap_or("Observation failed")
                        .into();
                    if let Err(error) = result {
                        entry.notice = error;
                    } else if let Some(notice) = result
                        .as_ref()
                        .ok()
                        .and_then(|(result, _)| failure_notice(result))
                    {
                        entry.notice = notice;
                    } else if entry.notice.starts_with("Input accepted")
                        || entry.notice.starts_with("Cancellation requested")
                    {
                        entry.notice.clear();
                    }
                }
            }
            Message::Materials(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.refreshing = false;
                    match result {
                        Ok((version, transcript)) => {
                            entry.material_version = Some(version);
                            entry.material_retry_after = None;
                            entry.transcript = transcript;
                            entry.rows = None;
                            if entry.notice.starts_with("Material view unavailable") {
                                entry.notice.clear();
                            }
                        }
                        Err(error) => {
                            // A failed read is retried after a delay, not once per frame.
                            entry.requested_material_version = None;
                            entry.material_retry_after = Some(
                                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                            entry.notice = format!("Material view unavailable: {error}");
                        }
                    }
                }
            }
            Message::Wrapped(key, rows) => {
                let width = self.view_width;
                if let Some(entry) = self.entry_mut(&key) {
                    entry.wrapping = false;
                    if Arc::ptr_eq(&rows.transcript, &entry.transcript) && rows.width == width {
                        entry.rows = Some(rows);
                    }
                }
            }
            Message::Retried(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.busy = None;
                    entry.notice = match result {
                        Ok(status) => save_label(&status),
                        Err(error) => error.to_string(),
                    };
                }
            }
            Message::Exported(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.busy = None;
                    entry.notice = match result {
                        Ok((path, importable)) => format!(
                            "Exported actual materials: {path}{}",
                            super::actions::export_warning(importable)
                        ),
                        Err(error) => format!("Export failed: {error}"),
                    };
                }
            }
            Message::Closed(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.busy = None;
                    entry.approvals_rx = None;
                    entry.approval_queue.clear();
                    entry.approval_reason = None;
                    entry.approval_scroll = 0;
                    match &result {
                        Ok(outcome) => {
                            entry.session = None;
                            entry.prepared = None;
                            entry.work = None;
                            entry.execution = "No work in this instance".into();
                            entry.clear_progress();
                            entry.tools = super::tool_progress::ToolView::default();
                            entry.editor = crate::input::editor();
                            entry.status = "Closed; F3 to reopen".into();
                            entry.notice = close_label(outcome);
                        }
                        Err(error) => {
                            entry.notice = format!(
                                "Close failed: {error}; owner retained. Retry, export or explicitly discard."
                            )
                        }
                    }
                }
                match result {
                    Ok(outcome) => {
                        if self.notice.starts_with("Exit paused")
                            && !self
                                .entries
                                .iter()
                                .any(|entry| entry.notice.starts_with("Close failed"))
                        {
                            self.notice.clear();
                        }
                        self.exit_reports
                            .push(format!("{}: {}", key.id, close_label(&outcome)))
                    }
                    Err(_) => {
                        self.quitting = false;
                        self.notice = "Exit paused: a session could not be saved. Resolve it before quitting.".into();
                    }
                }
            }
            Message::JobPanicked(key, kind, detail) => {
                let detail = tars_core::permission::escape_untrusted_text(&detail);
                let message = format!("Background task failed: {detail}");
                if kind == JobKind::List {
                    self.listing = false;
                    self.notice = message;
                    return;
                }
                let Some(key) = key else {
                    self.notice = message;
                    return;
                };
                let mut panicked_repair = None;
                let mut panicked_close = false;
                if let Some(entry) = self.entry_mut(&key) {
                    let clear_busy = |entry: &mut Entry, label: &str| {
                        if entry.busy == Some(label) {
                            entry.busy = None;
                        }
                    };
                    match kind {
                        JobKind::List => unreachable!("handled before the entry lookup"),
                        JobKind::Materials => {
                            entry.refreshing = false;
                            entry.requested_material_version = None;
                            entry.material_retry_after = Some(
                                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                            entry.notice = message.clone();
                        }
                        JobKind::Wrapping => {
                            entry.wrapping = false;
                            entry.notice = message.clone();
                        }
                        JobKind::Repair(request) => {
                            clear_busy(entry, "repairing materials");
                            if entry.repair_job == Some(request) {
                                entry.repair_job = None;
                            }
                            panicked_repair = Some(request);
                            entry.notice = message.clone();
                        }
                        JobKind::Open => {
                            clear_busy(entry, "opening");
                            entry.notice = message.clone();
                        }
                        JobKind::Close => {
                            clear_busy(entry, "closing");
                            panicked_close = true;
                            entry.notice = message.clone();
                        }
                        JobKind::Retry => {
                            clear_busy(entry, "retrying save");
                            entry.notice = message.clone();
                        }
                        JobKind::Export => {
                            clear_busy(entry, "exporting");
                            entry.notice = message.clone();
                        }
                        JobKind::Work => {
                            // The core work may still run; only the local
                            // observation adapter is gone.
                            entry.notice = message.clone();
                        }
                    }
                }
                if panicked_close && self.quitting {
                    self.quitting = false;
                    self.notice =
                        "Exit paused: a session close failed. Resolve it before quitting.".into();
                }
                if let Some(request) = panicked_repair
                    && let Some(super::Panel::Repair(panel)) = &mut self.panel
                    && panel.key == key
                    && panel.request == request
                {
                    panel.busy = false;
                    panel.lines = vec![message];
                }
            }
        }
    }

    /// Run one UI adapter job and turn a panic into a session-scoped message.
    /// The event loop keeps running so unrelated sessions stay usable.
    pub(super) fn spawn_job<F>(&mut self, key: Option<Key>, kind: JobKind, job: F)
    where
        F: std::future::Future<Output = Message> + Send + 'static,
    {
        spawn_job(&mut self.jobs, key, kind, job);
    }
}

/// Run blocking UI work with the same panic-hook suppression as a job poll.
pub(super) async fn blocking<F, T>(work: F) -> Result<T, tokio::task::JoinError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _guard = crate::terminal::SuppressTerminalRestore::enter();
        work()
    })
    .await
}

/// Field-taking form for call sites already borrowing `entries`.
pub(super) fn spawn_job<F>(
    jobs: &mut tokio::task::JoinSet<Message>,
    key: Option<Key>,
    kind: JobKind,
    job: F,
) where
    F: std::future::Future<Output = Message> + Send + 'static,
{
    jobs.spawn(async move {
        use futures_util::FutureExt;
        let job = SuppressRestore {
            inner: Box::pin(job),
        };
        match std::panic::AssertUnwindSafe(job).catch_unwind().await {
            Ok(message) => message,
            Err(payload) => Message::JobPanicked(key, kind, panic_detail(payload)),
        }
    });
}

/// Keep the terminal panic hook from restoring raw mode when the panic is
/// caught by the job wrapper. The flag is set around each poll because a
/// future may move between runtime worker threads.
struct SuppressRestore<F> {
    inner: std::pin::Pin<Box<F>>,
}

impl<F: std::future::Future> std::future::Future for SuppressRestore<F> {
    type Output = F::Output;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let _guard = crate::terminal::SuppressTerminalRestore::enter();
        self.inner.as_mut().poll(context)
    }
}

fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "UI adapter task panicked".into()
    }
}
