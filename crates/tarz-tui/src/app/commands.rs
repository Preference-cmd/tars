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
        scroll: usize,
    },
    Repair(Box<super::repair::RepairPanel>),
    ModeSelect {
        selected: usize,
    },
}

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("new", "Create a session with model settings"),
    ("sessions", "Find and open a saved or active session"),
    ("mode", "Show or change the permission mode"),
    ("help", "Keyboard shortcuts and session behavior"),
    ("tools", "Expand recorded tool calls and complete results"),
    (
        "repair",
        "Inspect incomplete tool exchanges before continuing",
    ),
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
    let clean = query.trim_start_matches('/').trim().to_lowercase();
    let word = clean.split_whitespace().next().unwrap_or("");
    COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, (name, _))| {
            if name.contains(&clean) || (!word.is_empty() && *name == word) {
                Some(i)
            } else {
                None
            }
        })
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
    fn command(&mut self, index: usize, query: &str) {
        match COMMANDS[index].0 {
            "new" => {
                self.form = Some(input::Form::new(
                    self.options.config.clone(),
                    self.options.profile.clone(),
                    self.options.preset.as_ref(),
                ))
            }
            "sessions" => {
                self.list();
                self.panel = Some(Panel::Sessions {
                    query: String::new(),
                    selected: self.selected,
                });
            }
            "mode" => {
                let clean = query.trim_start_matches('/').trim();
                let arg = if let Some(rest) = clean.strip_prefix("mode") {
                    rest.trim()
                } else {
                    ""
                };
                if arg.is_empty() {
                    if let Some(entry) = self.entries.get(self.selected) {
                        if let Some(session) = &entry.session {
                            let current_mode = session
                                .permission_mode()
                                .map_or(tarz_core::permission::PermissionMode::Ask, |(m, _)| m);
                            let selected_idx = match current_mode {
                                tarz_core::permission::PermissionMode::ReadOnly => 0,
                                tarz_core::permission::PermissionMode::Ask => 1,
                                tarz_core::permission::PermissionMode::AcceptEdits => 2,
                                tarz_core::permission::PermissionMode::FullAccess => 3,
                            };
                            self.panel = Some(Panel::ModeSelect { selected: selected_idx });
                        } else {
                            self.notice = "Open a session first to change its permission mode".into();
                        }
                    } else {
                        self.notice = "Open a session first to change its permission mode".into();
                    }
                } else {
                    match arg.parse::<tarz_core::permission::PermissionMode>() {
                        Ok(mode) => {
                            self.apply_permission_mode(mode);
                        }
                        Err(err) => {
                            self.notice = err.to_string();
                        }
                    }
                }
            }
            "help" => self.panel = Some(Panel::Info {
                title: "Keyboard shortcuts".into(), scroll: 0,
                lines: ["Enter / Ctrl-S   Send input", "Ctrl-J / Alt-Enter   New line",
                    "Shift-Tab   Cycle mode (ask -> accept-edits -> read-only)",
                    "/mode [name]   Show or change the permission mode",
                    "Enter / y allow · n deny (optional reason)   Confirmation panel",
                    "Esc / Ctrl-C   Interrupt work", "/ or F1   Commands; Esc returns",
                    "F2 new · F3 open · F4 next · F5 refresh", "F6 / /tools   Expand complete recorded tool results", "F7 / /repair   Preview and apply material repair", "PageUp / PageDown   Read conversation",
                    "Ctrl-Z undo · Ctrl-Y redo", "Ctrl-W close · Ctrl-Q save and quit",
                    "Ctrl-R retry save · Ctrl-E export", "Ctrl-D discard with confirmation", "",
                    "Switching views keeps work and drafts. Saved sessions restore materials, not old work records.",
                    "Use /status for full configuration, save state and error details."].into_iter().map(str::to_owned).collect(),
            }),
            "tools" => self.show_tools(),
            "repair" => self.show_repair(),
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
    pub(super) fn show_tools(&mut self) {
        let mut lines = Vec::new();
        if let Some(entry) = self.entries.get(self.selected) {
            lines.extend(entry.transcript.tool_details.iter().cloned());
        }
        if lines.is_empty() {
            lines.push(
                "No committed tool materials. Returned pending results may not be committed."
                    .into(),
            );
        }
        self.panel = Some(Panel::Info {
            title: "Recorded tool materials · viewing never executes".into(),
            lines,
            scroll: 0,
        });
    }
    pub(super) fn panel_key(&mut self, key: KeyEvent) {
        if matches!(self.panel, Some(Panel::Repair(_))) {
            self.repair_key(key);
            return;
        }
        let Some(mut panel) = self.panel.take() else {
            return;
        };
        if key.code == KeyCode::Esc {
            return;
        }
        let (query, selected, commands) = match &mut panel {
            Panel::Repair(_) => unreachable!("repair keys handled separately"),
            Panel::ModeSelect { selected } => {
                const MODES: [tarz_core::permission::PermissionMode; 4] = [
                    tarz_core::permission::PermissionMode::ReadOnly,
                    tarz_core::permission::PermissionMode::Ask,
                    tarz_core::permission::PermissionMode::AcceptEdits,
                    tarz_core::permission::PermissionMode::FullAccess,
                ];
                match key.code {
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(3),
                    KeyCode::Enter if key.kind == KeyEventKind::Press => {
                        let chosen = MODES[*selected];
                        self.panel = None;
                        self.apply_permission_mode(chosen);
                        return;
                    }
                    _ => {}
                }
                self.panel = Some(panel);
                return;
            }
            Panel::Commands { query, selected } => (query, selected, true),
            Panel::Sessions { query, selected } => (query, selected, false),
            Panel::Info { scroll, lines, .. } => {
                let max = crate::transcript::detail_rows(lines, self.view_width)
                    .count()
                    .saturating_sub(1);
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
                        let query_str = query.clone();
                        self.panel = None;
                        self.command(index, &query_str);
                    } else {
                        self.panel = None;
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
