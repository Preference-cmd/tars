use super::state::{App, Confirmation};
use crate::{input, render};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use std::{io, time::Duration};
use tars_core::application::ClosePolicy;

impl App {
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Environment checks begin after terminal initialization; the main
        // screen is only drawn once the splash settles.
        self.begin_startup();
        loop {
            tokio::select! {
                event = events.next() => match event {
                    Some(Ok(event)) => {
                        self.event(event);
                        if let Some(pending) = self.pending_external_edit.take() {
                            let edit_result = match crate::input::editor::resolve_editor_command() {
                                Ok(cmd) => {
                                    drop(events);
                                    let res = match crate::terminal::run_with_suspended_terminal(
                                        terminal,
                                        || {
                                            crate::input::editor::edit_prompt_with_command(
                                                &cmd,
                                                &pending.initial,
                                            )
                                        },
                                    ) {
                                        Ok(res) => res,
                                        Err(io_err) => {
                                            Err(format!("Terminal suspend/resume failed: {io_err}"))
                                        }
                                    };
                                    events = EventStream::new();
                                    res
                                }
                                Err(err) => Err(err),
                            };
                            self.apply_external_edit_result(&pending.key, edit_result);
                            let size = terminal.size()?;
                            self.view_width = render::transcript_width(size.width);
                            self.view_height = size.height;
                            self.refresh_size_flags();
                            self.viewport_height =
                                render::conversation_height(size.height, size.width, self);
                            terminal.draw(|frame| render::draw(frame, self))?;
                            self.dirty = false;
                        }
                    }
                    Some(Err(error)) => return Err(error),
                    None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "terminal input closed")),
                },
                result = self.jobs.join_next(), if !self.jobs.is_empty() => {
                    match result {
                        Some(Ok(message)) => self.complete(message),
                        // A cancelled or ended adapter must not close other
                        // sessions. Panics already arrive as JobPanicked.
                        // Quitting the splash aborts its checks, so a cancelled
                        // startup job is not a UI failure.
                        Some(Err(error)) if error.is_cancelled() => {}
                        Some(Err(error)) => {
                            self.notice = format!("UI task ended: {error}");
                            self.dirty = true;
                        }
                        None => {},
                    }
                },
                _ = tick.tick() => {
                    let size = terminal.size()?;
                    self.view_width = render::transcript_width(size.width);
                    self.view_height = size.height;
                    self.refresh_size_flags();
                    self.viewport_height = render::conversation_height(size.height, size.width, self);
                    self.poll();
                    if self.done { return Ok(()); }
                    if self.dirty {
                        terminal.draw(|frame| render::draw(frame, self))?;
                        self.dirty = false;
                    }
                },
            }
        }
    }
    pub(super) fn event(&mut self, event: Event) {
        self.dirty = true;
        if self.startup.is_some() {
            self.startup_event(event);
            return;
        }
        self.update_approval_guard();
        self.update_workspace_trust_guard();
        self.update_repair_guard();
        self.update_folder_trust_guard();
        self.update_ctrl_c_exit_grace();
        let is_guarded = self.is_approval_guarded();
        let is_workspace_trust_guarded = self.is_workspace_trust_guarded();
        if let Event::Resize(width, height) = event {
            self.view_width = render::transcript_width(width);
            self.view_height = height;
            self.refresh_size_flags();
            self.viewport_height = render::conversation_height(height, width, self);
            if let Some(entry) = self.entries.get_mut(self.selected)
                && entry.preview.is_active()
            {
                entry.preview.layout(self.view_width);
            }
            if let Some(super::Panel::Info {
                kind,
                lines,
                scroll,
                ..
            }) = &mut self.panel
            {
                *scroll = (*scroll).min(
                    crate::transcript::info_panel_row_count(kind, lines, self.view_width)
                        .saturating_sub(1),
                );
            }
            if let Some(super::Panel::Repair(panel)) = &mut self.panel {
                panel.scroll = panel.scroll.min(
                    crate::transcript::detail_rows(&panel.lines, self.view_width)
                        .count()
                        .saturating_sub(1),
                );
            }
            self.update_approval_guard();
            self.update_workspace_trust_guard();
            return;
        }
        if let Event::Paste(text) = &event {
            if self.quitting || self.confirmation.is_some() {
                return;
            }
            self.ctrl_c_exit_armed = false;
            if let Some(panel) = &mut self.panel {
                match panel {
                    super::commands::Panel::Commands { query, selected }
                    | super::commands::Panel::Sessions { query, selected }
                    | super::commands::Panel::Models { query, selected } => {
                        query.extend(text.chars().filter(|c| !c.is_control()));
                        *selected = 0;
                    }
                    super::commands::Panel::Provider(wizard) => {
                        wizard.paste_single_line(text);
                    }
                    _ => {}
                }
                return;
            }
            if self.folder_trust.is_some() {
                return;
            } else if let Some(entry) = self.entries.get_mut(self.selected) {
                if entry.workspace_rules_trust.is_some() || entry.workspace_agents_trust.is_some() {
                    return;
                }
                if let Some(reason) = &mut entry.approval_reason {
                    if !is_guarded {
                        reason.extend(text.chars().filter(|c| !c.is_control()));
                    }
                } else if entry.approval_queue.is_empty() {
                    self.paste_into_composer(text);
                }
            } else {
                input::paste(&mut self.scratch_editor, text, true);
            }
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.kind == KeyEventKind::Repeat && key.modifiers.contains(KeyModifiers::CONTROL) {
            return;
        }
        if !input::control(key, 'c') {
            self.ctrl_c_exit_armed = false;
        }
        if let Some(confirmation) = self.confirmation.take() {
            if key.kind != KeyEventKind::Press {
                self.confirmation = Some(confirmation);
                return;
            }
            if input::control(key, 'c') {
                match confirmation {
                    Confirmation::Quit { .. } => self.quitting = true,
                    Confirmation::Close { .. }
                    | Confirmation::Discard
                    | Confirmation::DeleteSession { .. } => {}
                }
                self.update_approval_guard();
                self.update_workspace_trust_guard();
                return;
            }
            match key.code {
                KeyCode::Char('y') => match confirmation {
                    Confirmation::Quit { .. } => self.quitting = true,
                    Confirmation::Close { .. } => self.close(self.selected, ClosePolicy::Save),
                    Confirmation::Discard => self.close(self.selected, ClosePolicy::DiscardLocal),
                    Confirmation::DeleteSession { id, .. } => self.delete_session(id),
                },
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.confirmation = Some(confirmation),
            }
            self.update_approval_guard();
            self.update_workspace_trust_guard();
            return;
        }
        if self.quitting {
            return;
        }
        if input::control(key, 'q') {
            self.request_quit();
            return;
        }
        if input::control(key, 'c') {
            self.handle_ctrl_c();
            return;
        }
        if self.panel.is_some() {
            self.panel_key(key);
            self.update_approval_guard();
            self.update_workspace_trust_guard();
            return;
        }
        if self.folder_trust.is_some() {
            self.folder_trust_key(key);
            self.update_approval_guard();
            self.update_workspace_trust_guard();
            return;
        }
        let trust_kind = self.entries.get(self.selected).and_then(|entry| {
            if entry.workspace_rules_trust.is_some() {
                Some(TrustPromptKind::Rules)
            } else if entry.workspace_agents_trust.is_some() {
                Some(TrustPromptKind::Agents)
            } else {
                None
            }
        });
        if let Some(kind) = trust_kind {
            self.workspace_trust_key(key, kind, is_workspace_trust_guarded);
            return;
        }
        if let Some(entry) = self.entries.get_mut(self.selected)
            && !entry.approval_queue.is_empty()
        {
            if key.kind != KeyEventKind::Press {
                return;
            }
            // The approval panel needs 30 x 19 to contain its body. Keep only
            // the escape hatches that do not decide the pending request.
            if self.too_small || self.view_height < 19 || self.view_width.saturating_add(4) < 30 {
                if key.code == KeyCode::Esc || input::control(key, 'c') {
                    clear_approvals(entry);
                    self.cancel();
                    self.update_approval_guard();
                }
                return;
            }

            // Accidental-key guard: right after the request on screen appears or
            // changes without a key press, keys typed for something else must not
            // decide it. Only cancel and scroll act for 500 ms; the rest is dropped.
            if is_guarded {
                if key.code == KeyCode::Esc || input::control(key, 'c') {
                    clear_approvals(entry);
                    self.cancel();
                    self.update_approval_guard();
                } else {
                    scroll_approval(entry, key.code);
                }
                return;
            }

            if input::control(key, 'w') {
                self.request_close();
                return;
            }
            let is_backtab = key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
            if is_backtab {
                self.cycle_permission_mode();
                return;
            }
            if entry.approval_reason.is_some() {
                match key.code {
                    KeyCode::Enter => {
                        let reason = entry.approval_reason.take().unwrap();
                        let trimmed = reason.trim();
                        let reason_opt = if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        };
                        if let Some(req) = entry.approval_queue.pop_front() {
                            req.respond(tars_core::permission::ApprovalDecision::Deny {
                                reason: reason_opt,
                            });
                            entry.approval_scroll = 0;
                        }
                        self.update_approval_guard();
                        return;
                    }
                    KeyCode::Esc => {
                        entry.approval_reason = None;
                    }
                    KeyCode::Backspace => {
                        if let Some(r) = &mut entry.approval_reason {
                            r.pop();
                        }
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && !c.is_control() =>
                    {
                        if let Some(r) = &mut entry.approval_reason {
                            r.push(c);
                        }
                    }
                    _ => {}
                }
                return;
            }

            let (is_command, has_command_rules, can_save_workspace_rule) =
                match entry.approval_queue.front().map(|req| &req.subject) {
                    Some(tars_core::permission::ApprovalSubject::Command {
                        suggested_prefixes,
                        can_save_workspace_rule,
                        ..
                    }) => (
                        true,
                        !suggested_prefixes.is_empty(),
                        !suggested_prefixes.is_empty() && *can_save_workspace_rule,
                    ),
                    _ => (false, false, false),
                };

            let no_ctrl_alt = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y') if no_ctrl_alt => {
                    if let Some(req) = entry.approval_queue.pop_front() {
                        req.respond(tars_core::permission::ApprovalDecision::Allow);
                        entry.approval_scroll = 0;
                    }
                    self.update_approval_guard();
                    return;
                }
                KeyCode::Char('1') if is_command && no_ctrl_alt => {
                    if let Some(req) = entry.approval_queue.pop_front() {
                        req.respond(tars_core::permission::ApprovalDecision::Allow);
                        entry.approval_scroll = 0;
                    }
                    self.update_approval_guard();
                    return;
                }
                KeyCode::Char('s' | 'S' | '2') if has_command_rules && no_ctrl_alt => {
                    if let Some(req) = entry.approval_queue.pop_front() {
                        req.respond(tars_core::permission::ApprovalDecision::AllowForSession);
                        entry.approval_scroll = 0;
                    }
                    self.update_approval_guard();
                    return;
                }
                KeyCode::Char('w' | 'W' | '3') if can_save_workspace_rule && no_ctrl_alt => {
                    if let Some(req) = entry.approval_queue.pop_front() {
                        req.respond(tars_core::permission::ApprovalDecision::AllowInWorkspace);
                        entry.approval_scroll = 0;
                    }
                    self.update_approval_guard();
                    return;
                }
                KeyCode::Char('n' | 'N') if no_ctrl_alt => {
                    entry.approval_reason = Some(String::new());
                }
                KeyCode::Esc | KeyCode::Char('c')
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        || key.code == KeyCode::Esc =>
                {
                    clear_approvals(entry);
                    self.cancel();
                    self.update_approval_guard();
                    return;
                }
                code => scroll_approval(entry, code),
            }
            return;
        }
        let is_backtab = key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        if is_backtab {
            self.cycle_permission_mode();
            return;
        }
        if key.kind == KeyEventKind::Press && key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.complete_input();
            return;
        }
        if key.kind == KeyEventKind::Press
            && (key.code == KeyCode::F(1)
                || (key.code == KeyCode::Char('/')
                    && key.modifiers.is_empty()
                    && !self.has_active_text_draft()))
        {
            self.show_commands();
            return;
        }
        if key.code == KeyCode::Enter {
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            {
                self.active_editor_mut().insert_newline();
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    entry.reset_history_cursor();
                }
            } else if key.kind == KeyEventKind::Press {
                self.submit();
            }
            return;
        }
        if input::control(key, 'j') {
            self.active_editor_mut().insert_newline();
            if let Some(entry) = self.entries.get_mut(self.selected) {
                entry.reset_history_cursor();
            }
            return;
        }
        if input::control(key, 's') {
            self.submit();
        } else if input::control(key, 'c') {
            self.cancel();
        } else if input::control(key, 'r') {
            self.retry();
        } else if input::control(key, 'e') {
            self.export();
        } else if input::control(key, 'g') {
            self.request_external_edit();
        } else if input::control(key, 'o') {
            self.show_tools();
        } else if input::control(key, 'd') {
            self.confirmation = Some(Confirmation::Discard);
        } else if input::control(key, 'w') {
            self.request_close();
        } else if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('q' | 's' | 'c' | 'r' | 'e' | 'g' | 'o' | 'd' | 'w')
            )
        {
            // Repeated command keys never fall through into text editing.
        } else {
            match key.code {
                KeyCode::F(2) if key.kind == KeyEventKind::Press => {
                    self.create_new_session();
                }
                KeyCode::F(3) if key.kind == KeyEventKind::Press => self.open(),
                KeyCode::F(4) if key.kind == KeyEventKind::Press => {
                    if !self.entries.is_empty() {
                        self.selected = (self.selected + 1) % self.entries.len();
                        self.refresh_size_flags();
                        self.update_approval_guard();
                        self.update_workspace_trust_guard();
                    }
                }
                KeyCode::F(5) if key.kind == KeyEventKind::Press => self.list(),
                KeyCode::F(6) if key.kind == KeyEventKind::Press => self.show_tools(),
                KeyCode::F(7) if key.kind == KeyEventKind::Press => self.show_repair(),
                KeyCode::Esc if key.kind == KeyEventKind::Press => self.cancel(),
                KeyCode::PageUp | KeyCode::PageDown => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        let page = entry.viewport(self.viewport_height).page;
                        if key.code == KeyCode::PageUp {
                            scroll_up_lines(entry, self.viewport_height, page, false);
                        } else {
                            scroll_down_lines(entry, self.viewport_height, page);
                        }
                    }
                }
                KeyCode::Up
                    if key.modifiers == KeyModifiers::ALT
                        || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        scroll_up_lines(entry, self.viewport_height, 3, true);
                    }
                }
                KeyCode::Down
                    if key.modifiers == KeyModifiers::ALT
                        || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        scroll_down_lines(entry, self.viewport_height, 3);
                    }
                }
                KeyCode::Char('u' | 'U')
                    if key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        let half = (entry.viewport(self.viewport_height).page / 2).max(1);
                        scroll_up_lines(entry, self.viewport_height, half, true);
                    }
                }
                KeyCode::Char('d' | 'D')
                    if key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        let half = (entry.viewport(self.viewport_height).page / 2).max(1);
                        scroll_down_lines(entry, self.viewport_height, half);
                    }
                }
                KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        scroll_to_top(entry);
                    }
                }
                KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        scroll_to_bottom(entry);
                    }
                }
                KeyCode::Home if key.modifiers.is_empty() => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        if entry.has_text_draft() {
                            input::edit(&mut entry.editor, key);
                        } else {
                            scroll_to_top(entry);
                        }
                    } else if self.scratch_has_text_draft() {
                        input::edit(&mut self.scratch_editor, key);
                    }
                }
                KeyCode::End if key.modifiers.is_empty() => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        if entry.has_text_draft() {
                            input::edit(&mut entry.editor, key);
                        } else {
                            scroll_to_bottom(entry);
                        }
                    } else if self.scratch_has_text_draft() {
                        input::edit(&mut self.scratch_editor, key);
                    }
                }
                KeyCode::Up if key.modifiers.is_empty() => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        if !entry.step_history_up() {
                            input::edit(&mut entry.editor, key);
                        }
                    } else {
                        input::edit(&mut self.scratch_editor, key);
                    }
                }
                KeyCode::Down if key.modifiers.is_empty() => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        if !entry.step_history_down() {
                            input::edit(&mut entry.editor, key);
                        }
                    } else {
                        input::edit(&mut self.scratch_editor, key);
                    }
                }
                _ => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        input::edit(&mut entry.editor, key);
                        if !matches!(
                            key.code,
                            KeyCode::Left
                                | KeyCode::Right
                                | KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::Home
                                | KeyCode::End
                        ) {
                            entry.reset_history_cursor();
                        }
                    } else {
                        input::edit(&mut self.scratch_editor, key);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum TrustPromptKind {
    Rules,
    Agents,
}

impl App {
    fn handle_ctrl_c(&mut self) {
        if self.folder_trust.is_some() {
            self.quitting = true;
            return;
        }
        let now = self.clock.now();
        if let Some(entry) = self.entries.get_mut(self.selected)
            && !entry.approval_queue.is_empty()
        {
            clear_approvals(entry);
            self.cancel();
            self.update_approval_guard();
            self.last_ctrl_c_cancel_at = Some(now);
            self.ctrl_c_exit_armed = false;
            return;
        }
        let trust_kind = self.entries.get(self.selected).and_then(|entry| {
            if entry.workspace_rules_trust.is_some() {
                Some(TrustPromptKind::Rules)
            } else if entry.workspace_agents_trust.is_some() {
                Some(TrustPromptKind::Agents)
            } else {
                None
            }
        });
        if let Some(kind) = trust_kind {
            self.decline_workspace_trust(kind);
            return;
        }
        if self.entry_has_active_work() {
            self.cancel();
            self.last_ctrl_c_cancel_at = Some(now);
            self.ctrl_c_exit_armed = false;
            self.set_active_notice("Cancellation requested; press Ctrl-C again once idle to exit");
            return;
        }
        let in_grace = self.last_ctrl_c_cancel_at.is_some_and(|cancel_at| {
            now.saturating_duration_since(cancel_at) < super::state::CTRL_C_EXIT_GRACE_WINDOW
        });
        if in_grace && !self.ctrl_c_exit_armed {
            self.ctrl_c_exit_armed = true;
            self.set_active_notice("Press Ctrl-C again to exit");
            return;
        }
        if let Some(super::commands::Panel::Provider(wizard)) = self.panel.as_mut() {
            wizard.clear_secret();
        }
        self.panel = None;
        self.last_ctrl_c_cancel_at = None;
        self.ctrl_c_exit_armed = false;
        self.request_quit();
    }

    fn folder_trust_key(&mut self, key: crossterm::event::KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if input::control(key, 'c') || input::control(key, 'q') {
            self.quitting = true;
            return;
        }
        if self.too_small {
            return;
        }
        let no_ctrl_alt = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if !no_ctrl_alt {
            return;
        }
        let is_guarded = self.is_folder_trust_guarded();
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Char('j' | 'k') => {
                if let Some(wizard) = self.folder_trust.as_mut() {
                    wizard.toggle();
                }
            }
            KeyCode::Esc | KeyCode::Char('2' | 'n' | 'N') => {
                self.quitting = true;
            }
            KeyCode::Enter => {
                let selected = self.folder_trust.as_ref().map(|wizard| wizard.selected);
                match selected {
                    Some(1) => self.quitting = true,
                    Some(0) if !is_guarded => self.confirm_folder_trust(),
                    _ => {}
                }
            }
            KeyCode::Char('1' | 'y' | 'Y') if !is_guarded => {
                self.confirm_folder_trust();
            }
            _ => {}
        }
    }

    fn workspace_trust_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        kind: TrustPromptKind,
        is_guarded: bool,
    ) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.too_small || self.view_height < 19 || self.view_width.saturating_add(4) < 30 {
            if key.code == KeyCode::Esc {
                self.decline_workspace_trust(kind);
            }
            return;
        }
        if is_guarded {
            if let Some(entry) = self.entries.get_mut(self.selected) {
                scroll_workspace_trust(entry, kind, key.code);
            }
            return;
        }
        if input::control(key, 'w') {
            self.request_close();
            return;
        }
        let no_ctrl_alt = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y') if no_ctrl_alt => match kind {
                TrustPromptKind::Rules => self.trust_workspace_rules(),
                TrustPromptKind::Agents => self.trust_workspace_agents(),
            },
            KeyCode::Char('n' | 'N') if no_ctrl_alt => self.decline_workspace_trust(kind),
            KeyCode::Esc => self.decline_workspace_trust(kind),
            code => {
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    scroll_workspace_trust(entry, kind, code);
                }
            }
        }
    }

    fn decline_workspace_trust(&mut self, kind: TrustPromptKind) {
        if let Some(entry) = self.entries.get_mut(self.selected) {
            match kind {
                TrustPromptKind::Rules => {
                    entry.workspace_rules_trust = None;
                    entry.notice =
                        "Declined workspace allow rules; only deny/ask and user rules are active"
                            .into();
                }
                TrustPromptKind::Agents => {
                    entry.workspace_agents_trust = None;
                    entry.notice =
                        "Declined workspace agent presets; only built-in/user/configured agents are active"
                            .into();
                }
            }
        }
        self.update_workspace_trust_guard();
    }
}

fn scroll_workspace_trust(entry: &mut super::Entry, kind: TrustPromptKind, code: KeyCode) {
    let (len, scroll) = match kind {
        TrustPromptKind::Rules => {
            let Some(prompt) = entry.workspace_rules_trust.as_mut() else {
                return;
            };
            (prompt.body_lines.len(), &mut prompt.scroll)
        }
        TrustPromptKind::Agents => {
            let Some(prompt) = entry.workspace_agents_trust.as_mut() else {
                return;
            };
            (prompt.body_lines.len(), &mut prompt.scroll)
        }
    };
    let max = len.saturating_sub(1);
    *scroll = match code {
        KeyCode::Up => scroll.saturating_sub(1),
        KeyCode::Down => scroll.saturating_add(1).min(max),
        KeyCode::PageUp => scroll.saturating_sub(5),
        KeyCode::PageDown => scroll.saturating_add(5).min(max),
        _ => return,
    };
}

fn clear_approvals(entry: &mut super::Entry) {
    entry.approval_queue.clear();
    entry.approval_reason = None;
    entry.approval_scroll = 0;
}

fn scroll_approval(entry: &mut super::Entry, code: KeyCode) {
    let max = approval_max_scroll(entry);
    entry.approval_scroll = match code {
        KeyCode::Up => entry.approval_scroll.saturating_sub(1),
        KeyCode::Down => entry.approval_scroll.saturating_add(1).min(max),
        KeyCode::PageUp => entry.approval_scroll.saturating_sub(5),
        KeyCode::PageDown => entry.approval_scroll.saturating_add(5).min(max),
        _ => return,
    };
}

impl App {
    fn paste_into_composer(&mut self, text: &str) {
        let Some(entry) = self.entries.get_mut(self.selected) else {
            return;
        };
        if entry.viewed_agent_id.is_none()
            && let Some(tokens) = crate::input::image::parse_pasted_path_tokens(text)
        {
            let workspace_root = entry
                .session
                .as_ref()
                .and_then(|session| session.definition().workspace_root);
            let mut prepared = Vec::with_capacity(tokens.len());
            let mut all_images = true;
            for token in &tokens {
                let Some(resolved) =
                    crate::input::image::resolve_local_path(token, workspace_root.as_deref())
                else {
                    all_images = false;
                    break;
                };
                match tars_core::media::prepare_image_from_path(&resolved) {
                    Ok(image) => prepared.push(image),
                    Err(_) => {
                        all_images = false;
                        break;
                    }
                }
            }
            if all_images && !prepared.is_empty() {
                if entry.busy.is_some() {
                    entry.notice =
                        "Wait for the current session operation before attaching images".into();
                    return;
                }
                let max = tars_core::media::MAX_IMAGES_PER_MESSAGE;
                if entry.pending_images.len() + prepared.len() > max {
                    entry.notice = format!(
                        "Cannot attach {} image(s): exceeds max {max} per message ({} pending)",
                        prepared.len(),
                        entry.pending_images.len()
                    );
                    return;
                }
                let added = prepared.len();
                let summary = if added == 1 {
                    let img = &prepared[0];
                    let name = img.name.as_deref().unwrap_or("image");
                    let escaped_name = tars_core::permission::escape_untrusted_text(name);
                    let size = crate::input::image::format_human_size(img.size_bytes);
                    format!("{escaped_name} ({}, {size})", img.media_type)
                } else {
                    format!("{added} images")
                };
                entry.pending_images.extend(prepared);
                entry.notice = format!(
                    "Attached {summary} — {} pending",
                    entry.pending_images.len()
                );
                self.refresh_viewport_height();
                return;
            }
        }
        input::paste(&mut entry.editor, text, true);
        entry.reset_history_cursor();
    }
}

fn scroll_up_lines(
    entry: &mut super::Entry,
    viewport_height: usize,
    lines: usize,
    load_on_hit_top: bool,
) {
    let viewport = entry.viewport(viewport_height);
    let at_top = viewport.start == 0;
    let next = viewport.start.saturating_sub(lines);
    entry.scroll = Some(next);
    entry.scroll_anchor = None;
    if if load_on_hit_top { next == 0 } else { at_top } {
        entry.request_earlier_history();
    }
}

fn scroll_down_lines(entry: &mut super::Entry, viewport_height: usize, lines: usize) {
    let viewport = entry.viewport(viewport_height);
    let next = (viewport.start + lines).min(viewport.end);
    entry.scroll = (next < viewport.end).then_some(next);
    if entry.scroll.is_none() {
        entry.has_unread_output = false;
    }
    entry.scroll_anchor = None;
}

fn scroll_to_top(entry: &mut super::Entry) {
    entry.scroll = Some(0);
    entry.scroll_anchor = None;
    entry.request_earlier_history();
}

fn scroll_to_bottom(entry: &mut super::Entry) {
    entry.scroll = None;
    entry.scroll_anchor = None;
    entry.has_unread_output = false;
}

fn approval_max_scroll(entry: &super::Entry) -> usize {
    let Some(request) = entry.approval_queue.front() else {
        return 0;
    };
    let lines = match &request.subject {
        tars_core::permission::ApprovalSubject::FileWrite { diff, .. } => {
            tars_core::permission::escape_untrusted_text(diff)
                .lines()
                .count()
        }
        tars_core::permission::ApprovalSubject::Command { .. } => {
            tars_core::permission::command_body(&request.subject).map_or(0, |lines| lines.len())
        }
        tars_core::permission::ApprovalSubject::McpCall { arguments, .. } => {
            tars_core::permission::escape_untrusted_text(arguments)
                .lines()
                .count()
        }
    };
    lines.saturating_sub(1)
}
