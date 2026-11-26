use super::{
    jobs::{Message, close_label, save_label},
    state::{App, Entry},
};
use crate::{input, transcript::Rows};
use causa::runtime::new_block_id;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tars_core::application::{ClosePolicy, ManagedSession, PreparedCreate, SaveStatus};
use tars_core::session::{SubmitRequest, WorkState};

pub(super) fn export_warning(importable: bool) -> &'static str {
    if importable {
        ""
    } else {
        " (exceeds the 64 MiB import limit; this version cannot import it directly)"
    }
}

pub(super) fn export(
    session: &ManagedSession,
    directory: &std::path::Path,
) -> Result<(String, bool), String> {
    let snapshot = session.read_materials().map_err(|e| e.to_string())?;
    let path = directory.join(format!("tars-recovery-{}.json", new_block_id().0));
    let context = snapshot.detached_context();
    let saved = tars_core::material::save_new(&path, &context).map_err(|e| e.to_string())?;
    Ok((path.display().to_string(), saved.importable))
}

impl App {
    pub(super) fn list(&mut self) {
        if self.listing || !self.capacity() {
            return;
        }
        self.listing = true;
        let host = self.host.clone();
        self.spawn_job(None, super::jobs::JobKind::List, async move {
            Message::Listed(host.list().await)
        });
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
        let model = definition.model.clone();
        let prompt_cache = definition.prompt_cache;
        let code_execution = definition.code_execution;
        let prepared = match PreparedCreate::prepare_persistent(definition, None) {
            Ok(prepared) => prepared,
            Err(error) => {
                form.error = error.to_string();
                return;
            }
        };
        let mut entry = Entry::new(prepared.conversation_id().as_str().to_owned(), model);
        entry.prompt_cache = prompt_cache;
        entry.code_execution = code_execution;
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
        entry.session_events_rx = None;
        entry.active_bg_count = 0;
        entry.viewed_agent_id = None;
        entry.viewed_agent_label = None;
        entry.viewed_agent_dirty = false;
        entry.approval_queue.clear();
        entry.approval_reason = None;
        entry.approval_scroll = 0;
        entry.workspace_rules_trust = None;
        entry.workspace_agents_trust = None;
        entry.material_version = None;
        entry.requested_material_version = None;
        entry.material_retry_after = None;
        entry.repair_job = None;
        entry.clear_history();
        entry.refreshing = false;
        entry.wrapping = false;
        entry.execution = "No work in this instance".into();
        entry.busy = Some("opening");
        let key = entry.key.clone();
        let prepared = entry.prepared.clone();
        let host = self.host.clone();
        self.spawn_job(Some(key.clone()), super::jobs::JobKind::Open, async move {
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
        if entry.session.is_none() {
            return;
        }
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to send input".into();
            return;
        }
        if entry.busy.is_some() {
            entry.notice = "Input retained; wait for the current session operation".into();
            return;
        }
        let text = entry.editor.lines().join("\n");
        if text.trim().is_empty() {
            return;
        }
        self.submit_text(text, true);
    }

    pub(super) fn submit_text(&mut self, text: String, push_history: bool) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        let Some(session) = entry.session.clone() else {
            return;
        };
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to send input".into();
            return;
        }
        if entry.busy.is_some() {
            entry.notice = "Input retained; wait for the current session operation".into();
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        let history_entry = text.clone();
        let request = SubmitRequest {
            request_key: new_block_id().0.to_string(),
            blocks: vec![tars_core::application::text_input(text)],
        };
        let progress = session.subscribe_progress();
        match session.submit(request) {
            Err(rejected) => {
                let hint = if matches!(
                    rejected.reason,
                    tars_core::application::ApplicationError::Repair(_)
                ) {
                    " · F7 / /repair"
                } else {
                    ""
                };
                entry.notice = format!("Input retained: {}{hint}", rejected.reason);
            }
            Ok(receipt) => {
                if push_history {
                    entry.editor = input::editor();
                    entry.push_input_history(&history_entry);
                }
                entry.work = Some(receipt.work.clone());
                entry.bind_progress(progress);
                entry.execution = "Running".into();
                entry.notice = "Input accepted; Ctrl-C cancels this work".into();
                entry.scroll = None;
                let key = entry.key.clone();
                self.spawn_job(Some(key.clone()), super::jobs::JobKind::Work, async move {
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

    pub(super) fn submit_onboarding(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(onboarding) = self.onboarding.as_mut() else {
            return;
        };
        let (input, workspace_root) = match onboarding.validate_and_build() {
            Ok(pair) => pair,
            Err(error) => {
                onboarding.error = error;
                return;
            }
        };
        let Some(target_path) = self.options.onboarding_config_path.clone() else {
            onboarding.error =
                "Unable to determine configuration path; pass --config or set TARS_HOME/HOME"
                    .into();
            return;
        };
        let new_config = match tars_core::config::write_initial_config(&target_path, &input) {
            Ok(config) => Arc::new(config),
            Err(error) => {
                onboarding.error = error.to_string();
                return;
            }
        };
        let preset = match (
            new_config
                .preset(self.options.profile.as_deref())
                .ok()
                .flatten(),
            self.options.code_execution_enabled,
        ) {
            (Some(_), Some(_)) => new_config
                .definition(
                    self.options.profile.as_deref(),
                    tars_core::config::DefinitionOverrides {
                        code_execution_enabled: self.options.code_execution_enabled,
                        ..Default::default()
                    },
                )
                .ok(),
            (preset, _) => preset,
        };
        let user_rules_path = new_config
            .config_dir()
            .map(|dir| dir.join("permissions.toml"));
        let mut host = tars_core::application::SessionHost::new(Some(self.options.store.clone()))
            .with_user_rules_path(user_rules_path.clone())
            .with_model_profiles(new_config.model_profiles());
        if let Some(external_tools) = &self.external_tools {
            host = host.with_external_tools(external_tools.clone());
        }
        self.options.config = new_config;
        self.options.config_path = Some(target_path.clone());
        self.options.preset = preset;
        self.user_rules_path = user_rules_path;
        self.host = host;

        let tools = workspace_root.as_ref().map(|_| {
            self.sandbox_status
                .as_ref()
                .map(|status| tars_core::config::default_tools_for_sandbox(status.is_available()))
                .unwrap_or_else(tars_core::config::default_tools_for_current_platform)
        });
        let definition = match self.options.config.definition(
            self.options.profile.as_deref(),
            tars_core::config::DefinitionOverrides {
                workspace_root,
                tools,
                code_execution_enabled: self.options.code_execution_enabled,
                ..Default::default()
            },
        ) {
            Ok(definition) => definition,
            Err(error) => {
                self.onboarding = None;
                self.onboarding_dismissed = true;
                self.notice = format!(
                    "Config saved to {}, but session creation failed: {error}",
                    target_path.display()
                );
                return;
            }
        };

        let model = definition.model.clone();
        let prompt_cache = definition.prompt_cache;
        let code_execution = definition.code_execution;
        let prepared = match PreparedCreate::prepare_persistent(definition, None) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.onboarding = None;
                self.onboarding_dismissed = true;
                self.notice = format!(
                    "Config saved to {}, but session creation failed: {error}",
                    target_path.display()
                );
                return;
            }
        };
        let mut entry = Entry::new(prepared.conversation_id().as_str().to_owned(), model);
        entry.prompt_cache = prompt_cache;
        entry.code_execution = code_execution;
        entry.prepared = Some(prepared);
        self.entries.push(entry);
        self.selected = self.entries.len() - 1;
        self.onboarding = None;
        self.onboarding_dismissed = true;
        self.notice = format!("Config: {} · F2 new / F3 open", target_path.display());
        self.open();
    }

    pub(super) fn run_doctor(&mut self) {
        if !self.capacity() {
            return;
        }
        self.panel = Some(super::commands::Panel::Info {
            title: super::commands::DOCTOR_PANEL_TITLE.into(),
            kind: super::commands::InfoKind::Plain,
            lines: vec!["Running diagnostics...".into()],
            scroll: 0,
        });
        let config = self.options.config.clone();
        let config_path = self.options.config_path.clone();
        let config_disabled = self.options.no_config;
        let profile = self.options.profile.clone();
        let store_dir = self.options.store.clone();
        let recovery_dir = self.options.recovery_dir.clone();
        let active_def = self
            .entries
            .get(self.selected)
            .and_then(|entry| entry.session.as_ref())
            .map(ManagedSession::definition);
        let workspace_root = active_def
            .as_ref()
            .and_then(|def| def.workspace_root.clone());
        let active_tools = active_def.map(|def| def.tools);
        let sandbox_status = self.sandbox_status.clone();
        let external_tools = self.external_tools.clone();
        let user_rules_path = self.user_rules_path.clone();

        self.spawn_job(None, super::jobs::JobKind::Doctor, async move {
            let result = super::jobs::blocking(move || {
                let input = tars_core::doctor::DoctorInput {
                    version: env!("CARGO_PKG_VERSION"),
                    config: &config,
                    config_path: config_path.as_deref(),
                    config_disabled,
                    profile: profile.as_deref(),
                    store_dir: &store_dir,
                    recovery_dir: &recovery_dir,
                    workspace_root: workspace_root.as_deref(),
                    active_tools: active_tools.as_deref(),
                    sandbox: sandbox_status.as_ref(),
                    external_tools: external_tools.as_ref(),
                    user_rules_path: user_rules_path.as_deref(),
                };
                tars_core::doctor::run_diagnostics(&input).format_lines()
            })
            .await;
            let lines = match result {
                Ok(lines) => lines,
                Err(error) => vec![format!("Failed to run diagnostics: {error}")],
            };
            Message::DoctorLoaded(lines)
        });
    }

    pub(super) fn run_init(&mut self) {
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session with a workspace first to run /init".into();
            return;
        };
        let Some(session) = entry.session.clone() else {
            entry.notice = "Open a session with a workspace first to run /init".into();
            return;
        };
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to run /init".into();
            return;
        }
        if entry.busy.is_some() || session.status().active_work.is_some() {
            entry.notice = "Wait for the current session operation before running /init".into();
            return;
        }
        let definition = session.definition();
        if definition.workspace_root.is_none() {
            entry.notice = "Bind a workspace to this session first to run /init".into();
            return;
        }
        let writable = definition.tools.iter().any(|tool| {
            matches!(
                tool,
                tars_core::definition::WorkspaceTool::WriteFile
                    | tars_core::definition::WorkspaceTool::EditFile
            )
        });
        self.submit_text(init_prompt(writable), false);
    }
    pub(super) fn trust_workspace_rules(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.busy.is_some() {
            entry.notice = "Wait for the current session operation".into();
            return;
        }
        let Some(session) = entry.session.clone() else {
            return;
        };
        entry.workspace_rules_trust = None;
        entry.busy = Some("trusting workspace rules");
        let key = entry.key.clone();
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::TrustWorkspaceRules,
            async move {
                let result = session.trust_workspace_rules().await;
                Message::WorkspaceRulesTrusted(key, result)
            },
        );
        self.update_workspace_trust_guard();
    }
    pub(super) fn trust_workspace_agents(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.busy.is_some() {
            entry.notice = "Wait for the current session operation".into();
            return;
        }
        let Some(session) = entry.session.clone() else {
            return;
        };
        entry.workspace_agents_trust = None;
        entry.busy = Some("trusting workspace agents");
        let key = entry.key.clone();
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::TrustWorkspaceAgents,
            async move {
                let result = session.trust_workspace_agents().await;
                Message::WorkspaceAgentsTrusted(key, result)
            },
        );
        self.update_workspace_trust_guard();
    }
    pub(super) fn cancel(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.selected)
            && let Some(session) = &entry.session
        {
            if let Some(active) = session.status().active_work {
                if let Some(work) = entry.work.as_ref().filter(|w| **w == active) {
                    entry.notice = match session.cancel(work) {
                        Ok(()) => "Cancellation requested; waiting for actual result".into(),
                        Err(e) => e.to_string(),
                    };
                } else {
                    session.cancel_all_active();
                    entry.notice = "Cancellation requested for active background tasks".into();
                }
            } else if session.active_finite_background_tasks() > 0 || session.has_active_execution()
            {
                session.cancel_all_active();
                entry.notice = "Cancellation requested for active background tasks".into();
            } else if let Some(work) = &entry.work {
                entry.notice = match session.cancel(work) {
                    Ok(()) => "Cancellation requested; waiting for actual result".into(),
                    Err(e) => e.to_string(),
                };
            }
        }
    }

    pub(super) fn cancel_or_report_idle(&mut self, agent_name: &str) {
        let has_active = self.entries.get(self.selected).is_some_and(|entry| {
            entry.busy == Some("submitting")
                || entry
                    .session
                    .as_ref()
                    .is_some_and(ManagedSession::has_active_execution)
        });
        if has_active {
            self.cancel();
        } else if let Some(entry) = self.entries.get_mut(self.selected) {
            entry.notice = format!(
                "Agent {} has no running operation",
                tars_core::permission::escape_untrusted_text(agent_name)
            );
        }
    }

    pub(super) fn complete_input(&mut self) {
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.viewed_agent_id.is_some() {
            return;
        }
        let workspace_root = entry
            .session
            .as_ref()
            .and_then(|session| session.definition().workspace_root);
        match input::complete_editor(&mut entry.editor, workspace_root.as_deref()) {
            input::CompleteResult::None => {}
            input::CompleteResult::Modified { notice } => {
                entry.reset_history_cursor();
                if let Some(notice) = notice {
                    entry.notice = notice;
                } else if entry.notice.starts_with("Commands: ")
                    || entry.notice.starts_with("Paths: ")
                    || entry.notice.starts_with("No matching ")
                    || entry.notice.starts_with("No workspace ")
                    || entry.notice.starts_with("Invalid workspace ")
                {
                    entry.notice.clear();
                }
            }
            input::CompleteResult::Notice(notice) => {
                entry.notice = notice;
            }
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
        entry.workspace_rules_trust = None;
        entry.workspace_agents_trust = None;
        let host = self.host.clone();
        let key = entry.key.clone();
        self.spawn_job(Some(key.clone()), super::jobs::JobKind::Close, async move {
            Message::Closed(key, host.close(&status.target, policy).await)
        });
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
            if session.status().lifecycle == tars_core::application::Lifecycle::Closing {
                self.close(self.selected, ClosePolicy::Save);
                return;
            }
            entry.busy = Some("retrying save");
            let key = entry.key.clone();
            self.spawn_job(Some(key.clone()), super::jobs::JobKind::Retry, async move {
                Message::Retried(key, session.retry_save().await)
            });
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
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Export,
            async move {
                let result = super::jobs::blocking(move || export(&session, &directory))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                Message::Exported(key, result)
            },
        );
    }
    pub(super) fn switch_viewed_agent(&mut self, target_agent_id: &str) {
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        let Some(session) = entry.session.clone() else {
            self.notice = "Open a session first to switch agents".into();
            return;
        };
        if target_agent_id == entry.id() {
            if entry.viewed_agent_id.is_none() {
                entry.notice = "Already viewing main (entry) agent".into();
                return;
            }
            entry.viewed_agent_id = None;
            entry.viewed_agent_label = None;
            entry.viewed_agent_dirty = false;
            entry.requested_material_version = None;
            entry.material_version = None;
            entry.material_retry_after = None;
            entry.refreshing = false;
            entry.transcript = Arc::new(crate::transcript::Transcript::default());
            entry.rows = None;
            entry.scroll = None;
            entry.scroll_anchor = None;
            entry.clear_history();
            entry.notice = "Switched to main (entry) agent".into();
            self.dirty = true;
            return;
        }
        let Some(snap) = session.agent_snapshot(target_agent_id) else {
            entry.notice = format!(
                "Agent '{}' is no longer available",
                tars_core::permission::escape_untrusted_text(target_agent_id)
            );
            return;
        };
        entry.viewed_agent_id = Some(snap.agent_id);
        entry.viewed_agent_label = Some(snap.name.clone());
        entry.viewed_agent_dirty = true;
        entry.refreshing = false;
        entry.transcript = Arc::new(crate::transcript::Transcript::default());
        entry.rows = None;
        entry.scroll = None;
        entry.scroll_anchor = None;
        entry.clear_history();
        entry.notice = format!(
            "Viewing child agent {} (read-only); run /agents main to return",
            tars_core::permission::escape_untrusted_text(&snap.name)
        );
        self.dirty = true;
    }

    pub(super) fn switch_agent_by_selector(&mut self, selector: &str) {
        let Some(entry) = self.entries.get(self.selected) else {
            self.notice = "Open a session first to switch agents".into();
            return;
        };
        let Some(session) = entry.session.clone() else {
            self.notice = "Open a session first to switch agents".into();
            return;
        };
        let selector = selector.trim();
        if selector == "main" || selector == entry.id() {
            let root_id = entry.id().to_owned();
            self.switch_viewed_agent(&root_id);
            return;
        }
        let agents = session.list_agents();
        if let Some(exact_id) = agents.iter().find(|a| a.agent_id == selector) {
            let id = exact_id.agent_id.clone();
            self.switch_viewed_agent(&id);
            return;
        }
        let by_name: Vec<_> = agents.iter().filter(|a| a.name == selector).collect();
        match by_name.as_slice() {
            [] => {
                self.notice = format!(
                    "No agent matching '{}'",
                    tars_core::permission::escape_untrusted_text(selector)
                );
            }
            [single] => {
                let id = single.agent_id.clone();
                self.switch_viewed_agent(&id);
            }
            _ => {
                self.notice = format!(
                    "Multiple agents named '{}'; use agent_id or /agents panel",
                    tars_core::permission::escape_untrusted_text(selector)
                );
            }
        }
    }

    pub(super) fn poll(&mut self) {
        self.poll_startup();
        self.poll_progress();
        for entry in &mut self.entries {
            if let Some(rx) = &mut entry.approvals_rx {
                let was_empty = entry.approval_queue.is_empty();
                while let Ok(request) = rx.try_recv() {
                    entry
                        .approval_queue
                        .push_back(super::state::PendingApproval::from_request(request));
                    self.dirty = true;
                }
                if was_empty && !entry.approval_queue.is_empty() {
                    entry.approval_reason = None;
                    entry.approval_scroll = 0;
                }
            }
            if drain_session_events(entry) {
                self.dirty = true;
            }
            if let Some(status) = self.host.lookup(&entry.id().into()) {
                let label = format!("{:?} | {}", status.lifecycle, save_label(&status.save));
                if entry.status != label {
                    entry.status = label;
                    self.dirty = true;
                }
                if let Some(agent_id) = entry.viewed_agent_id.clone() {
                    if let Some(session) = entry.session.clone()
                        && entry.viewed_agent_dirty
                        && !entry.refreshing
                        && entry.busy != Some("closing")
                        && self.jobs.len() < super::state::MAX_JOBS
                    {
                        entry.viewed_agent_dirty = false;
                        entry.refreshing = true;
                        let key = entry.key.clone();
                        let read_id = agent_id.clone();
                        super::jobs::spawn_job(
                            &mut self.jobs,
                            Some(key.clone()),
                            super::jobs::JobKind::AgentMaterials,
                            async move {
                                let result =
                                    super::jobs::blocking(move || {
                                        match session.read_agent_materials(&read_id) {
                                        Ok(snap) => Ok(Arc::new(
                                            crate::transcript::Transcript::from_materials(
                                                &snap.context,
                                                &snap.history,
                                            ),
                                        )),
                                        Err(
                                            tars_core::application::CoordinatorError::AgentBusy {
                                                ..
                                            },
                                        ) => Err(super::jobs::AgentMaterialsError::Busy),
                                        Err(error) => Err(super::jobs::AgentMaterialsError::Failed(
                                            error.to_string(),
                                        )),
                                    }
                                    })
                                    .await
                                    .unwrap_or_else(|e| {
                                        Err(super::jobs::AgentMaterialsError::Failed(e.to_string()))
                                    });
                                Message::AgentMaterials(key, agent_id, result)
                            },
                        );
                    }
                } else {
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
                        && entry
                            .material_retry_after
                            .is_none_or(|retry| tokio::time::Instant::now() >= retry)
                        && !entry.refreshing
                        && entry.busy != Some("closing")
                        && entry.busy != Some("compacting")
                        && self.jobs.len() < super::state::MAX_JOBS
                    {
                        entry.refreshing = true;
                        entry.requested_material_version = Some(version.clone());
                        #[cfg(test)]
                        {
                            entry.material_reads += 1;
                        }
                        let key = entry.key.clone();
                        super::jobs::spawn_job(
                            &mut self.jobs,
                            Some(key.clone()),
                            super::jobs::JobKind::Materials,
                            async move {
                                let result = super::jobs::blocking(move || {
                                    super::jobs::MaterialView::read(&session)
                                })
                                .await
                                .unwrap_or_else(|e| Err(e.to_string()));
                                Message::Materials(key, result)
                            },
                        );
                    }
                }
            }
        }
        self.poll_history();
        let mut wrap_job = None;
        if let Some(entry) = self.entries.get_mut(self.selected)
            && !entry.wrapping
            && self.jobs.len() < super::state::MAX_JOBS
        {
            let width = self.view_width;
            let history = entry.history.display();
            let current_stale = entry.rows.as_ref().is_none_or(|rows| {
                rows.width != width || !Arc::ptr_eq(&rows.transcript, &entry.transcript)
            });
            let history_stale = match (&entry.history_rows, &history) {
                (None, None) => false,
                (Some(rows), Some(display)) => {
                    rows.width != width || !Arc::ptr_eq(&rows.transcript, display)
                }
                _ => true,
            };
            if current_stale || history_stale {
                entry.wrapping = true;
                wrap_job = Some((entry.key.clone(), entry.transcript.clone(), history));
            }
        }
        if let Some((key, transcript, history)) = wrap_job {
            let width = self.view_width;
            self.spawn_job(
                Some(key.clone()),
                super::jobs::JobKind::Wrapping,
                async move {
                    let rows = super::jobs::blocking(move || Rows::wrap(transcript, width))
                        .await
                        .expect("pure text layout");
                    let history_rows = match history {
                        Some(history) => Some(
                            super::jobs::blocking(move || Rows::wrap(history, width))
                                .await
                                .expect("pure text layout"),
                        ),
                        None => None,
                    };
                    Message::Wrapped(key, rows, history_rows)
                },
            );
        }
        self.refresh_size_flags();
        self.update_approval_guard();
        self.update_workspace_trust_guard();
        self.update_repair_guard();
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

    /// Read the next history record when the screen is not full or the user
    /// scrolled to the top of the loaded conversation.
    fn poll_history(&mut self) {
        if self.jobs.len() >= super::state::MAX_JOBS {
            return;
        }
        let viewport_height = self.viewport_height;
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.session.is_none()
            || entry.viewed_agent_id.is_some()
            || entry.rows.is_none()
            || entry.wrapping
        {
            return;
        }
        if entry.history.loading() {
            return;
        }
        // Wait until the rows on screen match the cached history display, so a
        // fill-screen decision never runs ahead of a pending re-wrap.
        let display = entry.history.display();
        let history_ready = match (&display, &entry.history_rows) {
            (None, None) => true,
            (Some(display), Some(rows)) => {
                Arc::ptr_eq(&rows.transcript, display) && rows.width == self.view_width
            }
            _ => false,
        };
        if !history_ready {
            return;
        }
        let fill_screen = entry.scroll.is_none() && entry.conversation_len() < viewport_height;
        let anchor_wanted = entry.history.wants_earlier() && entry.history.has_loaded();
        let Some(read) = entry.history.next_read(fill_screen) else {
            return;
        };
        if anchor_wanted {
            entry.history_anchor = Some(entry.scroll);
        }
        let Some(session) = entry.session.clone() else {
            entry.history.cancel_read();
            return;
        };
        let key = entry.key.clone();
        #[cfg(test)]
        {
            entry.record_reads += 1;
        }
        let record = read.record;
        super::jobs::spawn_job(
            &mut self.jobs,
            Some(key.clone()),
            super::jobs::JobKind::HistoryRead,
            async move {
                let id = record.clone();
                let result = super::jobs::blocking(move || {
                    session
                        .read_history_record(&id)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::HistoryLoaded(key, record, result)
            },
        );
    }

    /// Replace the current materials with a model-written summary, as a job.
    pub(super) fn compact(&mut self) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session first to compact its context".into();
            return;
        };
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to compact".into();
            return;
        }
        if entry.busy.is_some() {
            entry.notice = "Wait for the current session operation".into();
            return;
        }
        let Some(session) = entry.session.clone() else {
            entry.notice = "Open a session first to compact its context".into();
            return;
        };
        if session.status().active_work.is_some() {
            entry.notice =
                "Cannot compact the context while a work is running; cancel it first".into();
            return;
        }
        entry.busy = Some("compacting");
        entry.notice = "Compacting the context; the replaced conversation stays on screen".into();
        let key = entry.key.clone();
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Compact,
            async move {
                let result = match session.compact().await {
                    Ok(outcome) => {
                        let reader = session.clone();
                        super::jobs::blocking(move || {
                            Ok(super::jobs::CompactedReload {
                                outcome,
                                view: super::jobs::MaterialView::read(&reader)?,
                            })
                        })
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                    }
                    Err(error) => {
                        let hint =
                            if matches!(error, tars_core::application::ApplicationError::Repair(_))
                            {
                                " · F7 / /repair"
                            } else {
                                ""
                            };
                        Err(format!("{error}{hint}"))
                    }
                };
                Message::Compacted(key, result)
            },
        );
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
                            match super::jobs::blocking(move || export(&session, &dir)).await {
                                Ok(Ok((path, importable))) => format!(
                                    "Recovery materials: {path}{}",
                                    export_warning(importable)
                                ),
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
    pub(super) fn apply_permission_mode(&mut self, mode: tars_core::permission::PermissionMode) {
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
            .map_or(tars_core::permission::PermissionMode::Ask, |(m, _)| m);
        let next_mode = match current_mode {
            tars_core::permission::PermissionMode::Ask => {
                tars_core::permission::PermissionMode::AcceptEdits
            }
            tars_core::permission::PermissionMode::AcceptEdits => {
                tars_core::permission::PermissionMode::ReadOnly
            }
            tars_core::permission::PermissionMode::ReadOnly => {
                tars_core::permission::PermissionMode::Ask
            }
            tars_core::permission::PermissionMode::FullAccess => {
                tars_core::permission::PermissionMode::Ask
            }
        };
        self.apply_permission_mode(next_mode);
    }
}

fn drain_session_events(entry: &mut Entry) -> bool {
    const MAX_SESSION_EVENTS_PER_TICK: usize = 64;
    let Some(mut rx) = entry.session_events_rx.take() else {
        return false;
    };
    let mut dirty = false;
    let mut closed = false;
    for _ in 0..MAX_SESSION_EVENTS_PER_TICK {
        match rx.try_recv() {
            Ok(ev) => {
                use tars_core::events::{OperationStatus, SessionEvent};
                match ev.as_ref() {
                    SessionEvent::AgentRegistered { .. }
                    | SessionEvent::BackgroundTaskChanged { .. }
                    | SessionEvent::InboxEventChanged { .. }
                    | SessionEvent::SessionLifecycleChanged { .. } => {
                        if let Some(session) = &entry.session {
                            entry.active_bg_count = session.active_finite_background_tasks();
                        }
                    }
                    SessionEvent::OperationStateChanged {
                        target_agent_id,
                        operation_id,
                        status,
                        ..
                    } => {
                        if let Some(session) = &entry.session {
                            entry.active_bg_count = session.active_finite_background_tasks();
                        }
                        if target_agent_id == entry.id()
                            && entry
                                .work
                                .as_ref()
                                .is_none_or(|w| w.turn_id.0 != *operation_id)
                        {
                            if *status == OperationStatus::Running {
                                entry.execution = "Running (background wakeup)".into();
                            } else if status.is_terminal() {
                                entry.execution = match status {
                                    OperationStatus::Completed => {
                                        "Completed (background wakeup)".into()
                                    }
                                    other => format!("Background wakeup: {other}"),
                                };
                                entry.requested_material_version = None;
                            }
                        }
                        if entry.viewed_agent_id.as_deref() == Some(target_agent_id.as_str()) {
                            entry.viewed_agent_dirty = true;
                        }
                    }
                    SessionEvent::AutoWakeupLimitReached {
                        agent_id,
                        max_auto_wakeups,
                        ..
                    } => {
                        if agent_id == entry.id() {
                            entry.notice = format!(
                                "Auto-wakeup limit ({max_auto_wakeups}) reached; remaining notifications queued for next turn"
                            );
                        } else {
                            entry.notice = format!(
                                "Agent {} reached auto-wakeup limit ({max_auto_wakeups}); remaining notifications queued",
                                tars_core::permission::escape_untrusted_text(agent_id)
                            );
                        }
                    }
                    SessionEvent::AgentProgress { .. } => {}
                }
                dirty = true;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                if let Some(session) = &entry.session {
                    entry.active_bg_count = session.active_finite_background_tasks();
                    if entry.execution == "Running (background wakeup)"
                        && session.status().active_work.is_none()
                        && session
                            .agent_snapshot(entry.id())
                            .is_none_or(|snap| snap.active_operation_id.is_none())
                    {
                        entry.execution = "Completed (background wakeup)".into();
                        entry.requested_material_version = None;
                    }
                    if let Some(agent_id) = entry.viewed_agent_id.as_deref() {
                        entry.viewed_agent_label =
                            session.agent_snapshot(agent_id).map(|snap| snap.name);
                        entry.viewed_agent_dirty = true;
                    }
                }
                dirty = true;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                closed = true;
                break;
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
        }
    }
    if !closed {
        entry.session_events_rx = Some(rx);
    }
    dirty
}

fn init_prompt(writable: bool) -> String {
    let base = "\
Inspect this workspace and prepare a concise, accurate `AGENTS.md` for future coding agents working in this repository.

First explore the workspace using your available read-only tools:
1. Build, test, lint, and formatting commands (check manifest/config files such as `Cargo.toml`, `package.json`, `Makefile`, CI workflows, etc.).
2. Architecture and directory/module layout (crates, packages, key entry points, and layer boundaries).
3. Important coding conventions, safety/error-handling rules, and test patterns.
4. Existing `AGENTS.md` (if present at the workspace root).";

    if writable {
        format!(
            "{base}\n\n\
Then create `AGENTS.md` at the workspace root (or update it if it already exists, preserving any existing user rules and verified constraints). Keep it factual, concise, and grounded only in files you directly observed."
        )
    } else {
        format!(
            "{base}\n\n\
Note: File write/edit tools are disabled in this session. Do not attempt to modify files; instead, output a concise, ready-to-save `AGENTS.md` draft directly in your response, grounded only in files you directly observed."
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn export_warning_names_the_import_boundary() {
        assert_eq!(super::export_warning(true), "");
        let warning = super::export_warning(false);
        assert!(warning.contains("64 MiB"));
        assert!(warning.contains("cannot import"));
    }
}
