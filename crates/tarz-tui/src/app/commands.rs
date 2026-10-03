use super::{App, Confirmation};
use crate::input;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tarz_core::application::ClosePolicy;

pub(crate) enum Panel {
    Commands {
        query: String,
        selected: usize,
    },
    Sessions {
        query: String,
        selected: usize,
    },
    Info {
        title: String,
        lines: Vec<String>,
        scroll: u16,
    },
}

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("new", "Create a session with model settings"),
    ("sessions", "Find and open a saved or active session"),
    ("help", "Keyboard shortcuts and session behavior"),
    ("status", "Full session status, settings and error details"),
    ("open", "Open the current stored session"),
    ("cancel", "Interrupt the current work"),
    ("retry", "Retry the original blocked save"),
    ("export", "Export current materials for recovery"),
    ("close", "Save and close the current session"),
    ("discard", "Confirm discarding local unsaved materials"),
    ("quit", "Save sessions and exit Tarz"),
];

pub(crate) fn matching_commands(query: &str) -> Vec<usize> {
    let query = query.trim_start_matches('/').to_lowercase();
    COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, (name, _))| name.contains(&query).then_some(i))
        .collect()
}

impl App {
    pub(crate) fn matching_sessions(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                (entry.id().to_lowercase().contains(&query)
                    || entry.title.to_lowercase().contains(&query))
                .then_some(i)
            })
            .collect()
    }
    pub(super) fn show_commands(&mut self) {
        self.panel = Some(Panel::Commands {
            query: String::new(),
            selected: 0,
        });
    }
    pub(super) fn request_quit(&mut self) {
        if self.form.is_some() || self.entries.iter().any(|e| e.has_draft()) {
            self.confirmation = Some(Confirmation::Quit);
        } else {
            self.quitting = true;
        }
    }
    pub(super) fn request_close(&mut self) {
        if self
            .entries
            .get(self.selected)
            .is_some_and(|e| e.has_draft())
        {
            self.confirmation = Some(Confirmation::Close);
        } else {
            self.close(self.selected, ClosePolicy::Save);
        }
    }
    fn command(&mut self, index: usize) {
        match COMMANDS[index].0 {
            "new" => self.form = Some(input::Form::new(self.options.preset.as_ref())),
            "sessions" => {
                self.list();
                self.panel = Some(Panel::Sessions {
                    query: String::new(),
                    selected: self.selected,
                });
            }
            "help" => self.panel = Some(Panel::Info {
                title: "Keyboard shortcuts".into(), scroll: 0,
                lines: ["Enter / Ctrl-S   Send input", "Ctrl-J / Alt-Enter   New line",
                    "Esc / Ctrl-C   Interrupt work", "/ or F1   Commands; Esc returns",
                    "F2 new · F3 open · F4 next · F5 refresh", "PageUp / PageDown   Read conversation",
                    "Ctrl-Z undo · Ctrl-Y redo", "Ctrl-W close · Ctrl-Q save and quit",
                    "Ctrl-R retry save · Ctrl-E export", "Ctrl-D discard with confirmation", "",
                    "Switching views keeps work and drafts. Saved sessions restore materials, not old work records.",
                    "Use /status for full configuration, save state and error details."].into_iter().map(str::to_owned).collect(),
            }),
            "status" => {
                let mut lines = vec![format!("Config: {}", self.options.config_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into())),
                    format!("Store: {}", self.options.store.display()),
                    format!("Recovery exports: {}", self.options.recovery_dir.display())];
                if let Some(entry) = self.entries.get(self.selected) {
                    lines.extend([format!("Session: {}", entry.id()), format!("Model: {}", entry.title), entry.execution.clone(), entry.status.clone(), entry.notice.clone()]);
                }
                lines.push(self.notice.clone());
                self.panel = Some(Panel::Info { title: "Session details".into(), lines, scroll: 0 });
            }
            "open" => self.open(),
            "cancel" => self.cancel(),
            "retry" => self.retry(),
            "export" => self.export(),
            "close" => self.request_close(),
            "discard" => self.confirmation = Some(Confirmation::Discard),
            "quit" => self.request_quit(),
            _ => unreachable!("commands are a closed local set"),
        }
    }
    pub(super) fn panel_key(&mut self, key: KeyEvent) {
        let Some(mut panel) = self.panel.take() else {
            return;
        };
        if key.code == KeyCode::Esc {
            return;
        }
        let (query, selected, commands) = match &mut panel {
            Panel::Commands { query, selected } => (query, selected, true),
            Panel::Sessions { query, selected } => (query, selected, false),
            Panel::Info { scroll, lines, .. } => {
                let max = lines
                    .iter()
                    .map(|l| {
                        l.chars()
                            .count()
                            .div_ceil(self.view_width.max(1).into())
                            .max(1)
                    })
                    .sum::<usize>()
                    .saturating_sub(1)
                    .min(u16::MAX as usize) as u16;
                match key.code {
                    KeyCode::Down => *scroll = scroll.saturating_add(1).min(max),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(5).min(max),
                    KeyCode::Up => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(5),
                    KeyCode::Enter => return,
                    _ => {}
                }
                self.panel = Some(panel);
                return;
            }
        };
        let matches = if commands {
            matching_commands(query)
        } else {
            self.matching_sessions(query)
        };
        *selected = (*selected).min(matches.len().saturating_sub(1));
        match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(matches.len().saturating_sub(1)),
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if let Some(&index) = matches.get(*selected) {
                    if commands {
                        self.command(index);
                    } else {
                        self.selected = index;
                        self.open();
                    }
                    return;
                }
            }
            KeyCode::Backspace => {
                query.pop();
                *selected = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.intersects(
                    crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT,
                ) =>
            {
                query.push(c);
                *selected = 0;
            }
            _ => {}
        }
        self.panel = Some(panel);
    }
}
