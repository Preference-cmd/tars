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
    pub(super) fn default_workspace_tools(&self) -> Vec<tars_core::definition::WorkspaceTool> {
        self.sandbox_status
            .as_ref()
            .map(tars_core::sandbox::SandboxStatus::is_available)
            .or_else(|| self.sandbox.as_ref().map(|check| check.available))
            .map(tars_core::config::default_tools_for_sandbox)
            .unwrap_or_else(tars_core::config::default_tools_for_current_platform)
    }

    pub(super) fn active_or_cli_workspace(&self) -> Option<PathBuf> {
        self.entries
            .get(self.selected)
            .and_then(|entry| {
                entry
                    .session
                    .as_ref()
                    .and_then(|s| s.definition().workspace_root)
                    .or_else(|| entry.workspace_root.clone())
            })
            .or_else(|| self.options.workspace.clone())
    }

    pub(super) fn prepare_and_open_definition(
        &mut self,
        definition: tars_core::definition::SessionDefinition,
    ) -> Result<(String, String), String> {
        let model = definition.model.clone();
        let prompt_cache = definition.prompt_cache;
        let code_execution = definition.code_execution;
        let workspace_root = definition.workspace_root.clone();
        let prepared = PreparedCreate::prepare_persistent(definition, None)
            .map_err(|error| error.to_string())?;
        let id = prepared.conversation_id().as_str().to_owned();
        let mut entry = Entry::new(id.clone(), model.clone());
        entry.prompt_cache = prompt_cache;
        entry.code_execution = code_execution;
        entry.workspace_root = workspace_root;
        entry.prepared = Some(prepared);
        self.entries.push(entry);
        self.scratch_editor = input::editor();
        self.selected = self.entries.len() - 1;
        self.refresh_size_flags();
        self.open();
        self.update_approval_guard();
        self.update_workspace_trust_guard();
        Ok((id, model))
    }

    fn resolve_preset_definition(
        &self,
        profile: Option<&str>,
        preset: &tars_core::definition::SessionDefinition,
        workspace_root: Option<PathBuf>,
        tools: Option<Vec<tars_core::definition::WorkspaceTool>>,
    ) -> Result<tars_core::definition::SessionDefinition, String> {
        if self.options.config.preset(profile).ok().flatten().is_some() {
            self.options
                .config
                .definition(
                    profile,
                    tars_core::config::DefinitionOverrides {
                        workspace_root,
                        tools,
                        code_execution_enabled: self.options.code_execution_enabled,
                        ..Default::default()
                    },
                )
                .map_err(|error| error.to_string())
        } else {
            let mut definition = preset.clone();
            definition.workspace_root = workspace_root;
            if let Some(tools) = tools {
                definition.tools = tools;
            }
            Ok(definition)
        }
    }

    pub(super) fn create_default_startup_session(&mut self) {
        if self.quitting
            || self.confirmation.is_some()
            || self.onboarding.is_some()
            || self.form.is_some()
            || self.panel.is_some()
            || !self.capacity()
        {
            return;
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.session.is_some() || entry.busy.is_some())
        {
            return;
        }
        let Some(preset) = self.options.preset.clone() else {
            return;
        };
        let workspace_root = self
            .options
            .workspace
            .clone()
            .or_else(|| preset.workspace_root.clone())
            .or_else(|| std::env::current_dir().ok());
        let tools = if preset.tools.is_empty() && workspace_root.is_some() {
            Some(self.default_workspace_tools())
        } else {
            None
        };
        let definition = match self.resolve_preset_definition(
            self.options.profile.as_deref(),
            &preset,
            workspace_root,
            tools,
        ) {
            Ok(definition) => definition,
            Err(error) => {
                self.notice = format!(
                    "Could not start default session: {} · Run /doctor or /new",
                    crate::transcript::display_single_line(&error)
                );
                return;
            }
        };
        if let Err(error) = self.prepare_and_open_definition(definition) {
            self.notice = format!(
                "Could not start default session: {} · Run /doctor or /new",
                crate::transcript::display_single_line(&error)
            );
        }
    }

    pub(super) fn clear_command(&mut self) {
        if self.quitting
            || self.confirmation.is_some()
            || self.onboarding.is_some()
            || self.form.is_some()
            || !self.capacity()
        {
            return;
        }
        let Some(preset) = self.options.preset.clone() else {
            self.notice = "No default model preset configured; use /new".into();
            return;
        };
        let workspace_root = self
            .active_or_cli_workspace()
            .or_else(|| preset.workspace_root.clone())
            .or_else(|| std::env::current_dir().ok());
        let tools = if preset.tools.is_empty() && workspace_root.is_some() {
            Some(self.default_workspace_tools())
        } else {
            None
        };
        let definition = match self.resolve_preset_definition(
            self.options.profile.as_deref(),
            &preset,
            workspace_root,
            tools,
        ) {
            Ok(definition) => definition,
            Err(error) => {
                self.notice = format!(
                    "Could not start fresh session: {} · Run /doctor or /new",
                    crate::transcript::display_single_line(&error)
                );
                return;
            }
        };
        match self.prepare_and_open_definition(definition) {
            Ok((id, _)) => {
                let short_id = id.get(..8).unwrap_or(&id);
                self.notice = format!("Started fresh session {short_id}");
            }
            Err(error) => {
                self.notice = format!(
                    "Could not start fresh session: {} · Run /doctor or /new",
                    crate::transcript::display_single_line(&error)
                );
            }
        }
    }

    pub(super) fn resume_command(&mut self, arg: &str) {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            self.pending_resume_selector = None;
            self.list();
            let selected = self
                .matching_sessions("")
                .iter()
                .position(|&index| index == self.selected)
                .unwrap_or(0);
            self.panel = Some(super::commands::Panel::Sessions {
                query: String::new(),
                selected,
            });
            return;
        }
        if !self.listing && !self.capacity() {
            return;
        }
        self.pending_resume_selector = Some(trimmed.to_owned());
        self.list();
    }

    pub(super) fn switch_model_profile(&mut self, profile_name: &str) {
        if self.quitting
            || self.confirmation.is_some()
            || self.onboarding.is_some()
            || self.form.is_some()
            || !self.capacity()
        {
            return;
        }
        let trimmed = profile_name.trim();
        if trimmed.is_empty() || !self.options.config.model_profiles().contains_key(trimmed) {
            self.notice = format!(
                "Profile '{}' not found (or no profiles in config.toml); use /new",
                crate::transcript::display_single_line(trimmed)
            );
            return;
        }
        let active_or_cli_ws = self.active_or_cli_workspace();
        let base_def = match self.options.config.definition(
            Some(trimmed),
            tars_core::config::DefinitionOverrides {
                workspace_root: active_or_cli_ws.clone(),
                code_execution_enabled: self.options.code_execution_enabled,
                ..Default::default()
            },
        ) {
            Ok(definition) => definition,
            Err(error) => {
                self.notice = format!(
                    "Could not switch to profile {}: {} · Run /doctor or /new",
                    crate::transcript::display_single_line(trimmed),
                    crate::transcript::display_single_line(&error.to_string())
                );
                return;
            }
        };
        let workspace_root = active_or_cli_ws
            .or_else(|| base_def.workspace_root.clone())
            .or_else(|| std::env::current_dir().ok());
        let needs_tools = base_def.tools.is_empty() && workspace_root.is_some();
        let definition = if base_def.workspace_root != workspace_root || needs_tools {
            let tools = needs_tools.then(|| self.default_workspace_tools());
            match self.options.config.definition(
                Some(trimmed),
                tars_core::config::DefinitionOverrides {
                    workspace_root,
                    tools,
                    code_execution_enabled: self.options.code_execution_enabled,
                    ..Default::default()
                },
            ) {
                Ok(definition) => definition,
                Err(error) => {
                    self.notice = format!(
                        "Could not switch to profile {}: {} · Run /doctor or /new",
                        crate::transcript::display_single_line(trimmed),
                        crate::transcript::display_single_line(&error.to_string())
                    );
                    return;
                }
            }
        } else {
            base_def
        };
        match self.prepare_and_open_definition(definition.clone()) {
            Ok((id, model)) => {
                let profile_preset = match (
                    self.options.config.preset(Some(trimmed)),
                    self.options.code_execution_enabled,
                ) {
                    (Ok(Some(_)), Some(_)) => self
                        .options
                        .config
                        .definition(
                            Some(trimmed),
                            tars_core::config::DefinitionOverrides {
                                code_execution_enabled: self.options.code_execution_enabled,
                                ..Default::default()
                            },
                        )
                        .ok(),
                    (Ok(preset), _) => preset,
                    (Err(_), _) => None,
                };
                self.options.profile = Some(trimmed.to_owned());
                self.options.preset = profile_preset.or(Some(definition));
                let short_id = id.get(..8).unwrap_or(&id);
                self.notice = format!(
                    "Switched to profile {} ({}) in new session {short_id}",
                    crate::transcript::display_single_line(trimmed),
                    crate::transcript::display_single_line(&model),
                );
            }
            Err(error) => {
                self.notice = format!(
                    "Could not switch to profile {}: {} · Run /doctor or /new",
                    crate::transcript::display_single_line(trimmed),
                    crate::transcript::display_single_line(&error)
                );
            }
        }
    }

    pub(super) fn list(&mut self) {
        if self.listing || !self.capacity() {
            return;
        }
        self.listing = true;
        let generation = self.list_generation;
        let host = self.host.clone();
        self.spawn_job(None, super::jobs::JobKind::List, async move {
            Message::Listed(generation, host.list().await)
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
        if let Err(error) = self.prepare_and_open_definition(definition) {
            if let Some(form) = self.form.as_mut() {
                form.error = error;
            }
            return;
        }
        self.form = None;
    }
    pub(super) fn open(&mut self) {
        if !self.capacity() {
            return;
        }
        let now = self.clock.now();
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
        entry.superseded_material_version = None;
        entry.material_retry_after = None;
        entry.repair_job = None;
        entry.clear_history();
        entry.refreshing = false;
        entry.wrapping = false;
        entry.turn_in_flight = false;
        entry.turn_started_at = None;
        entry.scroll = None;
        entry.has_unread_output = false;
        entry.queued_prompts.clear();
        entry.queue_paused = false;
        entry.execution = "No work in this instance".into();
        entry.set_busy("opening", now);
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
        let text = self.active_editor().lines().join("\n");
        if !text.contains('\n') {
            let trimmed = text.trim();
            if self.entries.get(self.selected).is_some()
                && (trimmed == "/image"
                    || trimmed.starts_with("/image ")
                    || trimmed.starts_with("/image\t"))
            {
                let entry = &mut self.entries[self.selected];
                if entry.viewed_agent_id.is_some() {
                    entry.notice =
                        "Viewing child agent (read-only); run /agents main to attach images".into();
                    return;
                }
                if entry.busy.is_some() {
                    entry.notice = "Input retained; wait for the current session operation".into();
                    return;
                }
                let arg = trimmed.strip_prefix("/image").unwrap_or("").trim();
                if self.image_command(arg)
                    && let Some(entry) = self.entries.get_mut(self.selected)
                {
                    entry.editor = input::editor();
                    entry.reset_history_cursor();
                }
                return;
            }
            if let Some(rest) = trimmed.strip_prefix('/') {
                let clean = rest.trim();
                if let Some(cmd_idx) = Self::exact_command_index(clean) {
                    *self.active_editor_mut() = input::editor();
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        entry.reset_history_cursor();
                    }
                    self.command(cmd_idx, clean);
                    return;
                }
            }
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            if !text.trim().is_empty() {
                self.notice =
                    "No active session — type /new (or F2) to create one, or /sessions to resume"
                        .into();
            }
            return;
        };
        if entry.session.is_none() {
            if !text.trim().is_empty() {
                entry.notice =
                    "Session is not open — press /open (or F3) first, or /new to create one".into();
            }
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
        let has_text = !text.trim().is_empty();
        let has_images = !entry.pending_images.is_empty();
        if !has_text && !has_images {
            return;
        }
        if entry.is_turn_running() {
            if entry.queued_prompts.len() >= super::state::MAX_QUEUED_PROMPTS {
                entry.notice = format!(
                    "Queue full (max {}); /queue to inspect or clear",
                    super::state::MAX_QUEUED_PROMPTS
                );
                return;
            }
            let images = std::mem::take(&mut entry.pending_images);
            let image_count = images.len();
            entry
                .queued_prompts
                .push_back(super::state::QueuedPrompt::new(text.clone(), images));
            entry.editor = input::editor();
            if has_text {
                entry.push_input_history(&text);
            } else {
                entry.reset_history_cursor();
            }
            let count = entry.queued_prompts.len();
            let image_suffix = if image_count > 0 && !has_text {
                format!(" with {image_count} image(s)")
            } else if image_count > 0 {
                format!(" (+{image_count} image(s))")
            } else {
                String::new()
            };
            entry.notice = if entry.queue_paused {
                format!("Queued message{image_suffix} ({count} waiting, paused); /queue to manage")
            } else {
                format!("Queued message{image_suffix} ({count} waiting); /queue to manage")
            };
            self.refresh_viewport_height();
            return;
        }
        let prompt =
            super::state::QueuedPrompt::new(text, std::mem::take(&mut entry.pending_images));
        match self.submit_prompt_at(self.selected, prompt, true) {
            Ok(()) => {
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    entry.queue_paused = false;
                }
                self.refresh_viewport_height();
            }
            Err((_, returned)) => {
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    entry.pending_images = returned.images;
                }
                self.refresh_viewport_height();
            }
        }
    }

    pub(super) fn submit_text(&mut self, text: String, push_history: bool) {
        let _ = self.submit_text_at(self.selected, text, push_history);
    }

    pub(super) fn submit_text_at(
        &mut self,
        index: usize,
        text: String,
        push_history: bool,
    ) -> Result<(), tars_core::application::ApplicationError> {
        match self.submit_prompt_at(index, super::state::QueuedPrompt::from(text), push_history) {
            Ok(()) => Ok(()),
            Err((Some(reason), _)) => Err(reason),
            Err((None, _)) => Ok(()),
        }
    }

    pub(super) fn submit_prompt_at(
        &mut self,
        index: usize,
        prompt: super::state::QueuedPrompt,
        push_history: bool,
    ) -> Result<
        (),
        (
            Option<tars_core::application::ApplicationError>,
            super::state::QueuedPrompt,
        ),
    > {
        if !self.capacity() {
            return Err((None, prompt));
        }
        let Some(entry) = self.entries.get_mut(index) else {
            return Err((None, prompt));
        };
        let Some(session) = entry.session.clone() else {
            return Err((None, prompt));
        };
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to send input".into();
            return Err((None, prompt));
        }
        if entry.busy.is_some() {
            entry.notice = "Input retained; wait for the current session operation".into();
            return Err((None, prompt));
        }
        let has_text = !prompt.text.trim().is_empty();
        if !has_text && prompt.images.is_empty() {
            return Err((None, prompt));
        }
        let history_entry = prompt.text.clone();
        let progress = session.subscribe_progress();
        let submit_result = if prompt.images.is_empty() {
            let request = SubmitRequest {
                request_key: new_block_id().0.to_string(),
                blocks: vec![tars_core::application::text_input(prompt.text.clone())],
            };
            session.submit(request).map_err(|rejected| rejected.reason)
        } else {
            let text_opt = has_text.then_some(prompt.text.as_str());
            session.submit_with_images(new_block_id().0.to_string(), text_opt, &prompt.images)
        };
        match submit_result {
            Err(reason) => {
                if push_history {
                    let hint =
                        if matches!(reason, tars_core::application::ApplicationError::Repair(_)) {
                            " · F7 / /repair"
                        } else {
                            ""
                        };
                    entry.notice = format!("Input retained: {reason}{hint}");
                }
                Err((Some(reason), prompt))
            }
            Ok(receipt) => {
                if push_history {
                    entry.editor = input::editor();
                    entry.pending_images.clear();
                    if has_text {
                        entry.push_input_history(&history_entry);
                    } else {
                        entry.reset_history_cursor();
                    }
                }
                let media_refs: Vec<causa::kernel::MediaRef> = prompt
                    .images
                    .iter()
                    .map(tars_core::media::PreparedImage::media_ref)
                    .collect();
                let echo_text = has_text.then_some(prompt.text.as_str());
                if entry.refreshing {
                    entry.superseded_material_version = entry.requested_material_version.clone();
                }
                entry.transcript = Arc::new(
                    entry
                        .transcript
                        .with_appended_user_echo(echo_text, &media_refs),
                );
                if entry.transcript.text.len() <= 64 * 1024 && self.view_width > 0 {
                    entry.rows = Some(Rows::wrap(entry.transcript.clone(), self.view_width));
                } else {
                    entry.rows = None;
                }
                entry.turn_in_flight = true;
                entry.turn_started_at = Some(self.clock.now());
                entry.work = Some(receipt.work.clone());
                entry.bind_progress(progress);
                entry.execution = "Running".into();
                entry.notice = "Input accepted; Ctrl-C cancels this work".into();
                entry.scroll = None;
                entry.has_unread_output = false;
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
                Ok(())
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
        let workspace_root = definition.workspace_root.clone();
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
        entry.workspace_root = workspace_root;
        entry.prepared = Some(prepared);
        self.entries.push(entry);
        self.scratch_editor = input::editor();
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
        let now = self.clock.now();
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
        entry.set_busy("trusting workspace rules", now);
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
        let now = self.clock.now();
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
        entry.set_busy("trusting workspace agents", now);
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
        let profile_names: Vec<String> = self.options.config.model_profiles().into_keys().collect();
        if let Some(entry) = self.entries.get_mut(self.selected) {
            if entry.viewed_agent_id.is_some() {
                return;
            }
            let workspace_root = entry
                .session
                .as_ref()
                .and_then(|session| session.definition().workspace_root);
            let result = input::complete_editor(
                &mut entry.editor,
                workspace_root.as_deref(),
                &profile_names,
            );
            if matches!(result, input::CompleteResult::Modified { .. }) {
                entry.reset_history_cursor();
            }
            apply_completion_result(&mut entry.notice, result);
            return;
        }
        let result = input::complete_editor(
            &mut self.scratch_editor,
            self.options.workspace.as_deref(),
            &profile_names,
        );
        apply_completion_result(&mut self.notice, result);
    }
    pub(super) fn close(&mut self, index: usize, policy: ClosePolicy) {
        if !self.capacity() {
            return;
        }
        let now = self.clock.now();
        let Some(entry) = self.entries.get_mut(index) else {
            return;
        };
        if entry.busy.is_some() {
            return;
        }
        let Some(status) = self.host.lookup(&entry.id().into()) else {
            return;
        };
        entry.set_busy("closing", now);
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
        let now = self.clock.now();
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
            entry.set_busy("retrying save", now);
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
        let now = self.clock.now();
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
        entry.set_busy("exporting", now);
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

    pub(super) fn export_markdown(&mut self, raw_path: &str) {
        if !self.capacity() {
            return;
        }
        let clock_now = self.clock.now();
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session first to export Markdown".into();
            return;
        };
        if entry.busy.is_some() {
            entry.notice =
                "Wait for the current session operation before exporting Markdown".into();
            return;
        }
        let Some(session) = entry.session.clone() else {
            entry.notice = "Open a session first to export Markdown".into();
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let viewing_child = entry.viewed_agent_id.is_some();
        let target_agent_id = entry
            .viewed_agent_id
            .clone()
            .unwrap_or_else(|| entry.id().to_owned());
        let agent_label = if viewing_child {
            Some(
                entry
                    .viewed_agent_label
                    .clone()
                    .or_else(|| session.agent_snapshot(&target_agent_id).map(|s| s.name))
                    .unwrap_or_else(|| target_agent_id.clone()),
            )
        } else {
            None
        };
        let trimmed = raw_path.trim();
        let output_path = if trimmed.is_empty() {
            let short_id: String = entry.id().chars().take(8).collect();
            let filename = match &agent_label {
                Some(label) => {
                    let slug = sanitize_export_filename_segment(label);
                    format!("tars-export-{short_id}-{slug}-{now_ms}.md")
                }
                None => format!("tars-export-{short_id}-{now_ms}.md"),
            };
            self.options.recovery_dir.join(filename)
        } else {
            let Some(parsed) = crate::input::image::parse_single_image_path(trimmed) else {
                entry.notice = "Usage: /export-md [path]".into();
                return;
            };
            if parsed.contains('\0') {
                entry.notice = "Invalid Markdown export path".into();
                return;
            }
            if parsed == "~" || parsed.starts_with("~/") || parsed.contains("://") {
                let Some(resolved) = crate::input::image::resolve_local_path(&parsed, None) else {
                    entry.notice = format!(
                        "Invalid Markdown export path: {}",
                        tars_core::permission::escape_untrusted_text(&parsed)
                    );
                    return;
                };
                resolved
            } else {
                PathBuf::from(parsed)
            }
        };
        let conversation_id: tars_core::session::ConversationId = entry.id().into();
        let title = entry.title();
        let fallback_model = entry.model().to_owned();
        let host = self.host.clone();
        let key = entry.key.clone();
        entry.set_busy("exporting markdown", clock_now);
        entry.notice = "Exporting Markdown...".into();
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::ExportMarkdown,
            async move {
                let result = match session
                    .expanded_agent_conversation(&target_agent_id, None)
                    .await
                {
                    Ok(expanded) => {
                        let unreadable_count = expanded.unreadable_records.len();
                        let model = if viewing_child {
                            host.inspect_agent(&conversation_id, &target_agent_id)
                                .await
                                .ok()
                                .map(|insp| insp.definition.model)
                                .unwrap_or(fallback_model)
                        } else {
                            fallback_model
                        };
                        let options = tars_core::export::MarkdownExportOptions {
                            title,
                            model: Some(model),
                            agent_label,
                            exported_at_ms: Some(now_ms),
                            include_prelude: false,
                        };
                        let markdown = tars_core::export::render_conversation_markdown(
                            &conversation_id,
                            &expanded,
                            &options,
                        );
                        let write_path = output_path.clone();
                        match super::jobs::blocking(move || {
                            tars_core::export::save_markdown_new(&write_path, &markdown)
                        })
                        .await
                        {
                            Ok(Ok(())) => Ok((output_path, unreadable_count)),
                            Ok(Err(error)) => Err(error.to_string()),
                            Err(join_err) => Err(join_err.to_string()),
                        }
                    }
                    Err(
                        tars_core::application::ApplicationError::AgentBusy { .. }
                        | tars_core::application::ApplicationError::Session(
                            tars_core::session::SessionError::Busy { .. },
                        ),
                    ) => Err(
                        "Wait for the current turn to finish before exporting Markdown".to_owned(),
                    ),
                    Err(error) => Err(error.to_string()),
                };
                Message::MarkdownExported { key, result }
            },
        );
    }

    pub(super) fn copy_command(&mut self, arg: &str) {
        let tokens: Vec<&str> = arg.split_whitespace().collect();
        let target = match tokens.as_slice() {
            [] => CopyTarget::LastReply,
            [sub] if sub.eq_ignore_ascii_case("id") => CopyTarget::SessionId,
            [sub] if sub.eq_ignore_ascii_case("code") => CopyTarget::CodeBlock(None),
            [sub, raw_idx] if sub.eq_ignore_ascii_case("code") => match raw_idx.parse::<usize>() {
                Ok(0) => {
                    self.set_copy_notice(
                        "Code block #0 is invalid (1-based index required)".into(),
                    );
                    return;
                }
                Ok(n) => CopyTarget::CodeBlock(Some(n)),
                Err(_) => {
                    self.set_copy_notice("Usage: /copy [code [N] | id]".into());
                    return;
                }
            },
            _ => {
                self.set_copy_notice("Usage: /copy [code [N] | id]".into());
                return;
            }
        };

        let Some(entry) = self.entries.get(self.selected) else {
            self.set_copy_notice(match target {
                CopyTarget::SessionId => "No active session to copy ID from".into(),
                _ => "No active session".into(),
            });
            return;
        };

        if matches!(target, CopyTarget::SessionId) {
            let full_id = entry.id().to_owned();
            let short = full_id.get(..8).unwrap_or(&full_id).to_owned();
            match self.write_clipboard(full_id.as_bytes()) {
                Ok(()) => {
                    self.set_copy_notice(format!(
                        "Copied session ID {short} to clipboard via OSC 52"
                    ));
                }
                Err(error) => {
                    self.set_copy_notice(format!(
                        "Failed to copy session ID: {}",
                        crate::transcript::display_single_line(&error.to_string())
                    ));
                }
            }
            return;
        }

        let mut replies = if let Some(session) = entry.session.as_ref() {
            let context = match entry.viewed_agent_id.as_deref() {
                Some(agent_id) => session
                    .read_agent_materials(agent_id)
                    .ok()
                    .map(|snap| snap.context),
                None => session.read_materials().ok().map(|snap| snap.context),
            };
            context
                .map(|ctx| collect_assistant_replies_from_blocks(ctx.blocks()))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if replies.is_empty() {
            replies = collect_assistant_replies_from_transcript(&entry.transcript);
        }

        match target {
            CopyTarget::SessionId => unreachable!("handled above"),
            CopyTarget::LastReply => {
                let Some(reply) = replies.last() else {
                    self.set_copy_notice("No assistant reply to copy".into());
                    return;
                };
                let chars = reply.chars().count();
                match self.write_clipboard(reply.as_bytes()) {
                    Ok(()) => {
                        self.set_copy_notice(format!(
                            "Copied last reply ({chars} chars) to clipboard via OSC 52"
                        ));
                    }
                    Err(error) => {
                        self.set_copy_notice(format!(
                            "Failed to copy last reply: {}",
                            crate::transcript::display_single_line(&error.to_string())
                        ));
                    }
                }
            }
            CopyTarget::CodeBlock(index_opt) => {
                let Some(blocks) = replies
                    .iter()
                    .rev()
                    .map(|reply| extract_fenced_code_blocks(reply))
                    .find(|blocks| !blocks.is_empty())
                else {
                    self.set_copy_notice("No fenced code block found in assistant replies".into());
                    return;
                };
                let available = blocks.len();
                let (one_based_idx, (lang, code)) = match index_opt {
                    None => (available, &blocks[available - 1]),
                    Some(n) if n <= available => (n, &blocks[n - 1]),
                    Some(n) => {
                        self.set_copy_notice(format!(
                            "Code block #{n} not found ({available} available)"
                        ));
                        return;
                    }
                };
                let lines = if code.is_empty() {
                    0
                } else {
                    code.split('\n').count()
                };
                let chars = code.chars().count();
                match self.write_clipboard(code.as_bytes()) {
                    Ok(()) => {
                        let lang_prefix = lang
                            .as_deref()
                            .map(|l| format!("{l}, "))
                            .unwrap_or_default();
                        self.set_copy_notice(format!(
                            "Copied code block #{one_based_idx} ({lang_prefix}{lines} lines, {chars} chars) to clipboard via OSC 52"
                        ));
                    }
                    Err(error) => {
                        self.set_copy_notice(format!(
                            "Failed to copy code block: {}",
                            crate::transcript::display_single_line(&error.to_string())
                        ));
                    }
                }
            }
        }
    }

    fn set_copy_notice(&mut self, message: String) {
        if let Some(entry) = self.entries.get_mut(self.selected) {
            entry.notice = message;
        } else {
            self.notice = message;
        }
        self.dirty = true;
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
            entry.has_unread_output = false;
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
        entry.has_unread_output = false;
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
        let now = self.clock.now();
        for entry in &mut self.entries {
            entry.sync_busy_started_at(now);
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
            if drain_session_events(entry, now) {
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
        self.try_dequeue_ready();
        let animating = self.startup.is_none()
            && self
                .entries
                .get(self.selected)
                .is_some_and(Entry::is_animating);
        if animating {
            self.tick_phase = (self.tick_phase + 1) % 2;
            if self.tick_phase == 0 {
                self.spinner_tick = self.spinner_tick.wrapping_add(1);
                self.dirty = true;
            }
        } else {
            self.tick_phase = 0;
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
        let now = self.clock.now();
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
        entry.set_busy("compacting", now);
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

    pub(super) fn try_dequeue_ready(&mut self) {
        if self.quitting
            || self.confirmation.is_some()
            || self.panel.is_some()
            || self.form.is_some()
            || self.onboarding.is_some()
            || self.jobs.len() >= super::state::MAX_JOBS
        {
            return;
        }
        for index in 0..self.entries.len() {
            let (session, eligible) = {
                let entry = &self.entries[index];
                let ok = !entry.queued_prompts.is_empty()
                    && !entry.queue_paused
                    && !entry.turn_in_flight
                    && entry.busy.is_none()
                    && entry.viewed_agent_id.is_none()
                    && entry.approval_queue.is_empty()
                    && entry.workspace_rules_trust.is_none()
                    && entry.workspace_agents_trust.is_none()
                    && entry.repair_job.is_none();
                (entry.session.clone(), ok)
            };
            if !eligible {
                continue;
            }
            let Some(session) = session else {
                continue;
            };
            let status = session.status();
            if matches!(status.save, SaveStatus::SaveBlocked { .. }) {
                let entry = &mut self.entries[index];
                entry.queue_paused = true;
                let count = entry.queued_prompts.len();
                entry.notice = format!(
                    "Save blocked; {count} queued message(s) paused — /queue pop to edit or /queue clear"
                );
                self.dirty = true;
                continue;
            }
            if status.lifecycle != tars_core::application::Lifecycle::Open {
                if matches!(
                    status.lifecycle,
                    tars_core::application::Lifecycle::Closing
                        | tars_core::application::Lifecycle::Closed
                        | tars_core::application::Lifecycle::Faulted
                        | tars_core::application::Lifecycle::Failed
                ) {
                    self.entries[index].queue_paused = true;
                    self.dirty = true;
                }
                continue;
            }
            if status.active_work.is_some()
                || matches!(
                    status.save,
                    SaveStatus::Saving { .. } | SaveStatus::Dirty { .. }
                )
            {
                continue;
            }
            if session
                .inspect_repair(None)
                .is_ok_and(|insp| insp.diagnosis.unpaired_count > 0)
            {
                let entry = &mut self.entries[index];
                entry.queue_paused = true;
                let count = entry.queued_prompts.len();
                entry.notice = format!(
                    "Materials need repair; {count} queued message(s) paused — /repair or /queue pop"
                );
                self.dirty = true;
                continue;
            }
            let Some(next) = self.entries[index].queued_prompts.pop_front() else {
                continue;
            };
            match self.submit_prompt_at(index, next, false) {
                Ok(()) => {
                    self.dirty = true;
                }
                Err((None, returned)) => {
                    self.entries[index].queued_prompts.push_front(returned);
                }
                Err((Some(error), returned)) => {
                    self.entries[index].queued_prompts.push_front(returned);
                    if !matches!(
                        error,
                        tars_core::application::ApplicationError::Saving
                            | tars_core::application::ApplicationError::Session(
                                tars_core::session::SessionError::Busy { .. }
                            )
                    ) {
                        let entry = &mut self.entries[index];
                        entry.queue_paused = true;
                        let count = entry.queued_prompts.len();
                        let hint =
                            if matches!(error, tars_core::application::ApplicationError::Repair(_))
                            {
                                " · F7 / /repair"
                            } else {
                                ""
                            };
                        entry.notice = format!(
                            "Auto-submit paused ({error}{hint}); {count} queued message(s) waiting — /queue pop or /queue clear"
                        );
                    }
                    self.dirty = true;
                }
            }
        }
    }

    pub(super) fn rename_command(&mut self, arg: &str) {
        if !self.capacity() {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session first to rename it".into();
            return;
        };
        if entry.session.is_none() {
            entry.notice = "Open a session first to rename it".into();
            return;
        }
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to rename".into();
            return;
        }
        if entry.busy.is_some() {
            entry.notice = "Wait for the current session operation before renaming".into();
            return;
        }
        if arg.is_empty() {
            let current = entry
                .title()
                .map(|t| {
                    format!(
                        " (current: \"{}\")",
                        tars_core::permission::escape_untrusted_text(&t)
                    )
                })
                .unwrap_or_default();
            entry.notice = format!("Usage: /rename <title> | /rename --clear{current}");
            return;
        }
        let raw_title = if arg == "--clear" { None } else { Some(arg) };
        let validated = match tars_core::validate_session_title(raw_title) {
            Ok(title) => title,
            Err(err) => {
                entry.notice = format!("Invalid title: {err}");
                return;
            }
        };
        let key = entry.key.clone();
        let id: tars_core::session::ConversationId = entry.id().into();
        let host = self.host.clone();
        self.spawn_job(
            Some(key.clone()),
            super::jobs::JobKind::Rename,
            async move {
                let result = host.rename(id, validated).await;
                Message::Renamed(key, result)
            },
        );
    }

    pub(super) fn fork_command(&mut self, arg: &str) {
        if !self.capacity() {
            return;
        }
        let now = self.clock.now();
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a saved idle session first to fork it".into();
            return;
        };
        let Some(session) = entry.session.clone() else {
            entry.notice = "Open a saved idle session first to fork it".into();
            return;
        };
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to fork".into();
            return;
        }
        if entry.busy.is_some() || entry.is_turn_running() {
            entry.notice = "Wait for the current turn or operation to finish before forking".into();
            return;
        }
        let status = session.status();
        match &status.save {
            SaveStatus::NotTracked => {
                entry.notice = "Temporary sessions cannot be forked".into();
                return;
            }
            SaveStatus::SaveBlocked { .. } => {
                entry.notice =
                    "Cannot fork while session save is blocked; retry or export first".into();
                return;
            }
            SaveStatus::Dirty { .. } | SaveStatus::Saving { .. } => {
                entry.notice = "Session is still saving; try /fork again in a moment".into();
                return;
            }
            SaveStatus::Saved { .. } => {}
        }
        let raw_title = (!arg.is_empty()).then_some(arg);
        let validated = match tars_core::validate_session_title(raw_title) {
            Ok(title) => title,
            Err(err) => {
                entry.notice = format!("Invalid fork title: {err}");
                return;
            }
        };
        entry.set_busy("forking", now);
        entry.notice = "Forking session...".into();
        let key = entry.key.clone();
        let source_id: tars_core::session::ConversationId = entry.id().into();
        let host = self.host.clone();
        self.spawn_job(Some(key.clone()), super::jobs::JobKind::Fork, async move {
            let result = host.fork(source_id, validated).await;
            let listing = if let Ok(ref new_id) = result {
                host.list().await.ok().and_then(|list| {
                    list.into_iter()
                        .find(|item| item.conversation_id == *new_id)
                })
            } else {
                None
            };
            Message::Forked(key, result, listing.map(Box::new))
        });
    }

    pub(super) fn delete_session(&mut self, id: String) {
        if !self.capacity() {
            return;
        }
        if self
            .entries
            .iter()
            .any(|e| e.id() == id && (e.session.is_some() || e.busy.is_some()))
            || self.host.lookup(&id.as_str().into()).is_some()
        {
            self.notice = "Close this session before deleting (/close)".into();
            return;
        }
        let now = self.clock.now();
        self.list_generation = self.list_generation.saturating_add(1);
        self.recently_deleted.insert(id.clone());
        let key = if let Some(entry) = self.entries.iter_mut().find(|e| e.id() == id) {
            entry.set_busy("deleting", now);
            Some(entry.key.clone())
        } else {
            None
        };
        let host = self.host.clone();
        let conv_id: tars_core::session::ConversationId = id.clone().into();
        self.spawn_job(key, super::jobs::JobKind::Delete, async move {
            let result = host.delete(conv_id).await;
            Message::Deleted(id, result)
        });
    }

    pub(super) fn request_external_edit(&mut self) {
        if self.startup.is_some()
            || self.quitting
            || self.confirmation.is_some()
            || self.panel.is_some()
            || self.form.is_some()
            || self.onboarding.is_some()
        {
            return;
        }
        let Some(entry) = self.entries.get_mut(self.selected) else {
            self.notice = "Open a session first to edit a prompt externally".into();
            return;
        };
        if entry.workspace_rules_trust.is_some()
            || entry.workspace_agents_trust.is_some()
            || !entry.approval_queue.is_empty()
        {
            return;
        }
        if entry.session.is_none() {
            entry.notice = "Open a session first to edit a prompt externally".into();
            return;
        }
        if entry.viewed_agent_id.is_some() {
            entry.notice = "Viewing child agent (read-only); run /agents main to edit".into();
            return;
        }
        if entry.busy.is_some() {
            entry.notice = "Wait for the current session operation before editing".into();
            return;
        }
        self.pending_external_edit = Some(super::state::PendingExternalEdit {
            key: entry.key.clone(),
            initial: entry.editor.lines().join("\n"),
        });
    }

    pub(super) fn apply_external_edit_result(
        &mut self,
        key: &super::state::Key,
        result: Result<String, String>,
    ) {
        self.dirty = true;
        let Some(entry) = self.entry_mut(key) else {
            return;
        };
        match result {
            Ok(new_text) => {
                input::set_editor_text(&mut entry.editor, &new_text, false);
                entry.reset_history_cursor();
                entry.notice = if new_text.trim().is_empty() {
                    "External edit cleared draft".into()
                } else {
                    "Updated draft from external editor".into()
                };
            }
            Err(error) => {
                entry.notice = error;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn run_pending_external_edit_with<F>(&mut self, runner: F) -> bool
    where
        F: FnOnce(&str) -> Result<String, String>,
    {
        let Some(pending) = self.pending_external_edit.take() else {
            return false;
        };
        let result = runner(&pending.initial);
        self.apply_external_edit_result(&pending.key, result);
        true
    }
}

fn drain_session_events(entry: &mut Entry, now: std::time::Instant) -> bool {
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
                                entry.turn_started_at.get_or_insert(now);
                            } else if status.is_terminal() {
                                entry.execution = match status {
                                    OperationStatus::Completed => {
                                        "Completed (background wakeup)".into()
                                    }
                                    other => format!("Background wakeup: {other}"),
                                };
                                entry.turn_started_at = None;
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
                        entry.turn_started_at = None;
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

fn sanitize_export_filename_segment(raw: &str) -> String {
    let mut slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    while slug.ends_with('_') {
        slug.pop();
    }
    if slug.is_empty() {
        "agent".to_owned()
    } else {
        slug
    }
}

enum CopyTarget {
    LastReply,
    CodeBlock(Option<usize>),
    SessionId,
}

fn collect_assistant_replies_from_blocks(blocks: &[causa::kernel::ContextBlock]) -> Vec<String> {
    let mut replies = Vec::new();
    for block in blocks {
        if let Some(source) = block.meta().source.as_deref()
            && (source == tars_core::compact::SUMMARY_SOURCE
                || tars_core::prelude::is_any_prelude_source(source)
                || source == tars_core::skills::SKILL_SOURCE
                || source == tars_core::events::TASK_NOTIFICATION_SOURCE)
        {
            continue;
        }
        let causa::kernel::BlockContent::Message {
            role: causa::kernel::Role::Assistant,
            parts,
        } = block.content()
        else {
            continue;
        };
        let mut buf = String::new();
        let mut seen_media = false;
        for part in parts {
            match part {
                causa::kernel::ContentPart::Text(text) => {
                    if seen_media
                        && !buf.is_empty()
                        && !buf.ends_with('\n')
                        && !text.0.starts_with('\n')
                    {
                        buf.push('\n');
                    }
                    seen_media = false;
                    buf.push_str(&text.0);
                }
                causa::kernel::ContentPart::Media(_) => {
                    seen_media = true;
                }
                causa::kernel::ContentPart::Sealed(_) => {}
            }
        }
        if !buf.trim().is_empty() {
            replies.push(buf);
        }
    }
    replies
}

fn is_assistant_role_header(header: &str) -> bool {
    let clean = header.trim().trim_start_matches('●').trim();
    if clean.eq_ignore_ascii_case("assistant") || clean.eq_ignore_ascii_case("model") {
        return true;
    }
    if clean.is_empty()
        || clean.eq_ignore_ascii_case("system")
        || clean.eq_ignore_ascii_case("user")
        || clean.starts_with("Context summary")
        || clean.starts_with("Session prelude")
        || clean.starts_with("Prelude update")
        || clean.starts_with("Skill")
        || clean.starts_with("Task notification")
    {
        return false;
    }
    true
}

fn collect_assistant_replies_from_transcript(
    transcript: &crate::transcript::Transcript,
) -> Vec<String> {
    use crate::transcript::TranscriptSegment;
    let segments = &transcript.segments;
    let mut replies = Vec::new();
    let mut idx = 0;
    while idx < segments.len() {
        let TranscriptSegment::RoleHeader(header) = &segments[idx] else {
            idx += 1;
            continue;
        };
        let clean_empty = header.trim().trim_start_matches('●').trim().is_empty();
        let header_is_assistant = is_assistant_role_header(header);
        idx += 1;
        let mut buf = String::new();
        let mut saw_markdown = false;
        while idx < segments.len() {
            match &segments[idx] {
                TranscriptSegment::MarkdownBody(body) => {
                    saw_markdown = true;
                    if !buf.is_empty() && !buf.ends_with('\n') && !body.starts_with('\n') {
                        buf.push('\n');
                    }
                    buf.push_str(body);
                    idx += 1;
                }
                TranscriptSegment::PlainBody(body) => {
                    if !buf.is_empty() && !buf.ends_with('\n') && !body.starts_with('\n') {
                        buf.push('\n');
                    }
                    buf.push_str(body);
                    idx += 1;
                }
                _ => break,
            }
        }
        if (header_is_assistant || (clean_empty && saw_markdown)) && !buf.trim().is_empty() {
            replies.push(buf);
        }
    }
    replies
}

fn extract_fenced_code_blocks(markdown: &str) -> Vec<(Option<String>, String)> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(markdown, options);
    let mut blocks = Vec::new();
    let mut active: Option<(Option<String>, String)> = None;
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                let lang = info
                    .split_whitespace()
                    .next()
                    .filter(|s| !s.is_empty())
                    .map(crate::transcript::display_single_line);
                active = Some((lang, String::new()));
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Indented)) => {
                active = None;
            }
            Event::Text(text) => {
                if let Some((_, buf)) = &mut active {
                    buf.push_str(&text);
                }
            }
            Event::Html(html) => {
                if let Some((_, buf)) = &mut active {
                    buf.push_str(&html);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, buf)) = &mut active {
                    buf.push('\n');
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, raw)) = active.take() {
                    let code = raw
                        .strip_suffix("\r\n")
                        .or_else(|| raw.strip_suffix('\n'))
                        .unwrap_or(&raw)
                        .to_owned();
                    blocks.push((lang, code));
                }
            }
            _ => {}
        }
    }
    blocks
}

fn is_completion_notice(notice: &str) -> bool {
    notice.starts_with("Commands: ")
        || notice.starts_with("Paths: ")
        || notice.starts_with("Profiles: ")
        || notice.starts_with("Image: ")
        || notice.starts_with("Copy: ")
        || notice.starts_with("Subcommands: ")
        || notice.starts_with("No matching ")
        || notice.starts_with("No workspace ")
        || notice.starts_with("Invalid workspace ")
}

fn apply_completion_result(target: &mut String, result: input::CompleteResult) {
    match result {
        input::CompleteResult::None => {}
        input::CompleteResult::Modified { notice } => {
            if let Some(notice) = notice {
                *target = notice;
            } else if is_completion_notice(target) {
                target.clear();
            }
        }
        input::CompleteResult::Notice(notice) => {
            *target = notice;
        }
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
