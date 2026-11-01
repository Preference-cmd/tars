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
                    Some(Ok(event)) => self.event(event),
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
        self.update_repair_guard();
        let is_guarded = self.is_approval_guarded();
        if let Event::Resize(width, height) = event {
            self.view_width = render::transcript_width(width);
            self.view_height = height;
            self.refresh_size_flags();
            self.viewport_height = render::conversation_height(height, width, self);
            if let Some(super::Panel::Info { lines, scroll, .. }) = &mut self.panel {
                *scroll = (*scroll).min(
                    crate::transcript::detail_rows(lines, self.view_width)
                        .count()
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
            return;
        }
        if let Event::Paste(text) = &event {
            if self.quitting || self.confirmation.is_some() {
                return;
            }
            if let Some(panel) = &mut self.panel {
                if let super::commands::Panel::Commands { query, selected }
                | super::commands::Panel::Sessions { query, selected } = panel
                {
                    query.extend(text.chars().filter(|c| !c.is_control()));
                    *selected = 0;
                }
                return;
            }
            if let Some(form) = &mut self.form {
                input::paste(&mut form.fields[form.selected], text, false);
            } else if let Some(entry) = self.entries.get_mut(self.selected) {
                if let Some(reason) = &mut entry.approval_reason {
                    if !is_guarded {
                        reason.extend(text.chars().filter(|c| !c.is_control()));
                    }
                } else if entry.approval_queue.is_empty() {
                    input::paste(&mut entry.editor, text, true);
                }
            }
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if let Some(confirmation) = self.confirmation.take() {
            if key.kind != KeyEventKind::Press {
                self.confirmation = Some(confirmation);
                return;
            }
            match key.code {
                KeyCode::Char('y') => match confirmation {
                    Confirmation::Quit => self.quitting = true,
                    Confirmation::Close => self.close(self.selected, ClosePolicy::Save),
                    Confirmation::Discard => self.close(self.selected, ClosePolicy::DiscardLocal),
                },
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.confirmation = Some(confirmation),
            }
            self.update_approval_guard();
            return;
        }
        if self.quitting {
            return;
        }
        if input::control(key, 'q') {
            self.request_quit();
            return;
        }
        if self.panel.is_some() {
            self.panel_key(key);
            self.update_approval_guard();
            return;
        }
        if let Some(form) = &mut self.form {
            if input::control(key, 's') {
                self.create();
            } else if key.code == KeyCode::Esc {
                self.form = None;
            } else {
                form.key(key);
            }
            self.update_approval_guard();
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

            match key.code {
                KeyCode::Enter | KeyCode::Char('y' | 'Y')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(req) = entry.approval_queue.pop_front() {
                        req.respond(tars_core::permission::ApprovalDecision::Allow);
                        entry.approval_scroll = 0;
                    }
                    self.update_approval_guard();
                    return;
                }
                KeyCode::Char('n' | 'N')
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
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
        if key.kind == KeyEventKind::Press
            && (key.code == KeyCode::F(1)
                || (key.code == KeyCode::Char('/')
                    && key.modifiers.is_empty()
                    && self
                        .entries
                        .get(self.selected)
                        .is_none_or(|e| !e.has_draft())))
        {
            self.show_commands();
            return;
        }
        if key.code == KeyCode::Enter {
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
            {
                if let Some(entry) = self.entries.get_mut(self.selected) {
                    entry.editor.insert_newline();
                }
            } else if key.kind == KeyEventKind::Press {
                self.submit();
            }
            return;
        }
        if input::control(key, 'j') {
            if let Some(entry) = self.entries.get_mut(self.selected) {
                entry.editor.insert_newline();
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
        } else if input::control(key, 'd') {
            self.confirmation = Some(Confirmation::Discard);
        } else if input::control(key, 'w') {
            self.request_close();
        } else if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('q' | 's' | 'c' | 'r' | 'e' | 'd' | 'w')
            )
        {
            // Repeated command keys never fall through into text editing.
        } else {
            match key.code {
                KeyCode::F(2) if key.kind == KeyEventKind::Press => {
                    self.form = Some(input::Form::new(
                        self.options.config.clone(),
                        self.options.profile.clone(),
                        self.options.preset.as_ref(),
                    ))
                }
                KeyCode::F(3) if key.kind == KeyEventKind::Press => self.open(),
                KeyCode::F(4) if key.kind == KeyEventKind::Press => {
                    if !self.entries.is_empty() {
                        self.selected = (self.selected + 1) % self.entries.len();
                        self.refresh_size_flags();
                        self.update_approval_guard();
                    }
                }
                KeyCode::F(5) if key.kind == KeyEventKind::Press => self.list(),
                KeyCode::F(6) if key.kind == KeyEventKind::Press => self.show_tools(),
                KeyCode::F(7) if key.kind == KeyEventKind::Press => self.show_repair(),
                KeyCode::Esc if key.kind == KeyEventKind::Press => self.cancel(),
                KeyCode::PageUp | KeyCode::PageDown => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        let viewport = entry.viewport(self.viewport_height);
                        let at_top = viewport.start == 0;
                        let end = viewport.end;
                        let offset = viewport.start;
                        let page = viewport.page;
                        entry.scroll = if key.code == KeyCode::PageUp {
                            Some(offset.saturating_sub(page))
                        } else {
                            let next = (offset + page).min(end);
                            (next < end).then_some(next)
                        };
                        if key.code == KeyCode::PageUp && at_top {
                            entry.request_earlier_history();
                        }
                    }
                }
                _ => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        input::edit(&mut entry.editor, key);
                    }
                }
            }
        }
    }
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
