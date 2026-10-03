use crate::{
    app::{App, Confirmation},
    input::FIELD_NAMES,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

pub(crate) fn transcript_width(width: u16) -> u16 {
    width
        .saturating_sub(if width >= 80 { 26 } else { 0 })
        .saturating_sub(2)
        .max(1)
}
pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 30 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("Resize terminal to at least 30 x 12. Ctrl-Q quits."),
            area,
        );
        return;
    }
    let regions = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(" TARZ  /  terminal prototype").style(Style::default().fg(Color::Cyan)),
        regions[0],
    );
    if let Some(form) = &app.form {
        let mut constraints = vec![Constraint::Length(3); form.fields.len()];
        constraints.push(Constraint::Min(1));
        let rows = Layout::vertical(constraints).split(regions[1]);
        for (index, field) in form.fields.iter().enumerate() {
            let border = Block::default()
                .borders(Borders::ALL)
                .title(FIELD_NAMES[index])
                .border_style(Style::default().fg(if index == form.selected {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }));
            let inner = border.inner(rows[index]);
            frame.render_widget(border, rows[index]);
            frame.render_widget(field, inner);
        }
        frame.render_widget(
            Paragraph::new(form.error.as_str()).style(Style::default().fg(Color::Yellow)),
            rows[form.fields.len()],
        );
    } else {
        let columns = Layout::horizontal([
            Constraint::Length(if area.width >= 80 { 26 } else { 0 }),
            Constraint::Min(1),
        ])
        .split(regions[1]);
        let items: Vec<_> = app
            .entries
            .iter()
            .map(|e| {
                ListItem::new(format!(
                    "{}  {}",
                    e.id().get(..8).unwrap_or(e.id()),
                    e.title
                ))
            })
            .collect();
        let mut state =
            ListState::default().with_selected((!items.is_empty()).then_some(app.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(Block::bordered().title("Sessions · F4 next"))
                .highlight_style(Style::default().fg(Color::Cyan))
                .highlight_symbol("› "),
            columns[0],
            &mut state,
        );
        if let Some(entry) = app.entries.get(app.selected) {
            let panels = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(5),
                Constraint::Length(3),
            ])
            .split(columns[1]);
            let status = format!(
                "{} | {}\n{}{}",
                entry.title,
                entry.execution,
                entry.status,
                entry.busy.map_or(String::new(), |b| format!(" | {b}"))
            );
            frame.render_widget(Paragraph::new(status), panels[0]);
            let block = Block::bordered().title("Materials · PageUp / PageDown");
            let inner = block.inner(panels[1]);
            frame.render_widget(block, panels[1]);
            if let Some(rows) = &entry.rows {
                let height = usize::from(inner.height);
                let end = rows.ranges.len().saturating_sub(height);
                let start = entry.scroll.unwrap_or(end).min(end);
                let visible: Vec<_> = rows
                    .ranges
                    .iter()
                    .skip(start)
                    .take(height)
                    .map(|range| Line::from(&rows.transcript.text[range.clone()]))
                    .collect();
                frame.render_widget(Paragraph::new(visible), inner);
            }
            let block = Block::bordered().title("Input · Enter newline / Ctrl-S submit");
            let inner = block.inner(panels[2]);
            frame.render_widget(block, panels[2]);
            frame.render_widget(&entry.editor, inner);
            frame.render_widget(
                Paragraph::new(entry.notice.as_str())
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow)),
                panels[3],
            );
        } else {
            frame.render_widget(Paragraph::new("No sessions. F2 opens model configuration.\nStored sessions are loaded without contacting the model."), columns[1]);
        }
    }
    let help = if let Some(confirmation) = &app.confirmation {
        match confirmation {
            Confirmation::Quit => "Unsubmitted drafts will be lost. Save sessions and quit? y / n",
            Confirmation::Close => "Unsubmitted draft will be lost. Save and close this session? y / n",
            Confirmation::Discard => "Discard local unconfirmed materials and draft? Existing commits are NOT rolled back. y / n",
        }.to_owned()
    } else if app.quitting {
        "Saving and closing sessions; waiting for actual completion...".into()
    } else if app.form.is_some() {
        "Tab / Shift-Tab field · Ctrl-S create · Esc back\nProvider: openai-chat / openai-responses / anthropic".into()
    } else {
        format!(
            "F2 new · F3 open · F4 next · F5 refresh · Ctrl-C cancel · Ctrl-Q quit\nCtrl-W close · Ctrl-R retry · Ctrl-E export · Ctrl-D discard\n{}",
            app.notice
        )
    };
    frame.render_widget(Paragraph::new(help), regions[2]);
}
