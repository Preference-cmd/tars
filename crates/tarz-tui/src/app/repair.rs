use super::{
    App, Panel,
    jobs::{Message, save_label},
    state::{CONFIRM_GUARD_WINDOW, GuardWindow, Key},
};
use causa::{kernel::BlockId, runtime::new_block_id};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tarz_core::{
    application::{RepairInspection, RepairMode, RepairPlan, RepairWriteWarning, SaveStatus},
    session::WorkRef,
};

/// One bounded hint line for a possible pre-interruption side effect.
fn warning_hint(warning: &RepairWriteWarning) -> String {
    format!("  {}", warning.advice())
}

/// Aggregate lines per warning kind; `suffix` closes each line ("." or ":").
fn warning_total_lines(total: usize, external: usize, suffix: &str) -> Vec<String> {
    let writes = total - external;
    let mut lines = Vec::new();
    if writes > 0 {
        lines.push(format!(
            "{writes} write call(s) may have changed files before interruption; read them again before continuing{suffix}"
        ));
    }
    if external > 0 {
        lines.push(format!(
            "{external} external call(s) may have caused external side effects; check the server state before continuing{suffix}"
        ));
    }
    lines
}

pub(crate) struct RepairPanel {
    pub(super) key: Key,
    pub(super) request: BlockId,
    work: Option<WorkRef>,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub plan: Option<RepairPlan>,
    pub busy: bool,
    pub(crate) guard: GuardWindow,
}

#[cfg(test)]
impl RepairPanel {
    pub(crate) fn for_test(lines: Vec<String>) -> Self {
        Self {
            key: Key {
                id: "repair-test".into(),
                generation: 0,
            },
            request: new_block_id(),
            work: None,
            lines,
            scroll: 0,
            plan: None,
            busy: false,
            guard: GuardWindow::new(),
        }
    }
}

impl App {
    pub(super) fn show_repair(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.repair_job.is_some() {
            self.notice =
                "This session already has a material inspection or repair in progress".into();
            return;
        }
        let Some(session) = entry.session.clone() else {
            self.notice = "Open this session before inspecting its materials".into();
            return;
        };
        let key = entry.key.clone();
        let work = entry.work.clone();
        let request = new_block_id();
        entry.repair_job = Some(request);
        self.panel = Some(Panel::Repair(Box::new(RepairPanel {
            key: key.clone(),
            request,
            work: work.clone(),
            lines: vec![
                "Waiting for the current work to settle and save. This does not cancel it.".into(),
            ],
            scroll: 0,
            plan: None,
            busy: true,
            guard: GuardWindow::new(),
        })));
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Repair(request),
            async move {
                let save_error = session
                    .wait_saved()
                    .await
                    .err()
                    .map(|error| error.to_string());
                let result = super::jobs::blocking(move || {
                    session
                        .inspect_repair(work.as_ref())
                        .map(|inspection| (inspection, save_error))
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::RepairInspected(key, request, result)
            },
        );
    }

    fn repair_panel(&mut self, key: &Key, request: BlockId) -> Option<&mut RepairPanel> {
        match self.panel.as_mut() {
            Some(Panel::Repair(panel)) if panel.key == *key && panel.request == request => {
                Some(panel)
            }
            _ => None,
        }
    }

    fn repair_job_finished(&mut self, key: &Key, request: BlockId) {
        if let Some(entry) = self.entry_mut(key)
            && entry.repair_job == Some(request)
        {
            entry.repair_job = None;
        }
    }

    pub(super) fn repair_inspected(
        &mut self,
        key: Key,
        request: BlockId,
        result: Result<(RepairInspection, Option<String>), String>,
    ) {
        self.repair_job_finished(&key, request);
        let Some(panel) = self.repair_panel(&key, request) else {
            return;
        };
        panel.guard.disarm();
        panel.busy = false;
        panel.lines = match result {
            Err(error) => vec![
                format!("Inspection unavailable: {error}"),
                "No materials were changed. Esc returns to your draft.".into(),
            ],
            Ok((inspection, save_error)) => {
                let diagnosis = &inspection.diagnosis;
                let mut lines = vec![format!(
                    "{} blocks · {} unpaired declarations · {} issues",
                    diagnosis.block_count, diagnosis.unpaired_count, diagnosis.issue_count
                )];
                if diagnosis.is_ready() {
                    lines.push("No structural repair is needed. Esc returns to the draft; sending starts a new work.".into());
                }
                if let Some(error) = save_error {
                    lines.push(format!(
                        "Save/lifecycle needs attention before applying: {error}"
                    ));
                }
                lines.extend(diagnosis.issues.iter().map(|issue| {
                    format!(
                        "{:?} {:?}: {}",
                        issue.kind,
                        issue.block_id.map(|id| id.0),
                        issue.message
                    )
                }));
                if diagnosis.issue_count > diagnosis.issues.len() {
                    lines.push("Diagnostic display is limited to the first 128 items.".into());
                }
                match inspection.original {
                    None => lines.push(
                        "No original work batch in this view. Reopening restores materials only."
                            .into(),
                    ),
                    Some(original) => {
                        lines.push(format!(
                            "Original work {}: batch available {} · {} results · {} pending",
                            original.work.turn_id.0,
                            original.available,
                            original.completed_count,
                            original.pending_count
                        ));
                        if let Some(error) = original.adoption_error {
                            lines.push(format!("Original-result adoption unavailable: {error}"));
                        }
                        for result in &original.results {
                            lines.push(format!(
                                "Original result {} → {}: {:?}",
                                result.result_id.0, result.call_block_id.0, result.status
                            ));
                            lines.push(format!(
                                "Result excerpt (up to 1024 bytes): {}",
                                result.excerpt
                            ));
                            if let Some(warning) = &result.write_warning {
                                lines.push(warning_hint(warning));
                            }
                        }
                        lines.extend(warning_total_lines(
                            original.write_warnings_count,
                            original.external_warnings_count,
                            ".",
                        ));
                    }
                }
                lines.extend([
                    "Removal only excludes unpaired declarations from future requests; it does not establish non-execution.".into(),
                    "Removing declarations does not adopt original uncommitted results. Context exports exclude that original batch.".into(),
                    "Original work states and UnknownOutcome results stay unchanged. Viewing and planning execute nothing.".into(),
                ]);
                lines
            }
        };
    }

    fn prepare_repair(&mut self, mode: RepairMode) {
        if !self.capacity() {
            return;
        }
        let Some(Panel::Repair(panel)) = self.panel.as_mut() else {
            return;
        };
        if panel.busy {
            return;
        }
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == panel.key) else {
            return;
        };
        if entry.repair_job.is_some() {
            return;
        }
        let Some(session) = entry.session.clone() else {
            return;
        };
        panel.plan = None;
        panel.guard.disarm();
        panel.busy = true;
        panel.request = new_block_id();
        entry.repair_job = Some(panel.request);
        panel.scroll = 0;
        panel.lines =
            vec!["Preparing a local candidate; original results remain unclaimed.".into()];
        let key = panel.key.clone();
        let request = panel.request;
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Repair(request),
            async move {
                let result = super::jobs::blocking(move || {
                    session.plan_repair(mode).map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::RepairPlanned(key, request, result)
            },
        );
    }

    pub(super) fn repair_planned(
        &mut self,
        key: Key,
        request: BlockId,
        result: Result<RepairPlan, String>,
    ) {
        self.repair_job_finished(&key, request);
        let now = self.clock.now();
        let is_ok = result.is_ok();
        let Some(panel) = self.repair_panel(&key, request) else {
            return;
        };
        panel.busy = false;
        match result {
            Err(error) => {
                panel.guard.disarm();
                panel.lines = vec![
                    format!("Cannot prepare this repair: {error}"),
                    "No materials were changed. F7 refreshes the actual state.".into(),
                ]
            }
            Ok(plan) => {
                panel.guard.arm(now, CONFIRM_GUARD_WINDOW);
                let summary = plan.summary();
                let mode = match plan.mode() {
                    RepairMode::RemoveUnpaired => "Remove unpaired declarations",
                    RepairMode::AdoptOriginal(_) => "Adopt original results",
                };
                panel.lines = vec![
                    format!("Preview: {mode}"),
                    format!(
                        "{} → {} blocks · remove {} declarations · adopt {} original results",
                        summary.original_blocks,
                        summary.candidate_blocks,
                        summary.removed_count,
                        summary.adopted_count
                    ),
                ];
                panel.lines.extend(
                    summary
                        .removed
                        .iter()
                        .map(|id| format!("Remove declaration {}", id.0)),
                );
                panel.lines.extend(
                    summary
                        .adopted
                        .iter()
                        .map(|id| format!("Adopt original result {}", id.0)),
                );
                for line in warning_total_lines(
                    summary.write_warnings_count,
                    summary.external_warnings_count,
                    ":",
                ) {
                    panel.lines.push(line);
                }
                for warning in &summary.write_warnings {
                    panel.lines.push(warning_hint(warning));
                }
                if summary.write_warnings_count > summary.write_warnings.len() {
                    panel
                        .lines
                        .push("Warning display limited to 128 items.".into());
                }
                if summary.removed_count > summary.removed.len()
                    || summary.adopted_count > summary.adopted.len()
                {
                    panel.lines.push("Each identity list displays at most 128 items; counts above include all items.".into());
                }
                panel.lines.extend([format!("Add application explanation {}", summary.explanation_id.0),
                    "Retained blocks keep their identities, contents and order. Original result statuses are preserved.".into(),
                    "Removed declarations do not prove an operation never ran; their unadopted results stay only in the retained original work.".into(),
                    "Enter applies exactly this versioned preview and waits for saving. It does not send your draft or resume old work.".into()]);
                panel.plan = Some(plan);
            }
        }
        if is_ok {
            self.dirty = true;
        }
    }

    fn apply_repair(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(Panel::Repair(panel)) = self.panel.as_mut() else {
            return;
        };
        if panel.busy {
            return;
        }
        let Some(entry) = self.entries.iter_mut().find(|entry| entry.key == panel.key) else {
            return;
        };
        let Some(session) = entry.session.clone() else {
            return;
        };
        let Some(plan) = panel.plan.take() else {
            return;
        };
        panel.guard.disarm();
        panel.busy = true;
        panel.lines =
            vec!["Applying the preview and waiting for saving. Your draft is retained.".into()];
        panel.scroll = 0;
        entry.busy = Some("repairing materials");
        entry.repair_job = Some(panel.request);
        let key = panel.key.clone();
        let request = panel.request;
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Repair(request),
            async move {
                let applying = session.clone();
                let applied = super::jobs::blocking(move || {
                    applying
                        .apply_repair(&plan)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                let did_apply = applied.is_ok();
                let result = match applied {
                    Ok(_) => session
                        .wait_saved()
                        .await
                        .map_err(|error| error.to_string()),
                    Err(error) => Err(error),
                };
                Message::RepairApplied(key, request, did_apply, result)
            },
        );
    }

    pub(super) fn repair_applied(
        &mut self,
        key: Key,
        request: BlockId,
        applied: bool,
        result: Result<SaveStatus, String>,
    ) {
        self.repair_job_finished(&key, request);
        let message = match result {
            Ok(save) => format!(
                "Repair applied; {}. Original work unchanged. Esc returns to your draft; send it explicitly to start new work.",
                save_label(&save)
            ),
            Err(error) if applied => format!(
                "Materials repaired, saving needs attention: {error}. Ctrl-R retries the same save; Ctrl-E exports current Context."
            ),
            Err(error) => {
                format!("Repair rejected; materials unchanged: {error}. F7 prepares a fresh view.")
            }
        };
        if let Some(entry) = self.entry_mut(&key) {
            if entry.busy == Some("repairing materials") {
                entry.busy = None;
            }
            entry.notice = message.clone();
        }
        if let Some(panel) = self.repair_panel(&key, request) {
            panel.guard.disarm();
            panel.busy = false;
            panel.lines = vec![message];
        }
    }

    pub(super) fn repair_key(&mut self, key: KeyEvent) {
        if self.is_repair_guarded() {
            if key.code == KeyCode::Esc {
                self.panel = None;
                return;
            }
            if let Some(Panel::Repair(panel)) = &mut self.panel {
                scroll_repair(panel, key.code, self.view_width);
            }
            return;
        }

        if key.code == KeyCode::Esc {
            self.panel = None;
            return;
        }
        if key.code == KeyCode::F(4) && key.kind == KeyEventKind::Press {
            self.panel = None;
            if !self.entries.is_empty() {
                self.selected = (self.selected + 1) % self.entries.len();
            }
            return;
        }
        if key.kind == KeyEventKind::Press {
            if key.code == KeyCode::F(7) {
                self.show_repair();
                return;
            }
            if crate::input::control(key, 'r') {
                self.panel = None;
                self.retry();
                return;
            }
            if crate::input::control(key, 'e') {
                self.panel = None;
                self.export();
                return;
            }
            if key.code == KeyCode::Enter {
                self.apply_repair();
                return;
            }
            if key.code == KeyCode::Char('r') {
                self.prepare_repair(RepairMode::RemoveUnpaired);
                return;
            }
            if key.code == KeyCode::Char('a') {
                if let Some(Panel::Repair(panel)) = &self.panel
                    && let Some(work) = &panel.work
                {
                    self.prepare_repair(RepairMode::AdoptOriginal(work.clone()));
                }
                return;
            }
        }
        if let Some(Panel::Repair(panel)) = &mut self.panel {
            scroll_repair(panel, key.code, self.view_width);
        }
    }
}

/// Scrolls the preview lines of the repair panel; other keys do nothing.
fn scroll_repair(panel: &mut RepairPanel, code: KeyCode, view_width: u16) {
    let max = crate::transcript::detail_rows(&panel.lines, view_width)
        .count()
        .saturating_sub(1);
    match code {
        KeyCode::Down => panel.scroll = panel.scroll.saturating_add(1).min(max),
        KeyCode::PageDown => panel.scroll = panel.scroll.saturating_add(5).min(max),
        KeyCode::Up => panel.scroll = panel.scroll.saturating_sub(1),
        KeyCode::PageUp => panel.scroll = panel.scroll.saturating_sub(5),
        _ => {}
    }
}
