use crate::app::{App, Confirmation};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthStr;

pub(super) const ACCENT: Color = Color::Rgb(215, 147, 114);
pub(super) const MUTED: Color = Color::DarkGray;

pub(crate) fn transcript_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}
fn input_height(app: &App, width: u16, height: u16) -> u16 {
    let cells = usize::from(width.saturating_sub(6).max(1));
    let rows = app.entries.get(app.selected).map_or(1, |entry| {
        entry
            .editor
            .lines()
            .iter()
            .map(|line| line.width().div_ceil(cells).max(1))
            .sum::<usize>()
            .clamp(1, 6) as u16
    });
    (rows + 2).min(height.saturating_sub(9).max(3))
}
pub(crate) fn conversation_height(height: u16, width: u16, app: &App) -> usize {
    usize::from(
        height
            .saturating_sub(3 + input_height(app, width, height) + 5)
            .max(1),
    )
}
pub(super) fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(4),
        area.height,
    )
}
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 30 || area.height < 12 {
        frame.render_widget(Paragraph::new("Resize to 30 x 12. Ctrl-Q quits."), area);
        return;
    }
    let body = inset(area);
    let regions = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(input_height(app, area.width, area.height)),
        Constraint::Length(5),
    ])
    .split(body);
    let entry = app.entries.get(app.selected);
    let model = entry
        .map(|e| e.title.as_str())
        .or(app.options.preset.as_ref().map(|p| p.model.as_str()))
        .unwrap_or("No model selected");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "Tarz",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  ·  {model}", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::styled(
                entry.map_or_else(
                    || "Local sessions  ·  /new to begin".into(),
                    |e| {
                        format!(
                            "Session {}  ·  /sessions to switch",
                            e.id().get(..8).unwrap_or(e.id())
                        )
                    },
                ),
                Style::default().fg(MUTED),
            ),
        ]),
        regions[0],
    );
    if let Some(confirmation) = &app.confirmation {
        super::panels::confirm(
            frame,
            confirmation,
            Rect {
                y: regions[1].y,
                height: area.height.saturating_sub(4),
                ..body
            },
        );
        return;
    }
    if app.form.is_some() {
        super::panels::form(
            frame,
            app,
            Rect {
                y: regions[1].y,
                height: area.height.saturating_sub(4),
                ..body
            },
        );
        return;
    }
    if let Some(entry) = entry {
        if let Some(rows) = &entry.rows {
            let end = rows
                .ranges
                .len()
                .saturating_sub(usize::from(regions[1].height));
            let start = entry.scroll.unwrap_or(end).min(end);
            let lines: Vec<_> = rows
                .ranges
                .iter()
                .skip(start)
                .take(usize::from(regions[1].height))
                .map(|range| {
                    let text = &rows.transcript.text[range.clone()];
                    let style = if text.starts_with("❯ ") {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::styled(text, style)
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), regions[1]);
        }
        if entry.transcript.text.is_empty() {
            welcome(frame, regions[1], entry.session.is_some());
        }
    } else {
        welcome(frame, regions[1], false);
    }
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(MUTED));
    let inner = block.inner(regions[2]);
    frame.render_widget(block, regions[2]);
    frame.render_widget(
        Paragraph::new("❯").style(Style::default().fg(ACCENT)),
        inner,
    );
    let editor_area = Rect {
        x: inner.x + 2,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    if let Some(entry) = entry {
        frame.render_widget(&entry.editor, editor_area);
        if !entry.has_draft() && app.panel.is_none() {
            frame.render_widget(
                Paragraph::new(if entry.session.is_some() {
                    "Ask something…"
                } else {
                    "/open this session, or /new"
                })
                .style(Style::default().fg(MUTED)),
                editor_area,
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new("/new to start · /sessions to resume").style(Style::default().fg(MUTED)),
            editor_area,
        );
    }
    footer(frame, app, regions[3]);
    if let Some(panel) = &app.panel {
        super::panels::draw(
            frame,
            app,
            panel,
            Rect {
                y: regions[1].y,
                height: regions[1].height + regions[2].height,
                ..body
            },
        );
    }
}
fn welcome(frame: &mut Frame<'_>, area: Rect, open: bool) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::default(),
            Line::styled(
                if open {
                    "Start a conversation"
                } else {
                    "Welcome to Tarz"
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::default(),
            Line::from(if open {
                "Type below, then press Enter to send."
            } else {
                "/new       Choose a model and create a session"
            }),
            Line::from(if open {
                "Ctrl-J adds a new line. Paste stays in your draft."
            } else {
                "/sessions  Find and reopen your saved sessions"
            }),
            Line::styled(
                "/help      Shortcuts and recovery options",
                Style::default().fg(MUTED),
            ),
        ])
        .wrap(Wrap { trim: false }),
        area,
    );
}
fn footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let entry = app.entries.get(app.selected);
    let message = if let Some(confirmation) = &app.confirmation {
        match confirmation {
            Confirmation::Quit => "Unsubmitted drafts will be lost. Save sessions and quit? y / n",
            Confirmation::Close => "Unsubmitted draft will be lost. Save and close? y / n",
            Confirmation::Discard => {
                "Discard local unsaved content? Existing commits are NOT rolled back. y / n"
            }
        }
    } else if app.quitting {
        "Saving and closing sessions; waiting for actual completion..."
    } else if matches!(app.panel, Some(crate::app::Panel::Info { .. })) {
        "↑↓ / PageUp PageDown scroll · Esc back"
    } else if app.panel.is_some() {
        "Type to filter · ↑↓ select · Enter choose · Esc back"
    } else if entry.is_some_and(|e| e.has_draft()) {
        "Enter send · Ctrl-J newline · F1 commands · Esc interrupt"
    } else {
        "Enter send · Ctrl-J newline · / commands · Esc interrupt"
    };
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if app.confirmation.is_some() {
                Color::Yellow
            } else {
                MUTED
            })),
        sections[0],
    );
    let status = entry
        .map(|e| {
            format!(
                "{} · {}{}",
                e.execution,
                e.status,
                e.busy.map_or(String::new(), |b| format!(" · {b}"))
            )
        })
        .unwrap_or_else(|| format!("{} saved sessions · F1 commands", app.entries.len()));
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(
            if entry.is_some_and(|e| e.status.contains("BLOCKED")) {
                Color::Yellow
            } else {
                MUTED
            },
        )),
        sections[1],
    );
    let entry_notice = entry.map_or("", |e| e.notice.as_str());
    let notice = if app.notice.starts_with("Exit paused") {
        format!("Exit paused · resolve the save failure to quit.\n{entry_notice}")
    } else if !app.notice.starts_with("Config:")
        && !app.notice.starts_with("F2 ")
        && !app.notice.is_empty()
    {
        format!("{}\n{entry_notice}", app.notice)
    } else {
        entry_notice.to_owned()
    };
    let warning = notice.contains("failed")
        || notice.contains("Failed")
        || notice.contains("paused")
        || notice.contains("retained")
        || notice.contains("UNRESOLVED");
    frame.render_widget(
        Paragraph::new(notice)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if warning { Color::Yellow } else { MUTED })),
        sections[2],
    );
}
