use super::state::{App, Entry, Key};
use crate::transcript::{Rows, Transcript};
use std::sync::Arc;
use tarz_core::application::{
    ApplicationError, CloseOutcome, ManagedSession, MaterialVersion, SaveStatus, SessionListing,
};
use tarz_core::session::{ModelFailure, WorkRef, WorkResult};

pub(super) enum Message {
    RepairInspected(
        Key,
        causa::kernel::BlockId,
        Result<(tarz_core::application::RepairInspection, Option<String>), String>,
    ),
    RepairPlanned(
        Key,
        causa::kernel::BlockId,
        Result<tarz_core::application::RepairPlan, String>,
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
                Arc<tarz_core::session::ToolProgressSnapshot>,
            ),
            String,
        >,
    ),
    Materials(Key, Result<(MaterialVersion, Arc<Transcript>), String>),
    Wrapped(Key, Rows),
    Retried(Key, Result<SaveStatus, ApplicationError>),
    Exported(Key, Result<String, String>),
    Closed(Key, Result<CloseOutcome, ApplicationError>),
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
    use tarz_core::application::CommitDisposition;
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
                            entry.session = Some(session);
                            entry.prepared = None;
                            entry.notice =
                                "Opened; only materials are restored, not old work records".into();
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
                            entry.transcript = transcript;
                            entry.rows = None;
                        }
                        Err(error) => entry.notice = format!("Material view unavailable: {error}"),
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
                        Ok(path) => format!("Exported actual materials: {path}"),
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
        }
    }
}
