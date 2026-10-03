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
            let lines: Vec<_> = lines.iter().map(|line| Line::from(line.as_str())).collect();
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0)),
                regions[1],
            );
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
                ListItem::new(vec![
                    Line::from(format!(
                        "{}  {}",
                        entry.id().get(..8).unwrap_or(entry.id()),
                        entry.title
                    )),
                    Line::styled(
                        format!("{} · {}", entry.execution, entry.status),
                        Style::default().fg(MUTED),
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
    let compact = area.height < 22;
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
