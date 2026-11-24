use super::{App, Confirmation};
use crate::input;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tars_core::application::ClosePolicy;

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

/// The `/status` line for the probed command sandbox, with its own vocabulary
/// and no UI reinterpretation.
pub(super) fn command_status_line(status: &tars_core::sandbox::SandboxStatus) -> String {
    format!("Commands: {status}")
}

pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("new", "Create a session with model settings"),
    ("sessions", "Find and open a saved or active session"),
    ("mode", "Show or change the permission mode"),
    (
        "rules",
        "Show active command rules or manage in-memory session rules",
    ),
    ("compact", "Replace the current context with a summary now"),
    (
        "auto_compact",
        "Show or change automatic compaction: on | off | PERCENT",
    ),
    ("summary", "Show the current context summary in full"),
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
    ("quit", "Save sessions and exit TARS"),
];

pub(crate) fn matching_commands(query: &str) -> Vec<usize> {
    let clean = query.trim_start_matches('/').trim().to_lowercase();
    let word = clean.split_whitespace().next().unwrap_or("");
    let mut matches: Vec<usize> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, (name, _))| {
            if name.contains(&clean) || (!word.is_empty() && *name == word) {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    // One command name may contain another (`compact` / `auto_compact`); the
    // exact name must win so a typed argument cannot select the longer one.
    matches.sort_by_key(|&index| COMMANDS[index].0 != word);
    matches
}

impl App {
    pub(crate) fn matching_sessions(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        let mut matches: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, entry)| {
                let matches_id = entry.id().to_lowercase().contains(&query);
                let matches_title = entry
                    .title()
                    .map(|t| t.to_lowercase().contains(&query))
                    .unwrap_or(false);
                let matches_model = entry.model().to_lowercase().contains(&query);
                (matches_id || matches_title || matches_model).then_some(i)
            })
            .collect();
        matches.sort_by(|&a, &b| {
            let entry_a = &self.entries[a];
            let entry_b = &self.entries[b];
            let a_open = entry_a.session.is_some();
            let b_open = entry_b.session.is_some();
            (!a_open, std::cmp::Reverse(entry_a.updated_at_ms()), a).cmp(&(
                !b_open,
                std::cmp::Reverse(entry_b.updated_at_ms()),
                b,
            ))
        });
        matches
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
                self.form = Some(
                    input::Form::new(
                        self.options.config.clone(),
                        self.options.profile.clone(),
                        self.options.preset.as_ref(),
                    )
                    .with_code_execution(self.options.code_execution_enabled),
                )
            }
            "sessions" => {
                self.list();
                // The picker is ordered, so start on the current session's row.
                let selected = self
                    .matching_sessions("")
                    .iter()
                    .position(|&index| index == self.selected)
                    .unwrap_or(0);
                self.panel = Some(Panel::Sessions {
                    query: String::new(),
                    selected,
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
                                .map_or(tars_core::permission::PermissionMode::Ask, |(m, _)| m);
                            let selected_idx = match current_mode {
                                tars_core::permission::PermissionMode::ReadOnly => 0,
                                tars_core::permission::PermissionMode::Ask => 1,
                                tars_core::permission::PermissionMode::AcceptEdits => 2,
                                tars_core::permission::PermissionMode::FullAccess => 3,
                            };
                            self.panel = Some(Panel::ModeSelect { selected: selected_idx });
                        } else {
                            self.notice = "Open a session first to change its permission mode".into();
                        }
                    } else {
                        self.notice = "Open a session first to change its permission mode".into();
                    }
                } else {
                    match arg.parse::<tars_core::permission::PermissionMode>() {
                        Ok(mode) => {
                            self.apply_permission_mode(mode);
                        }
                        Err(err) => {
                            self.notice = err.to_string();
                        }
                    }
                }
            }
            "rules" => {
                let clean = query.trim_start_matches('/').trim();
                let arg = clean.strip_prefix("rules").unwrap_or("").trim();
                self.rules_command(arg);
            }
            "auto_compact" => {
                let clean = query.trim_start_matches('/').trim();
                let arg = clean.strip_prefix("auto_compact").unwrap_or("").trim();
                self.auto_compact(arg);
            }
            "compact" => self.compact(),
            "summary" => self.show_summary(),
            "help" => self.panel = Some(Panel::Info {
                title: "Keyboard shortcuts".into(), scroll: 0,
                lines: ["Enter / Ctrl-S   Send input", "Ctrl-J / Alt-Enter   New line",
                    "Shift-Tab   Cycle mode (ask -> accept-edits -> read-only)",
                    "/mode [name]   Show or change the permission mode",
                    "/rules [list | remove <N> | clear]   Show active rules or manage session rules",
                    "/auto_compact [on|off|PERCENT]   Compaction switch and threshold",
                    "/compact   Summarize and replace the current context now",
                    "/summary   Show the current context summary in full",
                    "Enter / y allow · n deny (optional reason)   Confirmation panel",
                    "Esc / Ctrl-C   Interrupt work", "/ or F1   Commands; Esc returns",
                    "F2 new · F3 open · F4 next · F5 refresh", "F6 / /tools   Expand complete recorded tool results", "F7 / /repair   Preview and apply material repair", "PageUp / PageDown   Read conversation",
                    "Ctrl-Z undo · Ctrl-Y redo", "Ctrl-W close · Ctrl-Q save and quit",
                    "Ctrl-R retry save · Ctrl-E export", "Ctrl-D discard with confirmation", "",
                    "Switching views keeps work and drafts. Saved sessions restore materials, not old work records.",
                    "Compaction replaces the context with a model-written summary; older conversation loads as you scroll up.",
                    "Use /status for full configuration, save state and error details."].into_iter().map(str::to_owned).collect(),
            }),
            "tools" => self.show_tools(),
            "repair" => self.show_repair(),
            "status" => {
                let mut lines = vec![format!("Config: {}", self.options.config_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "none".into())),
                    format!("Store: {}", self.options.store.display()),
                    format!("Recovery exports: {}", self.options.recovery_dir.display())];
                if let Some(line) = self.search_status_line() {
                    lines.push(line);
                }
                if let Some(sandbox) = &self.sandbox {
                    lines.push(format!("Sandbox: {}", sandbox.detail));
                }
                if let Some(entry) = self.entries.get(self.selected) {
                    lines.extend([
                        format!("Session: {}", entry.id()),
                        format!("Model: {}", entry.model()),
                        format!(
                            "Prompt cache: {}",
                            if entry.prompt_cache() {
                                "on"
                            } else {
                                "off"
                            }
                        ),
                        format!(
                            "Code execution: {}",
                            match entry.code_execution() {
                                Some(ce) => format!(
                                    "on (timeout {}s, max_subcalls {}, max_concurrency {})",
                                    ce.timeout_secs, ce.max_subcalls, ce.max_concurrency
                                ),
                                None => "off".into(),
                            }
                        ),
                        entry.execution.clone(),
                        entry.status.clone(),
                        entry.notice.clone(),
                    ]);
                    if let Some(status) = entry
                        .session
                        .as_ref()
                        .and_then(|session| session.command_status())
                    {
                        lines.push(command_status_line(&status));
                    }
                    if let Some(capacity) = entry
                        .session
                        .as_ref()
                        .and_then(|session| session.work_capacity())
                    {
                        lines.push(format!(
                            "Works: {} / {}",
                            capacity.used, capacity.capacity
                        ));
                    }
                    if let Some(usage) = entry
                        .session
                        .as_ref()
                        .and_then(|session| session.session_usage())
                    {
                        lines.extend(tars_core::usage::session_usage_lines(&usage));
                    }
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

    fn rules_command(&mut self, arg: &str) {
        let Some(session) = self
            .entries
            .get(self.selected)
            .and_then(|entry| entry.session.clone())
        else {
            self.notice = "Open a session first to inspect or manage command rules".into();
            return;
        };

        if arg.is_empty() || arg.eq_ignore_ascii_case("list") {
            let rules = session.command_rules_snapshot();
            let report = session.open_report();
            let ws_trusted = session.workspace_rules_trusted();

            let mut lines = Vec::new();
            let user_path_label = match &self.user_rules_path {
                Some(path) => {
                    tars_core::permission::escape_untrusted_text(&path.display().to_string())
                }
                None => "(disabled via --no-config)".into(),
            };
            lines.push(format!("User rules (read-only): {user_path_label}"));
            if rules.user_rules().is_empty() {
                lines.push("  (none)".into());
            } else {
                for rule in rules.user_rules() {
                    lines.push(format!("  - {}", format_rule_entry(rule)));
                }
            }

            lines.push(String::new());
            let ws_path_label = match &report.workspace_rules.file_path {
                Some(path) => {
                    tars_core::permission::escape_untrusted_text(&path.display().to_string())
                }
                None => "(no workspace)".into(),
            };
            let trust_label = if ws_trusted {
                "trusted"
            } else {
                "UNTRUSTED (allow rules ignored; deny/ask active)"
            };
            lines.push(format!("Workspace rules [{trust_label}]: {ws_path_label}"));
            if rules.workspace_rules().is_empty() {
                lines.push("  (none)".into());
            } else {
                for rule in rules.workspace_rules() {
                    lines.push(format!("  - {}", format_rule_entry(rule)));
                }
            }
            if !ws_trusted && !report.workspace_rules.allow_rules.is_empty() {
                lines.push(format!(
                    "  ({} untrusted allow rule(s) currently ignored)",
                    report.workspace_rules.allow_rules.len()
                ));
            }

            lines.push(String::new());
            lines.push("Session rules (in-memory only; cleared on close/reopen):".into());
            if rules.session_rules().is_empty() {
                lines.push("  (none)".into());
            } else {
                for (idx, rule) in rules.session_rules().iter().enumerate() {
                    lines.push(format!("  {}. {}", idx + 1, format_rule_entry(rule)));
                }
            }

            lines.push(String::new());
            lines.push(
                "Precedence: deny > ask > allow across all layers (workspace/session cannot override user deny/ask)."
                    .into(),
            );
            lines.push(
                "/rules remove <N> removes an in-memory session rule · /rules clear clears all session rules."
                    .into(),
            );
            lines.push(
                "User and workspace rules are managed by editing their TOML files directly (TARS never rewrites global rules)."
                    .into(),
            );

            self.panel = Some(Panel::Info {
                title: "Command permission rules".into(),
                lines,
                scroll: 0,
            });
            return;
        }

        if arg.eq_ignore_ascii_case("clear") {
            match session.clear_session_rules() {
                Ok(0) => self.notice = "No in-memory session rules to clear".into(),
                Ok(count) => {
                    self.notice = format!("Cleared {count} in-memory session rule(s)");
                }
                Err(error) => {
                    self.notice = format!("Failed to clear session rules: {error}");
                }
            }
            self.dirty = true;
            return;
        }

        let mut parts = arg.split_whitespace();
        if let Some(sub) = parts.next()
            && sub.eq_ignore_ascii_case("remove")
        {
            let index_arg = parts.next();
            let extra = parts.next();
            let one_based = index_arg.and_then(|raw| raw.parse::<usize>().ok());
            match (one_based, extra) {
                (Some(n), None) if n >= 1 => match session.remove_session_rule(n - 1) {
                    Ok(true) => {
                        self.notice = format!("Removed in-memory session rule #{n}");
                    }
                    Ok(false) => {
                        self.notice = format!("No in-memory session rule #{n}");
                    }
                    Err(error) => {
                        self.notice = format!("Failed to remove session rule: {error}");
                    }
                },
                _ => {
                    self.notice = "Usage: /rules remove <N> (1-based session rule index)".into();
                }
            }
            self.dirty = true;
            return;
        }

        self.notice = "Usage: /rules [list | remove <N> | clear]".into();
        self.dirty = true;
    }
    /// `/auto_compact`: report the running settings, or change the switch or
    /// the trigger percentage. Only this session changes; shared TOML is never
    /// rewritten from here.
    fn auto_compact(&mut self, arg: &str) {
        let Some(session) = self
            .entries
            .get(self.selected)
            .and_then(|entry| entry.session.clone())
        else {
            self.notice = "Open a session first to change automatic compaction".into();
            return;
        };
        let Some(settings) = session.compaction_settings() else {
            self.notice =
                "This session does not keep compaction history; automatic compaction is unavailable"
                    .into();
            return;
        };
        let hint = if settings.context_window.is_none() {
            " · set `context_window` in the shared TOML profile or when creating the session"
        } else {
            ""
        };
        match arg {
            "" => {
                let usage = session.context_usage();
                self.panel = Some(Panel::Info {
                    title: "Automatic compaction".into(),
                    lines: auto_compact_lines(&settings, usage),
                    scroll: 0,
                });
            }
            "on" | "off" => {
                let enable = arg == "on";
                match session.set_auto_compact(enable) {
                    Ok(()) => {
                        self.notice = format!(
                            "Automatic compaction: {}{hint}",
                            if enable { "on" } else { "off" }
                        );
                    }
                    Err(error) => {
                        self.notice = format!("Failed to change automatic compaction: {error}");
                    }
                }
            }
            _ => match arg.parse::<u8>() {
                Ok(threshold) if (10..=95).contains(&threshold) => {
                    let result = session
                        .set_compact_threshold(threshold)
                        .and_then(|()| session.set_auto_compact(true));
                    match result {
                        Ok(()) => {
                            self.notice = format!("Automatic compaction: on at {threshold}%{hint}");
                        }
                        Err(error) => {
                            self.notice = format!("Failed to change automatic compaction: {error}");
                        }
                    }
                }
                _ => {
                    self.notice =
                        "Automatic compaction: use on, off, or a percentage between 10 and 95; settings unchanged"
                            .into();
                }
            },
        }
        self.dirty = true;
    }

    /// `/summary`: the current Context's summary block, full text.
    fn show_summary(&mut self) {
        let summary = self
            .entries
            .get(self.selected)
            .and_then(|entry| entry.transcript.summary.clone());
        let Some(summary) = summary else {
            self.notice = "No context summary in the current materials".into();
            return;
        };
        let lines = summary
            .lines()
            .map(|line| tars_core::permission::escape_untrusted_text(line).replace('\t', "    "))
            .collect();
        self.panel = Some(Panel::Info {
            title: "Context summary · full text".into(),
            lines,
            scroll: 0,
        });
    }

    pub(super) fn show_tools(&mut self) {
        const TOOLS_PANEL_TITLE: &str = "Recorded tool materials · viewing never executes";
        let Some(entry) = self.entries.get(self.selected) else {
            self.panel = Some(Panel::Info {
                title: TOOLS_PANEL_TITLE.into(),
                lines: vec![
                    "No committed tool materials. Returned pending results may not be committed."
                        .into(),
                ],
                scroll: 0,
            });
            return;
        };
        let has_spilled = entry.transcript.tool_artifacts.iter().any(Option::is_some);
        if !has_spilled {
            let mut lines = entry.transcript.tool_details.clone();
            if lines.is_empty() {
                lines.push(
                    "No committed tool materials. Returned pending results may not be committed."
                        .into(),
                );
            }
            self.panel = Some(Panel::Info {
                title: TOOLS_PANEL_TITLE.into(),
                lines,
                scroll: 0,
            });
            return;
        }
        let Some(session) = entry.session.clone() else {
            let lines = entry
                .transcript
                .tool_details
                .iter()
                .zip(entry.transcript.tool_artifacts.iter())
                .map(|(detail, artifact)| match artifact {
                    Some(spilled) => format!(
                        "Result for {} · {:?} · {}\nFull artifact cannot be read (session is not open); showing model-facing preview:\n{}",
                        spilled.call_block_id.0,
                        spilled.status,
                        crate::transcript::display(&spilled.artifact_id),
                        spilled.preview_json
                    ),
                    None => detail.clone(),
                })
                .collect();
            self.panel = Some(Panel::Info {
                title: TOOLS_PANEL_TITLE.into(),
                lines,
                scroll: 0,
            });
            return;
        };

        let mut initial_lines = Vec::with_capacity(entry.transcript.tool_details.len());
        for (detail, artifact) in entry
            .transcript
            .tool_details
            .iter()
            .zip(entry.transcript.tool_artifacts.iter())
        {
            match artifact {
                Some(spilled) => initial_lines.push(format!(
                    "Result for {} · {:?} · {}\nLoading full output from {}...\n{}",
                    spilled.call_block_id.0,
                    spilled.status,
                    crate::transcript::display(&spilled.artifact_id),
                    crate::transcript::display(&spilled.artifact_id),
                    spilled.preview_json
                )),
                None => initial_lines.push(detail.clone()),
            }
        }
        self.panel = Some(Panel::Info {
            title: TOOLS_PANEL_TITLE.into(),
            lines: initial_lines,
            scroll: 0,
        });

        let key = entry.key.clone();
        let version = entry.material_version.clone();
        let transcript = entry.transcript.clone();
        self.spawn_job(Some(key.clone()), super::jobs::JobKind::ToolArtifacts, async move {
            let result = super::jobs::blocking(move || {
                let mut lines = Vec::with_capacity(transcript.tool_details.len());
                let mut reads = 0usize;
                for (detail, artifact) in transcript
                    .tool_details
                    .iter()
                    .zip(transcript.tool_artifacts.iter())
                {
                    match artifact {
                        Some(spilled) => {
                            reads += 1;
                            let model_note = if spilled.artifact_id.starts_with("artifact:") {
                                "Model sees spilled preview only"
                            } else {
                                "Model sees cleared preview only"
                            };
                            match session.read_artifact(&spilled.artifact_id) {
                                Ok(full_text) => lines.push(format!(
                                    "Result for {} · {:?} · {}\n{model_note} · full output from {} ({} bytes):\n{}\n\nModel-facing preview:\n{}",
                                    spilled.call_block_id.0,
                                    spilled.status,
                                    crate::transcript::display(&spilled.artifact_id),
                                    crate::transcript::display(&spilled.artifact_id),
                                    spilled.size_bytes,
                                    crate::transcript::display(&full_text),
                                    spilled.preview_json,
                                )),
                                Err(error) => lines.push(format!(
                                    "Result for {} · {:?} · {}\nFull artifact cannot be read ({}); showing model-facing preview:\n{}",
                                    spilled.call_block_id.0,
                                    spilled.status,
                                    crate::transcript::display(&spilled.artifact_id),
                                    crate::transcript::display(&error.to_string()),
                                    spilled.preview_json,
                                )),
                            }
                        }
                        None => lines.push(detail.clone()),
                    }
                }
                (lines, reads)
            })
            .await;
            let (lines, reads) = match result {
                Ok(pair) => pair,
                Err(error) => (vec![format!("Failed to load tool artifacts: {error}")], 0),
            };
            super::jobs::Message::ToolArtifactsLoaded(key, version, lines, reads)
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
                const MODES: [tars_core::permission::PermissionMode; 4] = [
                    tars_core::permission::PermissionMode::ReadOnly,
                    tars_core::permission::PermissionMode::Ask,
                    tars_core::permission::PermissionMode::AcceptEdits,
                    tars_core::permission::PermissionMode::FullAccess,
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
                        self.refresh_size_flags();
                        self.open();
                        self.update_approval_guard();
                        self.update_workspace_trust_guard();
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

fn format_rule_entry(rule: &tars_core::permission::CommandRule) -> String {
    let decision = match rule.decision {
        tars_core::permission::RuleDecision::Allow => "allow",
        tars_core::permission::RuleDecision::Ask => "ask",
        tars_core::permission::RuleDecision::Deny => "deny",
    };
    let prefix = tars_core::permission::escape_untrusted_text(&rule.prefix.join(" "));
    if rule.outside_sandbox {
        format!("{decision}: {prefix} [outside_sandbox]")
    } else {
        format!("{decision}: {prefix}")
    }
}

/// Lines for the `/auto_compact` report: running switch, threshold, frozen
/// window and the current estimate, plus how to make automatic compaction
/// possible when the session has no window.
fn auto_compact_lines(
    settings: &tars_core::application::CompactionSettings,
    usage: Option<tars_core::application::ContextUsage>,
) -> Vec<String> {
    let mut lines = vec![
        format!(
            "Switch: {}",
            if settings.auto_compact { "on" } else { "off" }
        ),
        format!(
            "Threshold: {}% of the context window",
            settings.compact_threshold
        ),
        match settings.context_window {
            Some(window) => format!(
                "Context window: {window} tokens ({})",
                crate::app::history::window_label(window)
            ),
            None => "Context window: unknown".into(),
        },
        match usage {
            Some(usage) => format!(
                "Current estimate: {} tokens ({})",
                usage.estimate_tokens,
                crate::app::history::estimate_label(usage.estimate_tokens)
            ),
            None => "Current estimate: unavailable".into(),
        },
    ];
    if settings.context_window.is_none() {
        lines.push(String::new());
        lines.push("Automatic compaction does not run without a context window.".into());
        lines.push(
            "Set `context_window` in the shared TOML profile or when creating the session.".into(),
        );
    }
    lines.push(String::new());
    lines.push("/auto_compact on | off | PERCENT (10-95)".into());
    lines
}
