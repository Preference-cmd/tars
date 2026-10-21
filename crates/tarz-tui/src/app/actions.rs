use super::{
    jobs::{Message, close_label, save_label},
    state::{App, Entry},
};
use crate::{
    input,
    transcript::{Rows, Transcript},
};
use causa::kernel::Context;
use causa::runtime::new_block_id;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tarz_core::application::{ClosePolicy, ManagedSession, PreparedCreate, SaveStatus};
use tarz_core::session::{SubmitRequest, WorkState};

pub(super) fn export(
    session: &ManagedSession,
    directory: &std::path::Path,
) -> Result<String, String> {
    let snapshot = session.read_materials().map_err(|e| e.to_string())?;
    let path = directory.join(format!("tarz-recovery-{}.json", new_block_id().0));
    tarz_core::material::save_new(&path, &snapshot.context).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

impl App {
    pub(super) fn list(&mut self) {
        if self.listing || !self.capacity() {
            return;
        }
        self.listing = true;
        let host = self.host.clone();
        self.jobs
            .spawn(async move { Message::Listed(host.list().await) });
    }
    pub(super) fn create(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        let definition = match form.definition() {
            Ok(definition) => definition,
            Err(error) => {
                form.error = error;
                return;
            }
        };
        let title = definition.model.clone();
        let prepared = PreparedCreate::persistent(definition, Context::new());
        let mut entry = Entry::new(prepared.conversation_id().as_str().to_owned(), title);
        entry.prepared = Some(prepared);
        self.entries.push(entry);
        self.selected = self.entries.len() - 1;
        self.form = None;
        self.open();
    }
    pub(super) fn open(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.session.is_some() || entry.busy.is_some() {
            return;
        }
        // Reopen changes the UI instance key. Old completions can never update it.
        entry.clear_progress();
        entry.tools = super::tool_progress::ToolView::default();
        entry.key.generation += 1;
        entry.approvals_rx = None;
        entry.approval_queue.clear();
        entry.approval_reason = None;
        entry.approval_scroll = 0;
        entry.material_version = None;
        entry.requested_material_version = None;
        entry.repair_job = None;
        entry.refreshing = false;
        entry.wrapping = false;
        entry.execution = "No work in this instance".into();
        entry.busy = Some("opening");
        let key = entry.key.clone();
        let prepared = entry.prepared.clone();
        let host = self.host.clone();
        self.jobs.spawn(async move {
            let result = match prepared {
                Some(prepared) => host.create(prepared).await,
                None => host.open(key.id.clone().into()).await,
            };
            Message::Opened(key, result)
        });
    }
    pub(super) fn submit(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        let Some(session) = entry.session.clone() else {
            return;
        };
        if entry.busy.is_some() {
            entry.notice = "Input retained; wait for the current session operation".into();
            return;
        }
        let text = entry.editor.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }
        let request = SubmitRequest {
            request_key: new_block_id().0.to_string(),
            blocks: vec![tarz_core::application::text_input(text)],
        };
        let progress = session.subscribe_progress();
        match session.submit(request) {
            Err(rejected) => {
                let hint = if matches!(
                    rejected.reason,
                    tarz_core::application::ApplicationError::Repair(_)
                ) {
                    " · F7 / /repair"
                } else {
                    ""
                };
                entry.notice = format!("Input retained: {}{hint}", rejected.reason);
            }
            Ok(receipt) => {
                entry.editor = input::editor();
                entry.work = Some(receipt.work.clone());
                entry.bind_progress(progress);
                entry.execution = "Running".into();
                entry.notice = "Input accepted; Ctrl-C cancels this work".into();
                entry.scroll = None;
                let key = entry.key.clone();
                self.jobs.spawn(async move {
                    let result = loop {
                        match session.wait(&receipt.work, Duration::from_secs(30)).await {
                            Ok(observation) => {
                                if let WorkState::Finished(result) = observation.state {
                                    break Ok((result, observation.tools));
                                }
                            }
                            Err(error) => break Err(error.to_string()),
                        }
                    };
                    Message::Finished(key, receipt.work, result)
                });
            }
        }
    }
    pub(super) fn cancel(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.selected)
            && let (Some(session), Some(work)) = (&entry.session, &entry.work)
        {
            entry.notice = match session.cancel(work) {
                Ok(()) => "Cancellation requested; waiting for actual result".into(),
                Err(e) => e.to_string(),
            };
        }
    }
    pub(super) fn close(&mut self, index: usize, policy: ClosePolicy) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.busy.is_some() {
            return;
        }
        let Some(status) = self.host.lookup(&entry.id().into()) else {
            return;
        };
        entry.busy = Some("closing");
        entry.approval_queue.clear();
        entry.approval_reason = None;
        entry.approval_scroll = 0;
        let host = self.host.clone();
        let key = entry.key.clone();
        self.jobs
            .spawn(async move { Message::Closed(key, host.close(&status.target, policy).await) });
    }
    pub(super) fn retry(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.busy.is_some() {
            return;
        }
        if let Some(session) = entry.session.clone() {
            if !matches!(session.status().save, SaveStatus::SaveBlocked { .. }) {
                return;
            }
            // A failed Save close must be rejoined via close, not wait_saved on a
            // lifecycle that will disappear after releasing the store lock.
            if session.status().lifecycle == tarz_core::application::Lifecycle::Closing {
                self.close(self.selected, ClosePolicy::Save);
                return;
            }
            entry.busy = Some("retrying save");
            let key = entry.key.clone();
            self.jobs
                .spawn(async move { Message::Retried(key, session.retry_save().await) });
        } else if entry.prepared.is_some() {
            self.open();
        }
    }
    pub(super) fn export(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.busy.is_some() {
            return;
        }
        let Some(session) = entry.session.clone() else {
            entry.notice = "No opened material view; initial creation contains no input".into();
            return;
        };
        entry.busy = Some("exporting");
        let key = entry.key.clone();
        let directory = self.options.recovery_dir.clone();
        self.jobs.spawn(async move {
            let result = tokio::task::spawn_blocking(move || export(&session, &directory))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            Message::Exported(key, result)
        });
    }
    pub(super) fn poll(&mut self) {
        self.poll_progress();
        for entry in &mut self.entries {
            if let Some(rx) = &mut entry.approvals_rx {
                while let Ok(request) = rx.try_recv() {
                    entry.approval_queue.push_back(request);
                    self.dirty = true;
                }
            }
            if let Some(status) = self.host.lookup(&entry.id().into()) {
                let label = format!("{:?} | {}", status.lifecycle, save_label(&status.save));
                if entry.status != label {
                    entry.status = label;
                    self.dirty = true;
                }
                let version = match &status.save {
                    SaveStatus::Saved {
                        material_version, ..
                    }
                    | SaveStatus::Saving {
                        material_version, ..
                    }
                    | SaveStatus::SaveBlocked {
                        material_version, ..
                    } => Some(material_version),
                    _ => None,
                };
                if let (Some(session), Some(version)) = (entry.session.clone(), version)
                    && entry.requested_material_version.as_ref() != Some(version)
                    && !entry.refreshing
                    && entry.busy != Some("closing")
                    && self.jobs.len() < super::state::MAX_JOBS
                {
                    entry.refreshing = true;
                    entry.requested_material_version = Some(version.clone());
                    #[cfg(test)]
                    {
                        entry.material_reads += 1;
                    }
                    let key = entry.key.clone();
                    self.jobs.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let snapshot = session.read_materials().map_err(|e| e.to_string())?;
                            Ok((
                                snapshot.version,
                                Arc::new(Transcript::from_context(&snapshot.context)),
                            ))
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()));
                        Message::Materials(key, result)
                    });
                }
            }
        }
        if let Some(entry) = self.entries.get_mut(self.selected)
            && entry.rows.as_ref().is_none_or(|r| {
                r.width != self.view_width || !Arc::ptr_eq(&r.transcript, &entry.transcript)
            })
            && !entry.wrapping
            && self.jobs.len() < super::state::MAX_JOBS
        {
            entry.wrapping = true;
            let key = entry.key.clone();
            let transcript = entry.transcript.clone();
            let width = self.view_width;
            self.jobs.spawn(async move {
                let rows = tokio::task::spawn_blocking(move || Rows::wrap(transcript, width))
                    .await
                    .expect("pure text layout");
                Message::Wrapped(key, rows)
            });
        }
        if self.quitting {
            for index in 0..self.entries.len() {
                self.close(index, ClosePolicy::Save);
            }
            self.done = self
                .entries
                .iter()
                .all(|e| self.host.lookup(&e.id().into()).is_none())
                && self.jobs.is_empty();
        }
    }

    pub async fn emergency_close(&mut self) -> Vec<String> {
        self.jobs.abort_all();
        while self.jobs.join_next().await.is_some() {}
        let mut reports = Vec::new();
        for entry in &self.entries {
            let Some(status) = self.host.lookup(&entry.id().into()) else {
                continue;
            };
            match self.host.close(&status.target, ClosePolicy::Save).await {
                Ok(outcome) => reports.push(close_label(&outcome)),
                Err(error) => {
                    reports.push(format!("{}: save failed: {error}", entry.id()));
                    if let Some(session) = entry.session.clone() {
                        let dir: PathBuf = self.options.recovery_dir.clone();
                        reports.push(
                            match tokio::task::spawn_blocking(move || export(&session, &dir)).await
                            {
                                Ok(Ok(path)) => format!("Recovery materials: {path}"),
                                other => format!("Recovery export failed: {other:?}"),
                            },
                        );
                    }
                    match self
                        .host
                        .close(&status.target, ClosePolicy::DiscardLocal)
                        .await
                    {
                        Ok(outcome) => reports.push(close_label(&outcome)),
                        Err(error) => reports.push(format!("Resource cleanup failed: {error}")),
                    }
                }
            }
        }
        reports
    }
    pub(super) fn apply_permission_mode(&mut self, mode: tarz_core::permission::PermissionMode) {
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session first to change its permission mode".into();
            return;
        };
        let Some(session) = &entry.session else {
            self.notice = "Open a session first to change its permission mode".into();
            return;
        };
        match session.set_permission_mode(mode) {
            Ok((new_mode, _)) => {
                self.notice = format!("Permission mode: {new_mode}");
                self.dirty = true;
            }
            Err(error) => {
                self.notice = format!("Failed to set permission mode: {error}");
                self.dirty = true;
            }
        }
    }

    pub(super) fn cycle_permission_mode(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            self.notice = "Open a session first to change its permission mode".into();
            return;
        };
        let Some(session) = &entry.session else {
            self.notice = "Open a session first to change its permission mode".into();
            return;
        };
        let current_mode = session
            .permission_mode()
            .map_or(tarz_core::permission::PermissionMode::Ask, |(m, _)| m);
        let next_mode = match current_mode {
            tarz_core::permission::PermissionMode::Ask => {
                tarz_core::permission::PermissionMode::AcceptEdits
            }
            tarz_core::permission::PermissionMode::AcceptEdits => {
                tarz_core::permission::PermissionMode::ReadOnly
            }
            tarz_core::permission::PermissionMode::ReadOnly => {
                tarz_core::permission::PermissionMode::Ask
            }
            tarz_core::permission::PermissionMode::FullAccess => {
                tarz_core::permission::PermissionMode::Ask
            }
        };
        self.apply_permission_mode(next_mode);
    }
}
