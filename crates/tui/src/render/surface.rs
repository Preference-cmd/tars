use crate::app::{App, Confirmation};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
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
    let pending = app
        .entries
        .get(app.selected)
        .is_some_and(|entry| !entry.approval_queue.is_empty());
    let approval_visible =
        pending && app.panel.is_none() && app.form.is_none() && app.confirmation.is_none();
    let minimum_height = if approval_visible { 19 } else { 12 };
    if app.too_small || area.width < 30 || area.height < minimum_height {
        let message = if approval_visible {
            "Resize to at least 30 x 19 to answer the pending confirmation. Esc cancels this turn. Ctrl-Q quits."
        } else {
            "Resize to 30 x 12. Ctrl-Q quits."
        };
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), area);
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
                    "TARS",
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
    if app.panel.is_none() && entry.is_some_and(|e| !e.approval_queue.is_empty()) {
        super::panels::approval(
            frame,
            app,
            entry.unwrap(),
            Rect {
                y: regions[1].y,
                height: regions[1].height + regions[2].height,
                ..body
            },
        );
        footer(frame, app, regions[3]);
        return;
    }
    if let Some(entry) = entry {
        let history = entry.rows.as_ref().map_or(0, |rows| rows.ranges.len());
        let height = usize::from(regions[1].height);
        let viewport = entry.viewport(height);
        let start = viewport.start;
        let sticky = viewport.sticky;
        let mut lines = Vec::with_capacity(height);
        if sticky && height == 1 {
            // At minimum terminal height, retain both the temporary label and
            // the latest text instead of spending the sole row on a heading.
            let label = entry.preview.compact_label();
            let text = entry.preview.line(start.saturating_sub(history));
            let space = usize::from(regions[1].width).saturating_sub(label.width());
            let mut tail = text.len();
            let mut cells = 0;
            for (offset, grapheme) in text.grapheme_indices(true).rev() {
                cells += grapheme.width();
                if cells > space {
                    break;
                }
                tail = offset;
            }
            lines.push(Line::from(vec![
                Span::styled(
                    label,
                    Style::default().fg(if entry.preview.warning().is_some() {
                        Color::Yellow
                    } else {
                        MUTED
                    }),
                ),
                Span::raw(&text[tail..]),
            ]));
        } else if sticky {
            lines.push(Line::styled(
                if viewport.reserved == 1 && entry.preview.warning().is_some() {
                    entry.preview.compact_label()
                } else {
                    entry.preview.label()
                },
                Style::default().fg(
                    if entry.preview.warning().is_some() && viewport.reserved == 1 {
                        Color::Yellow
                    } else {
                        MUTED
                    },
                ),
            ));
            if let Some(warning) = entry.preview.warning()
                && lines.len() < viewport.reserved
            {
                lines.push(Line::styled(warning, Style::default().fg(Color::Yellow)));
            }
        }
        let tool_lines = entry.tools.lines();
        for index in start..entry.conversation_len() {
            if lines.len() >= height {
                break;
            }
            if index < history {
                let rows = entry.rows.as_ref().unwrap();
                let text = &rows.transcript.text[rows.ranges[index].clone()];
                let style = if text.starts_with("❯ ") {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::styled(text, style));
            } else if index >= history + entry.preview.line_count() {
                if let Some(text) = tool_lines.get(index - history - entry.preview.line_count()) {
                    lines.push(Line::styled(text.clone(), Style::default().fg(MUTED)));
                }
            } else {
                let offset = index - history;
                let text = entry.preview.line(offset);
                let style = if offset == 0 {
                    Style::default().fg(MUTED)
                } else if offset == 1 && entry.preview.warning().is_some() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                lines.push(Line::styled(text, style));
            }
        }
        frame.render_widget(Paragraph::new(lines), regions[1]);
        if entry.transcript.text.is_empty()
            && !entry.preview.is_active()
            && entry.tools.calls.is_empty()
        {
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
                    "Welcome to TARS"
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
    } else if matches!(app.panel, Some(crate::app::Panel::Repair(_))) {
        "↑↓ / PageUp/PageDown scroll · Esc back to draft"
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
    if let Some(e) = entry {
        let mut spans = vec![
            Span::styled(e.execution.clone(), Style::default().fg(MUTED)),
            Span::styled(" · ", Style::default().fg(MUTED)),
            Span::styled(
                e.status.clone(),
                Style::default().fg(if e.status.contains("BLOCKED") {
                    Color::Yellow
                } else {
                    MUTED
                }),
            ),
        ];
        if let Some(b) = e.busy {
            spans.push(Span::styled(format!(" · {b}"), Style::default().fg(MUTED)));
        }
        if let Some((mode, _)) = e.session.as_ref().and_then(|s| s.permission_mode()) {
            spans.push(Span::styled(" · mode ", Style::default().fg(MUTED)));
            let mode_style = if mode == tars_core::permission::PermissionMode::FullAccess {
                Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            spans.push(Span::styled(mode.to_string(), mode_style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), sections[1]);
    } else {
        frame.render_widget(
            Paragraph::new(format!(
                "{} saved sessions · F1 commands",
                app.entries.len()
            ))
            .style(Style::default().fg(MUTED)),
            sections[1],
        );
    }

    let other_waiting = app
        .entries
        .iter()
        .enumerate()
        .find(|(i, e)| *i != app.selected && !e.approval_queue.is_empty())
        .map(|(_, e)| {
            format!(
                "Session {} is waiting for confirmation",
                e.id().get(..8).unwrap_or(e.id())
            )
        });

    let self_waiting = if (app.panel.is_some() || app.form.is_some() || app.confirmation.is_some())
        && entry.is_some_and(|e| !e.approval_queue.is_empty())
    {
        entry.and_then(|e| e.approval_queue.front()).map(|req| {
            format!(
                "{} is waiting for confirmation · Esc to return",
                tars_core::permission::escape_untrusted_text(&req.tool_name)
            )
        })
    } else {
        None
    };

    let waiting_hint = self_waiting.or(other_waiting);

    let entry_notice = entry.map_or("", |e| e.notice.as_str());
    let mut notice = if app.notice.starts_with("Exit paused") {
        format!("Exit paused · resolve the save failure to quit.\n{entry_notice}")
    } else if !app.notice.starts_with("Config:")
        && !app.notice.starts_with("F2 ")
        && !app.notice.is_empty()
    {
        format!("{}\n{entry_notice}", app.notice)
    } else {
        entry_notice.to_owned()
    };
    if let Some(hint) = waiting_hint {
        if notice.is_empty() {
            notice = hint;
        } else {
            notice = format!("{hint}\n{notice}");
        }
    }
    let warning = notice.contains("failed")
        || notice.contains("Failed")
        || notice.contains("paused")
        || notice.contains("retained")
        || notice.contains("UNRESOLVED")
        || notice.contains("waiting for confirmation");
    frame.render_widget(
        Paragraph::new(notice)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if warning { Color::Yellow } else { MUTED })),
        sections[2],
    );
}
