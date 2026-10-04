use super::state::{App, Entry, Key};
use crate::transcript::{Rows, Transcript};
use std::sync::Arc;
use tarz_core::application::{
    ApplicationError, CloseOutcome, ManagedSession, MaterialVersion, SaveStatus, SessionListing,
};
use tarz_core::session::{WorkRef, WorkResult};

pub(super) enum Message {
    Listed(Result<Vec<SessionListing>, ApplicationError>),
    Opened(Key, Result<ManagedSession, ApplicationError>),
    Finished(Key, WorkRef, Result<Arc<WorkResult>, String>),
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
        WorkResult::Faulted(_) => "Worker fault; materials may be unavailable",
        _ => "Failed; check model, endpoint and credential configuration",
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
                    entry.clear_progress();
                    entry.scroll = None;
                    entry.execution = result
                        .as_deref()
                        .map(result_label)
                        .unwrap_or("Observation failed")
                        .into();
                    if let Err(error) = result {
                        entry.notice = error;
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
                    match &result {
                        Ok(outcome) => {
                            entry.session = None;
                            entry.prepared = None;
                            entry.work = None;
                            entry.clear_progress();
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
