use super::surface::{ACCENT, MUTED};
use crate::{
    app::{App, COMMANDS, Panel, matching_commands},
    input::FIELD_NAMES,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &App, panel: &Panel, area: Rect) {
    frame.render_widget(Clear, area);
    let (query, selected, commands) = match panel {
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
                        tarz_core::permission::escape_untrusted_text(line).replace('\t', "    "),
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
            frame.render_widget(
                Paragraph::new(hint)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(MUTED)),
                regions[2],
            );
            return;
        }
        Panel::Commands { query, selected } => (query, *selected, true),
        Panel::Sessions { query, selected } => (query, *selected, false),
        Panel::Info {
            title,
            lines,
            scroll,
        } => {
            let regions = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
            frame.render_widget(
                Paragraph::new(title.as_str())
                    .style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
                regions[0],
            );
            let lines: Vec<_> = crate::transcript::detail_rows(lines, regions[1].width)
                .skip(*scroll)
                .take(usize::from(regions[1].height))
                .map(Line::from)
                .collect();
            frame.render_widget(Paragraph::new(lines), regions[1]);
            return;
        }
    };
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if commands {
                    "Commands  /"
                } else {
                    "Sessions  /"
                },
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(query),
        ])),
        rows[0],
    );
    let items: Vec<_> = if commands {
        matching_commands(query)
            .iter()
            .map(|&index| {
                let (name, description) = COMMANDS[index];
                ListItem::new(vec![
                    Line::from(format!("/{name}")),
                    Line::styled(description, Style::default().fg(MUTED)),
                ])
            })
            .collect()
    } else {
        app.matching_sessions(query)
            .iter()
            .map(|&index| {
                let entry = &app.entries[index];
                let waiting = if !entry.approval_queue.is_empty() {
                    " · waiting for confirmation"
                } else {
                    ""
                };
                ListItem::new(vec![
                    Line::from(format!(
                        "{}  {}",
                        entry.id().get(..8).unwrap_or(entry.id()),
                        entry.title
                    )),
                    Line::styled(
                        format!("{} · {}{waiting}", entry.execution, entry.status),
                        Style::default().fg(if waiting.is_empty() {
                            MUTED
                        } else {
                            Color::Yellow
                        }),
                    ),
                ])
            })
            .collect()
    };
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(if commands {
                "No matching commands. Esc returns to your draft."
            } else {
                "No matching sessions. /new creates one."
            })
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
}

pub(super) fn form(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let form = app.form.as_ref().unwrap();
    let compact = area.height < 2 + 3 * form.fields.len() as u16 + 4;
    let shown: Vec<_> = if compact {
        vec![form.selected]
    } else {
        (0..form.fields.len()).collect()
    };
    let mut sizes = vec![Constraint::Length(2)];
    sizes.extend(std::iter::repeat_n(Constraint::Length(3), shown.len()));
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
        let block = Block::default()
            .borders(Borders::BOTTOM)
            .title(FIELD_NAMES[index])
            .border_style(Style::default().fg(if index == form.selected {
                ACCENT
            } else {
                MUTED
            }));
        let inner = block.inner(rows[row + 1]);
        frame.render_widget(block, rows[row + 1]);
        if index == form.selected {
            frame.render_widget(field, inner);
        } else {
            frame.render_widget(Paragraph::new(field.lines().join("")), inner);
        }
    }
    frame.render_widget(
        Paragraph::new(form.error.as_str())
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::Yellow)),
        rows[shown.len() + 1],
    );
    let hint = if app.confirmation.is_some() {
        "Discard this form and unsent drafts, save sessions and quit? y / n"
    } else {
        "Tab / Shift-Tab field · Ctrl-S create · Esc back\nSettings apply to this new session only."
    };
    frame.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if app.confirmation.is_some() {
                Color::Yellow
            } else {
                MUTED
            })),
        rows[shown.len() + 2],
    );
}

pub(super) fn confirm(frame: &mut Frame<'_>, confirmation: &crate::app::Confirmation, area: Rect) {
    use crate::app::Confirmation;
    let (title, text) = match confirmation {
        Confirmation::Quit => (
            "Save and quit?",
            "Unsubmitted drafts will be lost. Sessions will be saved before exiting.",
        ),
        Confirmation::Close => (
            "Save and close?",
            "This draft will be lost. The session will be saved before closing.",
        ),
        Confirmation::Discard => (
            "Discard local content?",
            "Unsaved materials and draft will be lost. Existing commits are NOT rolled back.",
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
    frame.render_widget(
        Paragraph::new("Confirm? y / n · Esc back")
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(ACCENT)),
        rows[2],
    );
}

pub(super) fn approval(frame: &mut Frame<'_>, _app: &App, entry: &crate::app::Entry, area: Rect) {
    frame.render_widget(Clear, area);
    let Some(req) = entry.approval_queue.front() else {
        return;
    };
    let title_line = match &req.subject {
        tarz_core::permission::ApprovalSubject::FileWrite { path, action, .. } => {
            let action_str = match action {
                tarz_core::permission::WriteAction::Created => "create",
                tarz_core::permission::WriteAction::Updated => "update",
            };
            format!(
                "{} wants to {} {} (permission mode: {})",
                tarz_core::permission::escape_untrusted_text(&req.tool_name),
                action_str,
                tarz_core::permission::escape_untrusted_text(path),
                req.mode,
            )
        }
        tarz_core::permission::ApprovalSubject::McpCall { server, tool, .. } => {
            format!(
                "{} wants to call MCP tool {}/{} (permission mode: {})",
                tarz_core::permission::escape_untrusted_text(&req.tool_name),
                tarz_core::permission::escape_untrusted_text(server),
                tarz_core::permission::escape_untrusted_text(tool),
                req.mode,
            )
        }
    };

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
            tarz_core::permission::ApprovalSubject::FileWrite { .. } => {
                format!(
                    "({} more pending write confirmation(s) in queue)",
                    entry.approval_queue.len() - 1
                )
            }
            tarz_core::permission::ApprovalSubject::McpCall { .. } => {
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
        Constraint::Length(2),
    ])
    .split(area);

    frame.render_widget(Paragraph::new(header_lines), regions[0]);

    let body_height = usize::from(regions[1].height.saturating_sub(2));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED));

    match &req.subject {
        tarz_core::permission::ApprovalSubject::FileWrite { diff, .. } => {
            let escaped_diff = tarz_core::permission::escape_untrusted_text(diff);
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
        tarz_core::permission::ApprovalSubject::McpCall { arguments, .. } => {
            let escaped_args = tarz_core::permission::escape_untrusted_text(arguments);
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
        "Enter / y allow · n deny · Esc cancel turn · Shift-Tab mode · ↑↓ PgUp PgDn scroll"
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(MUTED)),
        regions[3],
    );
}

fn diff_block() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
}
