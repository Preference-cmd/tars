use super::state::{App, Confirmation};
use crate::{input, render};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use std::{io, time::Duration};
use tarz_core::application::ClosePolicy;

impl App {
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.list();
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
                        Some(Err(error)) => return Err(io::Error::other(format!("UI adapter task failed: {error}"))),
                        None => {},
                    }
                },
                _ = tick.tick() => {
                    let size = terminal.size()?;
                    self.view_width = render::transcript_width(size.width);
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
        if let Event::Resize(width, height) = event {
            self.view_width = render::transcript_width(width);
            self.viewport_height = render::conversation_height(height, width, self);
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
                input::paste(&mut entry.editor, text, true);
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
                    self.form = Some(input::Form::new(self.options.preset.as_ref()))
                }
                KeyCode::F(3) if key.kind == KeyEventKind::Press => self.open(),
                KeyCode::F(4) => {
                    if !self.entries.is_empty() {
                        self.selected = (self.selected + 1) % self.entries.len();
                    }
                }
                KeyCode::F(5) if key.kind == KeyEventKind::Press => self.list(),
                KeyCode::Esc if key.kind == KeyEventKind::Press => self.cancel(),
                KeyCode::PageUp | KeyCode::PageDown => {
                    if let Some(entry) = self.entries.get_mut(self.selected) {
                        let viewport = entry.viewport(self.viewport_height);
                        let end = viewport.end;
                        let offset = viewport.start;
                        let page = viewport.page;
                        entry.scroll = if key.code == KeyCode::PageUp {
                            Some(offset.saturating_sub(page))
                        } else {
                            let next = (offset + page).min(end);
                            (next < end).then_some(next)
                        };
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
