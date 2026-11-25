use super::state::{App, Entry, Key};
use crate::transcript::{Rows, Transcript};
use std::sync::Arc;
use tars_core::application::{
    ApplicationError, CloseOutcome, ManagedSession, MaterialVersion, SaveStatus, SessionListing,
    WorkOutcome,
};
use tars_core::compact::{CompactionOutcome, HistoryRecord, HistoryRecordEntry};
use tars_core::session::{WorkRef, WorkResult};

/// One material read: the current Context's rows and its record index.
pub(super) struct MaterialView {
    pub(super) version: MaterialVersion,
    pub(super) transcript: Arc<Transcript>,
    pub(super) history: Vec<HistoryRecordEntry>,
    /// The Context's leading blocks that a rewrite inserted.
    pub(super) prefix: Vec<causa::kernel::ContextBlock>,
    /// IDs of blocks outside `history_range` (fixed prefix and current conversation).
    pub(super) live_tail_ids: std::collections::HashSet<causa::kernel::BlockId>,
    /// Rewrite records known in memory.
    pub(super) records: Vec<Arc<HistoryRecord>>,
}

impl MaterialView {
    /// Build the view of one snapshot off the UI thread.
    pub(super) fn read(session: &ManagedSession) -> Result<Self, String> {
        let snapshot = session
            .read_materials()
            .map_err(|error| error.to_string())?;
        let transcript = Transcript::from_materials(&snapshot.context, &snapshot.history);
        let blocks = snapshot.context.blocks();
        let prefix = blocks[transcript.history_range.clone()].to_vec();
        let live_tail_ids = blocks[..transcript.history_range.start]
            .iter()
            .chain(&blocks[transcript.history_range.end..])
            .map(causa::kernel::ContextBlock::id)
            .collect();
        let records = session.written_history_records();
        Ok(Self {
            version: snapshot.version,
            transcript: Arc::new(transcript),
            history: snapshot.history,
            prefix,
            live_tail_ids,
            records,
        })
    }
}

/// Materials and history reloaded after one explicit compaction.
pub(super) struct CompactedReload {
    pub(super) outcome: CompactionOutcome,
    pub(super) view: MaterialView,
}

pub(super) enum AgentMaterialsError {
    Busy,
    Failed(String),
}

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
    WorkspaceRulesTrusted(Key, Result<usize, ApplicationError>),
    WorkspaceAgentsTrusted(Key, Result<usize, ApplicationError>),
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
    Materials(Key, Result<MaterialView, String>),
    AgentMaterials(Key, String, Result<Arc<Transcript>, AgentMaterialsError>),
    HistoryLoaded(Key, String, Result<HistoryRecord, String>),
    ToolArtifactsLoaded(Key, Option<MaterialVersion>, Vec<String>, usize),
    Compacted(Key, Result<CompactedReload, String>),
    Wrapped(Key, Rows, Option<Rows>),
    Retried(Key, Result<SaveStatus, ApplicationError>),
    Exported(Key, Result<(String, bool), String>),
    Closed(Key, Result<CloseOutcome, ApplicationError>),
    AgentOperationCancelled(Key, String, Result<bool, ApplicationError>),
    /// The startup sandbox check finished.
    StartupSandbox(
        super::startup::SandboxCheck,
        Option<tars_core::sandbox::SandboxStatus>,
    ),
    /// The startup tool check (or its re-resolve after an installation) finished.
    StartupTools(super::startup::ToolCheck),
    /// The consent-gated installation finished; the reason is core's own text.
    StartupInstalled(Result<(), String>),
    /// `/doctor` diagnostic lines finished loading.
    DoctorLoaded(Vec<String>),
    JobPanicked(Option<Key>, JobKind, String),
}

/// What a UI adapter job owns, so a caught panic clears only that state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JobKind {
    List,
    Open,
    TrustWorkspaceRules,
    TrustWorkspaceAgents,
    Work,
    Close,
    Retry,
    Export,
    Materials,
    AgentMaterials,
    HistoryRead,
    ToolArtifacts,
    Compact,
    Wrapping,
    CancelAgentOperation,
    Repair(causa::kernel::BlockId),
    /// One of the two startup checks.
    Startup,
    /// The consent-gated managed installation.
    StartupInstall,
    /// Read-only `/doctor` diagnostics.
    Doctor,
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
/// Longest notice shown after opening, in bytes.
const MAX_OPENED_NOTICE_BYTES: usize = 256;

/// The fixed clause shown when the session's command sandbox is unavailable.
pub(super) const COMMAND_UNAVAILABLE_NOTICE: &str = "command sandbox unavailable (see /status)";

/// The startup clause for a probed command sandbox: only an unavailable sandbox
/// adds a notice; `/status` always shows the full outcome.
pub(super) fn command_sandbox_notice(available: bool) -> Option<&'static str> {
    (!available).then_some(COMMAND_UNAVAILABLE_NOTICE)
}

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

/// The core-supplied notice for a non-completed work outcome, with `/compact`
/// guidance added for context-window exhaustion.
fn outcome_notice(outcome: &WorkOutcome) -> Option<String> {
    if outcome.kind == "completed" {
        return None;
    }
    if outcome.kind == "context_window_exceeded" {
        let base = outcome
            .hint
            .as_deref()
            .unwrap_or("compact the session before submitting new work");
        return Some(format!("{base}; run /compact"));
    }
    if let Some(failure) = &outcome.model_failure {
        let detail = tars_core::permission::escape_untrusted_text(&failure.detail);
        return Some(match &outcome.hint {
            Some(hint) => format!("Model error: {detail} ({hint})"),
            None => format!("Model error: {detail}"),
        });
    }
    match (&outcome.hint, &outcome.detail) {
        (Some(hint), Some(detail)) => {
            let detail = tars_core::permission::escape_untrusted_text(detail);
            Some(format!("{hint}: {detail}"))
        }
        (Some(hint), None) => Some(hint.clone()),
        (None, Some(detail)) => Some(tars_core::permission::escape_untrusted_text(detail)),
        (None, None) => None,
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
                            let (model, title, updated_at_ms, prompt_cache, code_execution) =
                                match &item.record {
                                    Ok(summary) => (
                                        summary.definition.model.clone(),
                                        summary.title.clone(),
                                        summary.updated_at_ms,
                                        summary.definition.prompt_cache,
                                        summary.definition.code_execution,
                                    ),
                                    Err(_) => ("invalid record".into(), None, 0, true, None),
                                };
                            let mut entry = Entry::new(id, model);
                            entry.title = title;
                            if updated_at_ms > 0 {
                                entry.updated_at_ms = updated_at_ms;
                            }
                            entry.prompt_cache = prompt_cache;
                            entry.code_execution = code_execution;
                            entry.notice = match (item.record, item.active) {
                                (Err(error), _) => format!("Invalid record: {error}"),
                                (_, Err(error)) => format!("Lock status unavailable: {error}"),
                                (_, Ok(true)) => "Active in another process".into(),
                                _ => String::new(),
                            };
                            self.entries.push(entry);
                        }
                        if self.entries.is_empty()
                            && self.options.auto_onboard
                            && !self.onboarding_dismissed
                            && self.onboarding.is_none()
                            && self.form.is_none()
                            && self.panel.is_none()
                            && self.confirmation.is_none()
                            && !self.quitting
                        {
                            self.onboarding = Some(crate::input::OnboardingForm::new(
                                std::env::current_dir().ok(),
                            ));
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
                            entry.session_events_rx = Some(session.subscribe_events());
                            entry.active_bg_count = session.active_finite_background_tasks();
                            entry.viewed_agent_id = None;
                            entry.viewed_agent_label = None;
                            entry.viewed_agent_dirty = false;
                            entry.approval_queue.clear();
                            entry.approval_reason = None;
                            entry.approval_scroll = 0;
                            entry.model = session.definition().model;
                            entry.title = session.title();
                            entry.updated_at_ms = session.updated_at_ms();
                            entry.prompt_cache = session.definition().prompt_cache;
                            entry.code_execution = session.definition().code_execution;
                            let report = session.open_report();
                            entry.workspace_rules_trust =
                                super::state::WorkspaceRulesTrustPrompt::from_report(
                                    &report.workspace_rules,
                                );
                            entry.workspace_agents_trust =
                                super::state::WorkspaceAgentsTrustPrompt::from_report(
                                    &report.workspace_agents,
                                );
                            let command_notice = session
                                .command_status()
                                .and_then(|status| command_sandbox_notice(status.is_available()));
                            entry.notice = opened_notice(
                                &session.mcp_status(),
                                report.permission_downgraded,
                                command_notice,
                            );
                            entry.session = Some(session);
                            entry.prepared = None;
                            self.refresh_size_flags();
                            self.update_workspace_trust_guard();
                        }
                        Err(error) => {
                            entry.notice =
                                format!("Open failed: {error}; F3 retries the same operation")
                        }
                    }
                }
            }
            Message::WorkspaceRulesTrusted(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    if entry.busy == Some("trusting workspace rules") {
                        entry.busy = None;
                    }
                    entry.notice = match result {
                        Ok(count) => {
                            format!("Trusted workspace rules; {count} allow rule(s) active")
                        }
                        Err(error) => format!("Failed to trust workspace rules: {error}"),
                    };
                }
            }
            Message::WorkspaceAgentsTrusted(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    if entry.busy == Some("trusting workspace agents") {
                        entry.busy = None;
                    }
                    entry.notice = match result {
                        Ok(count) => {
                            format!("Trusted workspace agents; {count} preset(s) active")
                        }
                        Err(error) => format!("Failed to trust workspace agents: {error}"),
                    };
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
                    let outcome = result.as_ref().map(|(result, _)| WorkOutcome::of(result));
                    entry.execution = outcome
                        .as_ref()
                        .map(|outcome| outcome.summary.clone())
                        .unwrap_or_else(|_| "Observation failed".to_owned());
                    if let Err(error) = result {
                        entry.notice = error;
                    } else if let Some(notice) = outcome.as_ref().ok().and_then(outcome_notice) {
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
                    // Discard stale root reads when the user has switched to a child agent view.
                    if entry.viewed_agent_id.is_some() {
                        entry.requested_material_version = None;
                        return;
                    }
                    entry.refreshing = false;
                    // A late material read for an instance that already closed
                    // must not rebuild a conversation view with no session.
                    if entry.session.is_none() {
                        return;
                    }
                    match result {
                        Ok(view) => {
                            // A read started before `/compact` would show the old
                            // materials; the compaction result reloads them.
                            if entry.busy == Some("compacting") {
                                entry.requested_material_version = None;
                                return;
                            }
                            // The requested version is already on screen (installed by a
                            // reload such as compaction), so another version came from an
                            // earlier read. A newer one dropped here is read again once the
                            // save status moves past the requested version.
                            if entry.requested_material_version.is_some()
                                && entry.material_version == entry.requested_material_version
                                && Some(&view.version) != entry.requested_material_version.as_ref()
                            {
                                return;
                            }
                            // A rewrite made live supplies its rewrite records in
                            // memory, so history is expanded without file reads.
                            let rebuilt = entry.history.refresh(
                                view.history,
                                view.prefix,
                                &view.live_tail_ids,
                                &view.records,
                            );
                            if rebuilt {
                                entry.capture_scroll_anchor();
                                entry.history_anchor = None;
                            }
                            entry.material_version = Some(view.version);
                            entry.material_retry_after = None;
                            entry.transcript = view.transcript;
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
            Message::AgentMaterials(key, agent_id, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    if entry.session.is_none()
                        || entry.viewed_agent_id.as_deref() != Some(agent_id.as_str())
                    {
                        return;
                    }
                    entry.refreshing = false;
                    if let Some(session) = &entry.session {
                        entry.viewed_agent_label =
                            session.agent_snapshot(&agent_id).map(|snap| snap.name);
                    }
                    match result {
                        Ok(transcript) => {
                            entry.transcript = transcript;
                            entry.rows = None;
                            if entry.notice.starts_with("Agent materials unavailable")
                                || (entry.notice.starts_with("Child agent ")
                                    && entry
                                        .notice
                                        .contains("is running; materials will load when idle"))
                            {
                                entry.notice.clear();
                            }
                        }
                        Err(AgentMaterialsError::Busy) => {
                            let label = entry
                                .viewed_agent_label
                                .as_deref()
                                .unwrap_or(agent_id.as_str());
                            entry.notice = format!(
                                "Child agent {} is running; materials will load when idle",
                                tars_core::permission::escape_untrusted_text(label)
                            );
                        }
                        Err(AgentMaterialsError::Failed(error)) => {
                            entry.notice = format!(
                                "Agent materials unavailable: {}",
                                tars_core::permission::escape_untrusted_text(&error)
                            );
                        }
                    }
                }
            }
            Message::HistoryLoaded(key, record, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    match result {
                        Ok(document) => entry.history.insert_loaded(document),
                        Err(error) => entry.history.mark_unreadable(&record, error),
                    }
                }
            }
            Message::ToolArtifactsLoaded(key, version, lines, reads) => {
                #[cfg(not(test))]
                let _ = reads;
                if let Some(entry) = self.entry_mut(&key) {
                    #[cfg(test)]
                    {
                        entry.artifact_reads = entry.artifact_reads.saturating_add(reads);
                    }
                    if entry.material_version == version
                        && self
                            .entries
                            .get(self.selected)
                            .is_some_and(|selected| selected.key == key)
                        && let Some(super::commands::Panel::Info {
                            title,
                            lines: panel_lines,
                            ..
                        }) = &mut self.panel
                        && title == "Recorded tool materials · viewing never executes"
                    {
                        *panel_lines = lines;
                    }
                }
            }
            Message::Compacted(key, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    entry.busy = None;
                    match result {
                        Ok(reload) => {
                            let outcome = reload.outcome;
                            let view = reload.view;
                            let notice = format!(
                                "Compacted ({}, {} → {} tokens); the replaced conversation remains above",
                                outcome.trigger.name(),
                                super::history::estimate_label(outcome.estimate_before),
                                super::history::estimate_label(outcome.estimate_after),
                            );
                            if entry.viewed_agent_id.is_none() {
                                let rebuilt = entry.history.refresh(
                                    view.history,
                                    view.prefix,
                                    &view.live_tail_ids,
                                    &view.records,
                                );
                                if rebuilt {
                                    entry.capture_scroll_anchor();
                                    entry.history_anchor = None;
                                }
                                entry.material_version = Some(view.version.clone());
                                entry.requested_material_version = Some(view.version);
                                entry.material_retry_after = None;
                                entry.transcript = view.transcript;
                                entry.rows = None;
                            } else {
                                entry.requested_material_version = None;
                            }
                            entry.notice = notice;
                        }
                        Err(error) => entry.notice = format!("Compaction failed: {error}"),
                    }
                }
            }
            Message::Wrapped(key, rows, history_rows) => {
                let width = self.view_width;
                if let Some(entry) = self.entry_mut(&key) {
                    entry.wrapping = false;
                    let accepted =
                        Arc::ptr_eq(&rows.transcript, &entry.transcript) && rows.width == width;
                    if accepted {
                        entry.rows = Some(rows);
                    }
                    let previous = entry
                        .history_rows
                        .as_ref()
                        .map_or(0, |rows| rows.ranges.len());
                    match history_rows {
                        Some(history_rows) => {
                            let current = entry.history.display();
                            if current.as_ref().is_some_and(|display| {
                                Arc::ptr_eq(&history_rows.transcript, display)
                            }) && history_rows.width == width
                            {
                                let added = history_rows.ranges.len().saturating_sub(previous);
                                entry.history_rows = Some(history_rows);
                                if added > 0
                                    && let Some(anchor) = entry.history_anchor.take()
                                    && entry.scroll == anchor
                                    && let Some(scroll) = anchor
                                {
                                    // Keep the rows being read in place; the
                                    // regenerated header may scroll away.
                                    entry.scroll = Some(scroll.saturating_add(added));
                                }
                            }
                        }
                        None => entry.history_rows = None,
                    }
                    if accepted {
                        entry.restore_scroll_anchor();
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
            Message::StartupSandbox(check, raw) => self.startup_sandbox(check, raw),
            Message::StartupTools(check) => self.startup_tools(check),
            Message::StartupInstalled(result) => self.startup_installed(result),
            Message::DoctorLoaded(lines) => {
                if let Some(super::commands::Panel::Info {
                    title,
                    lines: panel_lines,
                    scroll,
                }) = &mut self.panel
                    && title == super::commands::DOCTOR_PANEL_TITLE
                {
                    *panel_lines = lines;
                    *scroll = 0;
                }
            }
            Message::AgentOperationCancelled(key, agent_name, result) => {
                if let Some(entry) = self.entry_mut(&key) {
                    let escaped = tars_core::permission::escape_untrusted_text(&agent_name);
                    entry.notice = match result {
                        Ok(true) => format!("Cancelled running operation for agent {escaped}"),
                        Ok(false) => format!("Agent {escaped} has no running operation"),
                        Err(error) => format!("Failed to cancel agent {escaped}: {error}"),
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
                            let was_viewing_child = entry.viewed_agent_id.take().is_some();
                            entry.session = None;
                            entry.session_events_rx = None;
                            entry.active_bg_count = 0;
                            entry.viewed_agent_label = None;
                            entry.viewed_agent_dirty = false;
                            if was_viewing_child {
                                entry.transcript = Arc::new(Transcript::default());
                                entry.rows = None;
                                entry.scroll = None;
                            }
                            entry.workspace_rules_trust = None;
                            entry.workspace_agents_trust = None;
                            entry.prepared = None;
                            entry.work = None;
                            entry.execution = "No work in this instance".into();
                            entry.clear_progress();
                            entry.clear_history();
                            entry.tools = super::tool_progress::ToolView::default();
                            entry.editor = crate::input::editor();
                            entry.reset_history_cursor();
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
                if matches!(kind, JobKind::Startup | JobKind::StartupInstall) {
                    self.startup_failed(&detail);
                    return;
                }
                if kind == JobKind::Doctor {
                    if let Some(super::commands::Panel::Info {
                        title,
                        lines: panel_lines,
                        scroll,
                    }) = &mut self.panel
                        && title == super::commands::DOCTOR_PANEL_TITLE
                    {
                        *panel_lines = vec![message.clone()];
                        *scroll = 0;
                    }
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
                        JobKind::Startup | JobKind::StartupInstall | JobKind::Doctor => {
                            unreachable!("handled before the entry lookup")
                        }
                        JobKind::Materials | JobKind::AgentMaterials => {
                            entry.refreshing = false;
                            entry.requested_material_version = None;
                            entry.material_retry_after = Some(
                                tokio::time::Instant::now() + std::time::Duration::from_secs(1),
                            );
                            entry.notice = message.clone();
                        }
                        JobKind::HistoryRead => {
                            entry.history.cancel_read();
                            entry.notice = message.clone();
                        }
                        JobKind::ToolArtifacts | JobKind::CancelAgentOperation => {
                            entry.notice = message.clone();
                        }
                        JobKind::Compact => {
                            clear_busy(entry, "compacting");
                            entry.requested_material_version = None;
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
                        JobKind::TrustWorkspaceRules => {
                            clear_busy(entry, "trusting workspace rules");
                            entry.notice = message.clone();
                        }
                        JobKind::TrustWorkspaceAgents => {
                            clear_busy(entry, "trusting workspace agents");
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
