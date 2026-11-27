use super::theme::{ACCENT, MUTED};
use crate::{
    app::{
        App, COMMANDS, Panel, matching_commands,
        provider::{
            CredentialStoreCache, ProviderFieldKind, ProviderWizardState, ProviderWizardStep,
        },
    },
    input::{FIELD_NAMES, Form},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

pub(super) fn commands_popup(
    frame: &mut Frame<'_>,
    query: &str,
    selected: usize,
    show_header: bool,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let list_area = if show_header && area.height >= 2 {
        let title = if area.width < 66 {
            " Commands "
        } else {
            " Commands · ↑↓ select · Tab complete · Enter run · Esc close "
        };
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(MUTED))
            .title(Span::styled(title, Style::default().fg(MUTED)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    let matches = matching_commands(query);
    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching commands · Esc closes")
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
            list_area,
        );
        return;
    }
    let max_item_width = usize::from(list_area.width).saturating_sub(2);
    let items: Vec<_> = matches
        .iter()
        .map(|&index| {
            let (name, description) = COMMANDS[index];
            let label = if name.len() > 12 {
                format!("/{name}  ")
            } else {
                format!("/{name:<13}")
            };
            let spans = super::surface::truncate_spans_display_width(
                vec![
                    Span::raw(label),
                    Span::styled(description, Style::default().fg(MUTED)),
                ],
                max_item_width,
            );
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        list_area,
        &mut state,
    );
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &App, panel: &Panel, area: Rect) {
    frame.render_widget(Clear, area);
    let (query, selected) = match panel {
        Panel::ModeSelect { selected } => {
            let regions = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);
            frame.render_widget(
                Paragraph::new("Permission Mode")
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                regions[0],
            );
            const MODES: [(&str, &str); 4] = [
                (
                    "read-only",
                    "Execute read-only tools; reject writes without confirmation",
                ),
                (
                    "ask",
                    "Ask for confirmation before write tools or prompted MCP tools (default)",
                ),
                (
                    "accept-edits",
                    "Workspace write tools execute without asking; ask for prompted tools",
                ),
                ("full-access", "All tools execute without asking"),
            ];
            let current_mode = app
                .entries
                .get(app.selected)
                .and_then(|e| e.session.as_ref())
                .and_then(|s| s.permission_mode())
                .map(|(m, _)| m.as_str());

            let items: Vec<_> = MODES
                .iter()
                .map(|&(name, desc)| {
                    let is_current = current_mode == Some(name);
                    let title = if is_current {
                        format!("{name}  (current)")
                    } else {
                        name.to_string()
                    };
                    ListItem::new(vec![
                        Line::from(title),
                        Line::styled(format!("  {desc}"), Style::default().fg(MUTED)),
                    ])
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                    .highlight_symbol("❯ "),
                regions[1],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new("↑↓ select · Enter apply · Esc return")
                    .style(Style::default().fg(MUTED)),
                regions[2],
            );
            return;
        }
        Panel::Agents { selected } => {
            let entry = app.entries.get(app.selected);
            let agents = entry
                .and_then(|e| e.session.as_ref())
                .map(|s| s.list_agents())
                .unwrap_or_default();
            let regions = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(area);
            frame.render_widget(
                Paragraph::new(format!("Session agents ({})", agents.len()))
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                regions[0],
            );
            if agents.is_empty() {
                frame.render_widget(
                    Paragraph::new("No active agents in this session.")
                        .wrap(Wrap { trim: false })
                        .style(Style::default().fg(MUTED)),
                    regions[1],
                );
            } else {
                let items: Vec<_> = agents
                    .iter()
                    .map(|agent| {
                        let is_entry = entry.is_some_and(|e| agent.agent_id == e.id());
                        let is_viewing = entry.is_some_and(|e| match &e.viewed_agent_id {
                            None => agent.agent_id == e.id(),
                            Some(id) => *id == agent.agent_id,
                        });
                        let escaped_name =
                            tars_core::permission::escape_untrusted_text(&agent.name);
                        let name_label = if is_entry && agent.name == "main" {
                            "main (entry)".to_owned()
                        } else if is_entry {
                            format!("{escaped_name} (entry)")
                        } else {
                            escaped_name
                        };
                        let preset_part = agent
                            .preset_name
                            .as_deref()
                            .map(|preset| {
                                format!(
                                    " · preset {}",
                                    tars_core::permission::escape_untrusted_text(preset)
                                )
                            })
                            .unwrap_or_default();
                        let viewing_part = if is_viewing { " · (viewing)" } else { "" };
                        let status_part = if agent.active_operation_id.is_some() {
                            "running"
                        } else {
                            "idle"
                        };
                        let pending_part = if agent.pending_inbox_count > 0 {
                            format!(" · pending {}", agent.pending_inbox_count)
                        } else {
                            String::new()
                        };
                        let short_id = agent.agent_id.get(..8).unwrap_or(&agent.agent_id);
                        let detail_style = if agent.active_operation_id.is_some() {
                            Style::default().fg(Color::Cyan)
                        } else if agent.pending_inbox_count > 0 {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(MUTED)
                        };
                        ListItem::new(vec![
                            Line::from(format!("{name_label}{preset_part}{viewing_part}")),
                            Line::styled(
                                format!(
                                    "  {short_id} · {} · {status_part}{pending_part}",
                                    agent.interaction
                                ),
                                detail_style,
                            ),
                        ])
                    })
                    .collect();
                let mut state =
                    ListState::default().with_selected(Some((*selected).min(items.len() - 1)));
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
                        .highlight_symbol("❯ "),
                    regions[1],
                    &mut state,
                );
            }
            frame.render_widget(
                Paragraph::new("↑↓ select · Enter switch view · x cancel running · Esc return")
                    .style(Style::default().fg(MUTED)),
                regions[2],
            );
            return;
        }
        Panel::Repair(panel) => {
            let regions = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(area);
            frame.render_widget(
                Paragraph::new("Continue after interruption · material repair")
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                regions[0],
            );
            let lines: Vec<_> = crate::transcript::detail_rows(&panel.lines, regions[1].width)
                .skip(panel.scroll)
                .take(usize::from(regions[1].height))
                .map(|line| {
                    Line::from(
                        tars_core::permission::escape_untrusted_text(line).replace('\t', "    "),
                    )
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), regions[1]);
            let hint = if panel.busy {
                "Waiting for local work / saving…\nEsc closes this view; no work is resumed."
            } else if panel.plan.is_some() {
                "Enter apply this preview · r remove preview · a original-results preview\nF7 refresh · Ctrl-R retry save · Esc keep materials · F4 next session"
            } else {
                "r preview removal · a preview original results · F7 refresh\nCtrl-R retry save · Esc keep materials · F4 next session"
            };
            let hint_style = if app.is_repair_guarded() {
                Style::default().fg(MUTED).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(MUTED).remove_modifier(Modifier::DIM)
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .wrap(Wrap { trim: false })
                    .style(hint_style),
                regions[2],
            );
            return;
        }
        Panel::Commands { .. } => {
            unreachable!("Panel::Commands is rendered as a floating popup in surface::draw")
        }
        Panel::Sessions { query, selected } => (query, *selected),
        Panel::Models { query, selected } => {
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        "Models  /",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(query),
                ])),
                rows[0],
            );
            let profiles = app.matching_model_profiles(query);
            let mut items: Vec<ListItem<'_>> = profiles
                .iter()
                .map(|profile| {
                    let is_active = match app.options.profile.as_deref() {
                        Some(active_name) => active_name == profile.name,
                        None => app
                            .entries
                            .get(app.selected)
                            .map(|e| e.model() == profile.model)
                            .unwrap_or_else(|| {
                                app.options.preset.as_ref().is_some_and(|p| {
                                    p.model == profile.model && p.provider == profile.provider
                                })
                            }),
                    };
                    let escaped_name = crate::transcript::display_single_line(&profile.name);
                    let escaped_model = crate::transcript::display_single_line(&profile.model);
                    let title = if is_active {
                        format!("{escaped_name}  [active]")
                    } else {
                        escaped_name
                    };
                    ListItem::new(vec![
                        Line::from(title),
                        Line::styled(
                            format!("  {} · {escaped_model}", profile.provider.as_str()),
                            Style::default().fg(MUTED),
                        ),
                    ])
                })
                .collect();
            items.push(ListItem::new(vec![
                Line::from("Custom settings (/new)..."),
                Line::styled(
                    "  Open full session creation form",
                    Style::default().fg(MUTED),
                ),
            ]));
            items.push(ListItem::new(vec![
                Line::from("+ Add model provider (/provider)"),
                Line::styled(
                    "  Configure a provider preset and API key",
                    Style::default().fg(MUTED),
                ),
            ]));
            let mut state =
                ListState::default().with_selected(Some((*selected).min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("❯ ")
                    .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                rows[1],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(
                    "↑↓ select · type to filter · Enter switch (new session) · Esc back",
                )
                .style(Style::default().fg(MUTED)),
                rows[2],
            );
            return;
        }
        Panel::Provider(wizard) => {
            provider_wizard(frame, app, wizard, area);
            return;
        }
        Panel::Info {
            title,
            kind,
            lines,
            scroll,
        } => {
            let regions = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
            frame.render_widget(
                Paragraph::new(title.as_str())
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                regions[0],
            );
            let lines: Vec<_> = crate::transcript::info_panel_lines(kind, lines, regions[1].width)
                .into_iter()
                .skip(*scroll)
                .take(usize::from(regions[1].height))
                .collect();
            frame.render_widget(Paragraph::new(lines), regions[1]);
            return;
        }
    };
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "Sessions  /",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(query),
        ])),
        rows[0],
    );
    let items: Vec<_> = app
        .matching_sessions(query)
        .iter()
        .map(|&index| {
            let entry = &app.entries[index];
            let waiting = if !entry.approval_queue.is_empty() {
                " · waiting for confirmation"
            } else {
                ""
            };
            let model = entry.model();
            let time_str = tars_core::format_timestamp_utc(entry.updated_at_ms());
            let first_line = match entry.title() {
                Some(title) => format!(
                    "{}  ·  {time_str}",
                    tars_core::permission::escape_untrusted_text(&title)
                ),
                None => format!("{model}  ·  {time_str}"),
            };
            ListItem::new(vec![
                Line::from(first_line),
                Line::styled(
                    format!("{model}{waiting}"),
                    Style::default().fg(if waiting.is_empty() {
                        MUTED
                    } else {
                        Color::Yellow
                    }),
                ),
            ])
        })
        .collect();
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching sessions. /new creates one.")
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED)),
            rows[1],
        );
    } else {
        let mut state = ListState::default().with_selected(Some(selected.min(items.len() - 1)));
        frame.render_stateful_widget(
            List::new(items)
                .highlight_symbol("❯ ")
                .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
            rows[1],
            &mut state,
        );
    }
    frame.render_widget(
        Paragraph::new("↑↓ select · Enter open · Ctrl-D/Del delete · Esc back")
            .style(Style::default().fg(MUTED)),
        rows[2],
    );
}

pub(super) fn form(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let form = app.form.as_ref().unwrap();
    let command = form.command_summary();
    let command_rows = u16::from(command.is_some());
    let compact = area.height < 2 + 3 * form.fields.len() as u16 + 4 + command_rows;
    let shown: Vec<_> = if compact {
        vec![form.selected]
    } else {
        (0..form.fields.len()).collect()
    };
    let mut sizes = vec![Constraint::Length(2)];
    sizes.extend(std::iter::repeat_n(Constraint::Length(3), shown.len()));
    if command.is_some() {
        sizes.push(Constraint::Length(1));
    }
    sizes.push(Constraint::Min(1));
    sizes.push(Constraint::Length(if compact { 2 } else { 3 }));
    let rows = Layout::vertical(sizes).split(area);
    frame.render_widget(
        Paragraph::new(format!(
            "New session  ·  {} / {}",
            form.selected + 1,
            form.fields.len()
        ))
        .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    for (row, &index) in shown.iter().enumerate() {
        let field = &form.fields[index];
        let raw_value = field.lines().join("");
        let title = if index == 3 {
            Line::from(vec![
                Span::raw(FIELD_NAMES[index]),
                credential_status_badge(&raw_value, &app.credential_store_cache),
            ])
        } else {
            Line::from(FIELD_NAMES[index])
        };
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .title(title)
            .border_style(Style::default().fg(if index == form.selected {
                ACCENT
            } else {
                MUTED
            }));
        let inner = block.inner(rows[row + 1]);
        frame.render_widget(block, rows[row + 1]);
        if Form::is_selector_field(index) {
            let selector_text = format!("◀ {} ▶", raw_value.trim());
            let style = if index == form.selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            frame.render_widget(Paragraph::new(selector_text).style(style), inner);
        } else if index == form.selected {
            frame.render_widget(field, inner);
        } else {
            frame.render_widget(Paragraph::new(raw_value), inner);
        }
    }
    if let Some(command) = &command {
        frame.render_widget(
            Paragraph::new(command.as_str()).style(Style::default().fg(MUTED)),
            rows[shown.len() + 1],
        );
    }
    let error_row = shown.len() + 1 + usize::from(command.is_some());
    frame.render_widget(
        Paragraph::new(form.error.as_str())
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        rows[error_row],
    );
    let hint = if app.confirmation.is_some() {
        "Discard this form and unsent drafts, save sessions and quit? y / n"
    } else {
        "Tab / Shift-Tab field · ←/→/Space choose · Ctrl-S create · Esc back\nSettings apply to this new session only."
    };
    frame.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if app.confirmation.is_some() {
                Color::Yellow
            } else {
                MUTED
            })),
        rows[error_row + 1],
    );
}

pub(super) fn folder_trust(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let wizard = app.folder_trust.as_ref().unwrap();
    let compact = area.height < 16 || area.width < 46;
    let short_path = super::surface::format_workspace_label(&wizard.workspace_root, usize::MAX);
    let full_path =
        tars_core::permission::escape_untrusted_text(&wizard.workspace_root.display().to_string());

    let mut lines = Vec::new();
    lines.push(Line::styled(
        "Do you trust the files in this folder?",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));

    if compact {
        lines.push(Line::from(short_path));
    } else {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Folder: ", Style::default().fg(MUTED)),
            Span::raw(short_path.clone()),
        ]));
        if short_path != full_path {
            lines.push(Line::from(vec![
                Span::styled("Path:   ", Style::default().fg(MUTED)),
                Span::raw(full_path),
            ]));
        }
        lines.push(Line::default());
        lines.push(Line::from("Trusting this folder allows TARS to:"));
        lines.push(Line::from(
            "  • Read and edit files in this workspace (subject to permission mode)",
        ));
        lines.push(Line::from(
            "  • Run commands in this workspace (subject to permission mode)",
        ));
        lines.push(Line::from(
            "  • Read workspace instructions and skills (AGENTS.md / skills)",
        ));
        lines.push(Line::default());
        lines.push(Line::styled(
            "Note: .tars/permissions.toml and .tars/agents/*.md remain independently verified by content hash and confirmation panels.",
            Style::default().fg(MUTED),
        ));
        lines.push(Line::default());
    }

    let yes_label = if wizard.saving {
        "1. Yes, trust this folder (saving…)"
    } else {
        "1. Yes, trust this folder"
    };
    let options = [(0usize, yes_label), (1usize, "2. No, exit")];
    for (idx, label) in options {
        let active = wizard.selected == idx;
        let prefix = if active { "❯ " } else { "  " };
        let style = if active {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!("{prefix}{label}"), style));
    }

    if !wizard.error.is_empty() {
        if !compact {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            tars_core::permission::escape_untrusted_text(&wizard.error),
            Style::default().fg(Color::Yellow),
        ));
    }

    let hint = if compact {
        "↑↓/jk · Enter · y/n/Ctrl-C"
    } else {
        "↑↓ / j k select · Enter confirm · y trust · n / Esc / Ctrl-C exit"
    };
    let hint_style = if app.is_folder_trust_guarded() {
        Style::default().fg(MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(MUTED).remove_modifier(Modifier::DIM)
    };
    let hint_widget = Paragraph::new(hint)
        .wrap(Wrap { trim: false })
        .style(hint_style);
    use unicode_width::UnicodeWidthStr;
    let hint_lines = hint.width().div_ceil(usize::from(area.width.max(1)));
    let hint_height = u16::try_from(hint_lines)
        .unwrap_or(1)
        .clamp(1, area.height.saturating_sub(1).max(1));

    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(hint_height)]).split(area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[0]);
    frame.render_widget(hint_widget, rows[1]);
}

pub(super) fn confirm(frame: &mut Frame<'_>, confirmation: &crate::app::Confirmation, area: Rect) {
    use crate::app::Confirmation;
    let (title, text): (&str, String) = match confirmation {
        Confirmation::Quit {
            has_drafts,
            has_queue,
        } => {
            let body = match (*has_drafts, *has_queue) {
                (true, true) => {
                    "Unsubmitted drafts and queued messages will be lost. Sessions will be saved before exiting."
                }
                (false, true) => {
                    "Queued messages will be lost. Sessions will be saved before exiting."
                }
                _ => "Unsubmitted drafts will be lost. Sessions will be saved before exiting.",
            };
            ("Save and quit?", body.into())
        }
        Confirmation::Close {
            has_draft,
            has_queue,
        } => {
            let body = match (*has_draft, *has_queue) {
                (true, true) => {
                    "This draft and queued messages will be lost. The session will be saved before closing."
                }
                (false, true) => {
                    "Queued messages for this session will be lost. The session will be saved before closing."
                }
                _ => "This draft will be lost. The session will be saved before closing.",
            };
            ("Save and close?", body.into())
        }
        Confirmation::Discard => (
            "Discard local content?",
            "Unsaved materials and draft will be lost. Existing commits are NOT rolled back."
                .into(),
        ),
        Confirmation::DeleteSession { label, .. } => (
            "Delete session?",
            format!(
                "Session {label}: this permanently deletes the saved session, its history, and spilled artifacts."
            ),
        ),
    };
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(title).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        rows[0],
    );
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), rows[1]);
    let hint = match confirmation {
        Confirmation::Quit { .. } => "y or Ctrl-C quit · n / Esc cancel",
        Confirmation::Close { .. } | Confirmation::Discard | Confirmation::DeleteSession { .. } => {
            "Confirm? y / n · Esc / Ctrl-C cancel"
        }
    };
    frame.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(ACCENT)),
        rows[2],
    );
}

pub(super) fn approval(frame: &mut Frame<'_>, app: &App, entry: &crate::app::Entry, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(req) = entry.approval_queue.front() else {
        return;
    };
    let title_line = match &req.subject {
        tars_core::permission::ApprovalSubject::FileWrite { path, action, .. } => {
            let action_str = match action {
                tars_core::permission::WriteAction::Created => "create",
                tars_core::permission::WriteAction::Updated => "update",
            };
            format!(
                "{} wants to {} {} (permission mode: {})",
                tars_core::permission::escape_untrusted_text(&req.tool_name),
                action_str,
                tars_core::permission::escape_untrusted_text(path),
                req.mode,
            )
        }
        tars_core::permission::ApprovalSubject::Command {
            target: tars_core::permission::CommandTarget::Outside { .. },
            ..
        } => {
            format!(
                "{} wants to run a command outside the sandbox (permission mode: {})",
                tars_core::permission::escape_untrusted_text(&req.tool_name),
                req.mode,
            )
        }
        tars_core::permission::ApprovalSubject::Command { .. } => {
            format!(
                "{} wants to run a command (permission mode: {})",
                tars_core::permission::escape_untrusted_text(&req.tool_name),
                req.mode,
            )
        }
        tars_core::permission::ApprovalSubject::McpCall { server, tool, .. } => {
            format!(
                "{} wants to call MCP tool {}/{} (permission mode: {})",
                tars_core::permission::escape_untrusted_text(&req.tool_name),
                tars_core::permission::escape_untrusted_text(server),
                tars_core::permission::escape_untrusted_text(tool),
                req.mode,
            )
        }
    };

    let agent_prefix = super::surface::approval_agent_prefix(&req.origin, entry.id());
    let title_line = format!("{agent_prefix}{title_line}");

    let current_mode = entry
        .session
        .as_ref()
        .and_then(|s| s.permission_mode())
        .map(|(m, _)| m);

    let mut header_lines = vec![Line::styled(
        title_line,
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if let Some(m) = current_mode.filter(|m| *m != req.mode) {
        header_lines.push(Line::styled(
            format!("Permission mode changed to {m}; this pending call still requires a response."),
            Style::default().fg(Color::Yellow),
        ));
    }
    if entry.approval_queue.len() > 1 {
        let msg = match &req.subject {
            tars_core::permission::ApprovalSubject::FileWrite { .. } => {
                format!(
                    "({} more pending write confirmation(s) in queue)",
                    entry.approval_queue.len() - 1
                )
            }
            tars_core::permission::ApprovalSubject::Command { .. }
            | tars_core::permission::ApprovalSubject::McpCall { .. } => {
                format!(
                    "({} more pending confirmation(s) in queue)",
                    entry.approval_queue.len() - 1
                )
            }
        };
        header_lines.push(Line::styled(msg, Style::default().fg(MUTED)));
    }

    let header_len = header_lines.len() as u16;
    let reason_height = if entry.approval_reason.is_some() {
        2
    } else {
        0
    };
    let regions = Layout::vertical([
        Constraint::Length(header_len + 1),
        Constraint::Min(1),
        Constraint::Length(reason_height),
        Constraint::Length(3),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_lines), regions[0]);

    let body_height = usize::from(regions[1].height.saturating_sub(2));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));

    match &req.subject {
        tars_core::permission::ApprovalSubject::FileWrite { diff, .. } => {
            let escaped_diff = tars_core::permission::escape_untrusted_text(diff);
            let diff_lines: Vec<&str> = escaped_diff.lines().collect();
            let scroll = entry
                .approval_scroll
                .min(diff_lines.len().saturating_sub(body_height));
            let visible_diff: Vec<Line> = diff_lines
                .iter()
                .skip(scroll)
                .take(body_height)
                .map(|&line| {
                    let style = if line.starts_with('+') && !line.starts_with("+++") {
                        Style::default().fg(Color::Green)
                    } else if line.starts_with('-') && !line.starts_with("---") {
                        Style::default().fg(Color::Red)
                    } else if line.starts_with("@@") {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    };
                    Line::styled(line, style)
                })
                .collect();
            frame.render_widget(Paragraph::new(visible_diff).block(diff_block()), regions[1]);
        }
        tars_core::permission::ApprovalSubject::Command { .. } => {
            let body =
                tars_core::permission::command_body(&req.subject).expect("command approval body");
            let scroll = entry
                .approval_scroll
                .min(body.len().saturating_sub(body_height));
            let visible: Vec<Line> = body
                .iter()
                .skip(scroll)
                .take(body_height)
                .map(|line| {
                    let style = if line.starts_with("NOT in the sandbox")
                        || (line.starts_with("Rule ") && line.contains("outside_sandbox = true"))
                    {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else if line.starts_with("Rule prefix") {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else if line.starts_with("Rule note:") {
                        Style::default().fg(MUTED)
                    } else {
                        Style::default()
                    };
                    Line::styled(line.as_str(), style)
                })
                .collect();
            frame.render_widget(Paragraph::new(visible).block(block), regions[1]);
        }
        tars_core::permission::ApprovalSubject::McpCall { arguments, .. } => {
            let escaped_args = tars_core::permission::escape_untrusted_text(arguments);
            let arg_lines: Vec<&str> = escaped_args.lines().collect();
            let scroll = entry
                .approval_scroll
                .min(arg_lines.len().saturating_sub(body_height));
            let visible_args: Vec<Line> = arg_lines
                .iter()
                .skip(scroll)
                .take(body_height)
                .map(|&line| Line::from(line))
                .collect();
            frame.render_widget(Paragraph::new(visible_args).block(block), regions[1]);
        }
    }

    if let Some(reason) = &entry.approval_reason {
        frame.render_widget(
            Paragraph::new(vec![Line::styled(
                format!("Reason for the model (optional, Enter to skip): {reason}"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )]),
            regions[2],
        );
    }

    let hint = if entry.approval_reason.is_some() {
        "Enter confirm deny · Esc cancel"
    } else {
        match &req.subject {
            tars_core::permission::ApprovalSubject::Command {
                suggested_prefixes,
                can_save_workspace_rule,
                ..
            } if !suggested_prefixes.is_empty() && *can_save_workspace_rule => {
                "1/y/Enter allow once · 2/s allow for session · 3/w always in workspace · n deny\nEsc cancel turn · Shift-Tab mode · ↑↓ PgUp PgDn scroll"
            }
            tars_core::permission::ApprovalSubject::Command {
                suggested_prefixes, ..
            } if !suggested_prefixes.is_empty() => {
                "1/y/Enter allow once · 2/s allow for session · n deny\nEsc cancel turn · Shift-Tab mode · ↑↓ PgUp PgDn scroll"
            }
            _ => {
                "Enter / y allow · n deny · Esc cancel turn · Shift-Tab mode · ↑↓ PgUp PgDn scroll"
            }
        }
    };
    let hint_style = if app.is_approval_guarded() {
        Style::default().fg(MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(MUTED).remove_modifier(Modifier::DIM)
    };
    frame.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(hint_style),
        regions[3],
    );
}

fn diff_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
}

pub(super) fn workspace_rules_trust(
    frame: &mut Frame<'_>,
    app: &App,
    entry: &crate::app::Entry,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let Some(prompt) = entry.workspace_rules_trust.as_ref() else {
        return;
    };
    let mut header_lines = vec![
        Line::styled(
            "Trust workspace command allow rules?",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "File: {}",
                tars_core::permission::escape_untrusted_text(
                    &prompt.file_path.display().to_string()
                )
            ),
            Style::default().fg(MUTED),
        ),
    ];
    if prompt.has_outside_sandbox {
        header_lines.push(Line::styled(
            "WARNING: [OUTSIDE SANDBOX] rules allow execution outside the sandbox without prompting.",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    header_lines.push(Line::from(
        "Pending allow rules (currently ignored; deny/ask rules are already active):",
    ));
    render_workspace_trust_prompt(
        frame,
        app,
        area,
        header_lines,
        &prompt.body_lines,
        prompt.scroll,
    );
}

pub(super) fn workspace_agents_trust(
    frame: &mut Frame<'_>,
    app: &App,
    entry: &crate::app::Entry,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let Some(prompt) = entry.workspace_agents_trust.as_ref() else {
        return;
    };
    let header_lines = vec![
        Line::styled(
            "Trust workspace agent presets?",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "Directory: {}",
                tars_core::permission::escape_untrusted_text(
                    &prompt.agents_dir.display().to_string()
                )
            ),
            Style::default().fg(MUTED),
        ),
        Line::from("Pending workspace presets from .tars/agents/*.md (currently excluded):"),
    ];
    render_workspace_trust_prompt(
        frame,
        app,
        area,
        header_lines,
        &prompt.body_lines,
        prompt.scroll,
    );
}

fn render_workspace_trust_prompt(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    header_lines: Vec<Line<'_>>,
    body_lines: &[crate::app::WorkspaceTrustLine],
    scroll: usize,
) {
    use crate::app::WorkspaceTrustLineKind;

    let header_len = header_lines.len() as u16;
    let regions = Layout::vertical([
        Constraint::Length(header_len + 1),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_lines), regions[0]);

    let body_height = usize::from(regions[1].height.saturating_sub(2));
    let scroll = scroll.min(body_lines.len().saturating_sub(body_height));
    let visible: Vec<Line> = body_lines
        .iter()
        .skip(scroll)
        .take(body_height)
        .map(|line| {
            let style = match line.kind {
                WorkspaceTrustLineKind::Plain => Style::default(),
                WorkspaceTrustLineKind::Rule => Style::default().fg(Color::Cyan),
                WorkspaceTrustLineKind::RuleOutsideSandbox => {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                }
                WorkspaceTrustLineKind::Heading => Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                WorkspaceTrustLineKind::DiffAdd => Style::default().fg(Color::Green),
                WorkspaceTrustLineKind::DiffRemove => Style::default().fg(Color::Red),
                WorkspaceTrustLineKind::DiffHunk => Style::default().fg(Color::Cyan),
            };
            Line::styled(line.text.as_str(), style)
        })
        .collect();
    frame.render_widget(Paragraph::new(visible).block(diff_block()), regions[1]);

    let hint = "Enter / y trust · n / Esc / Ctrl-C decline · ↑↓ PgUp PgDn scroll";
    let hint_style = if app.is_workspace_trust_guarded() {
        Style::default().fg(MUTED).add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(MUTED).remove_modifier(Modifier::DIM)
    };
    frame.render_widget(Paragraph::new(hint).style(hint_style), regions[2]);
}

fn valid_env_var_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn credential_status_badge(raw: &str, cache: &CredentialStoreCache) -> Span<'static> {
    let trimmed = raw.trim();
    if let Some(env_var) = trimmed.strip_prefix("env:") {
        let var = env_var.trim();
        let is_set =
            valid_env_var_name(var) && std::env::var_os(var).is_some_and(|val| !val.is_empty());
        return if is_set {
            Span::styled("  ✓ [env set]", Style::default().fg(Color::Green))
        } else {
            Span::styled("  ✗ [env unset]", Style::default().fg(Color::Yellow))
        };
    }
    if !tars_core::definition::valid_credential_name(trimmed) {
        return Span::styled("  ✗ [invalid]", Style::default().fg(Color::Yellow));
    }
    match cache {
        CredentialStoreCache::Error => {
            Span::styled("  ✗ [store error]", Style::default().fg(Color::Yellow))
        }
        CredentialStoreCache::Loaded(names) if names.iter().any(|n| n == trimmed) => {
            Span::styled("  ✓ [stored]", Style::default().fg(Color::Green))
        }
        CredentialStoreCache::Loaded(_) | CredentialStoreCache::Unavailable => {
            Span::styled("  ✗ [missing]", Style::default().fg(Color::Yellow))
        }
        CredentialStoreCache::Unknown => Span::styled("  [checking…]", Style::default().fg(MUTED)),
    }
}

fn provider_wizard(frame: &mut Frame<'_>, app: &App, wizard: &ProviderWizardState, area: Rect) {
    let compact = area.height < 16 || area.width < 46;
    match wizard.step {
        ProviderWizardStep::SelectPreset => {
            let rows = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
            frame.render_widget(
                Paragraph::new("Add model provider (1/2 · select preset)")
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                rows[0],
            );
            let profiles = app.options.config.model_profiles();
            let mut items: Vec<ListItem<'_>> = tars_core::config::PROVIDER_PRESETS
                .iter()
                .map(|preset| {
                    let configured = profiles.contains_key(preset.id)
                        || app.credential_store_cache.contains(preset.id);
                    let badge = if configured { "  [configured]" } else { "" };
                    let endpoint_note = preset.endpoint.unwrap_or("protocol default");
                    if compact {
                        ListItem::new(Line::from(format!(
                            "{} ({}){badge}",
                            preset.label,
                            preset.provider.as_str()
                        )))
                    } else {
                        ListItem::new(vec![
                            Line::from(format!(
                                "{}  ({}){badge}",
                                preset.label,
                                preset.provider.as_str()
                            )),
                            Line::styled(
                                format!("  model {} · {endpoint_note}", preset.suggested_model),
                                Style::default().fg(MUTED),
                            ),
                        ])
                    }
                })
                .collect();
            items.push(if compact {
                ListItem::new(Line::from("Custom provider"))
            } else {
                ListItem::new(vec![
                    Line::from("Custom provider"),
                    Line::styled(
                        "  Configure protocol, endpoint URL, model, and API key",
                        Style::default().fg(MUTED),
                    ),
                ])
            });
            let mut state = ListState::default()
                .with_selected(Some(wizard.selected_preset.min(items.len() - 1)));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("❯ ")
                    .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                rows[1],
                &mut state,
            );
            let hint = if compact {
                "↑↓/jk · Enter · Esc"
            } else {
                "↑↓ / j k select · Enter next · Esc back"
            };
            frame.render_widget(
                Paragraph::new(hint).style(Style::default().fg(MUTED)),
                rows[2],
            );
        }
        ProviderWizardStep::Configure => {
            let fields = wizard.fields();
            let active_idx = wizard.active_field.min(fields.len().saturating_sub(1));
            let ultra_compact = area.height < 10;
            let info_rows = if wizard.is_custom || compact || ultra_compact {
                0u16
            } else {
                2u16
            };
            let title_rows = if ultra_compact { 1u16 } else { 2u16 };
            let field_height = if ultra_compact { 2u16 } else { 3u16 };
            let hint_rows = if compact || ultra_compact { 1u16 } else { 2u16 };
            let error_reserve = if ultra_compact { 0u16 } else { 1u16 };
            let field_slots_avail = area
                .height
                .saturating_sub(title_rows + info_rows + error_reserve + hint_rows);
            let shown: Vec<usize> =
                if usize::from(field_slots_avail) < fields.len() * usize::from(field_height) {
                    vec![active_idx]
                } else {
                    (0..fields.len()).collect()
                };
            let mut sizes = vec![Constraint::Length(title_rows)];
            if info_rows > 0 {
                sizes.push(Constraint::Length(info_rows));
            }
            sizes.extend(std::iter::repeat_n(
                Constraint::Length(field_height),
                shown.len(),
            ));
            sizes.push(Constraint::Min(error_reserve));
            sizes.push(Constraint::Length(hint_rows));
            let rows = Layout::vertical(sizes).split(area);

            let title = if wizard.is_custom {
                "Configure custom provider (2/2)".to_owned()
            } else {
                format!("Configure {} (2/2)", wizard.preset_label)
            };
            frame.render_widget(
                Paragraph::new(title)
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                rows[0],
            );

            let mut row_cursor = 1;
            if info_rows > 0 {
                let ep = wizard.preset_endpoint.as_deref().unwrap_or("default");
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled(
                            format!(
                                "Provider: {} ({})",
                                wizard.preset_label,
                                wizard.provider.as_str()
                            ),
                            Style::default().fg(MUTED),
                        ),
                        Line::styled(format!("Endpoint: {ep}"), Style::default().fg(MUTED)),
                    ]),
                    rows[row_cursor],
                );
                row_cursor += 1;
            }

            let stored_already = app
                .credential_store_cache
                .contains(wizard.profile_name.trim());
            for &idx in &shown {
                let kind = fields[idx];
                let active = idx == active_idx;
                let (label, value_display) = match kind {
                    ProviderFieldKind::Protocol => (
                        "Protocol (←/→/Space)".to_owned(),
                        format!("◀ {} ▶", wizard.provider.as_str()),
                    ),
                    ProviderFieldKind::Endpoint => (
                        "Endpoint (full HTTP/HTTPS URL)".to_owned(),
                        wizard.endpoint.clone(),
                    ),
                    ProviderFieldKind::ProfileName => (
                        "Profile / credential name".to_owned(),
                        wizard.profile_name.clone(),
                    ),
                    ProviderFieldKind::Model => ("Model".to_owned(), wizard.model.clone()),
                    ProviderFieldKind::ApiKey => {
                        let suffix = if wizard.ollama_default_hint {
                            " [local default · replace if your server requires auth]"
                        } else if wizard.api_key.is_empty() && stored_already {
                            " [stored · leave blank to keep]"
                        } else {
                            ""
                        };
                        (format!("API Key{suffix}"), wizard.masked_api_key())
                    }
                    ProviderFieldKind::MaxTokens => (
                        if wizard.provider == tars_core::definition::Provider::Anthropic {
                            "Max tokens (required for anthropic)".to_owned()
                        } else {
                            "Max tokens (optional)".to_owned()
                        },
                        wizard.max_tokens.clone(),
                    ),
                };
                let block = Block::default()
                    .borders(Borders::BOTTOM)
                    .title(label)
                    .border_style(Style::default().fg(if active { ACCENT } else { MUTED }));
                let inner = block.inner(rows[row_cursor]);
                frame.render_widget(block, rows[row_cursor]);
                let display = if active && kind != ProviderFieldKind::Protocol {
                    format!("{value_display}▏")
                } else {
                    value_display
                };
                let style = if active && kind == ProviderFieldKind::Protocol {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                frame.render_widget(Paragraph::new(display).style(style), inner);
                row_cursor += 1;
            }

            frame.render_widget(
                Paragraph::new(wizard.error.as_str())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow)),
                rows[row_cursor],
            );
            let hint = if wizard.saving {
                "Saving configuration and 0600 credentials..."
            } else if compact {
                "Tab field · Enter save · Esc back"
            } else if wizard.is_custom {
                "Tab / ↑↓ field · ←/→/Space protocol · Enter / Ctrl-S save · Esc back"
            } else {
                "Tab / ↑↓ field · Enter / Ctrl-S save · Esc back"
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                rows[row_cursor + 1],
            );
        }
    }
}
