//! Startup splash: the sandbox and tool checks before the conversation view.
//!
//! Every row and the action line come from the startup state, so the splash
//! only lays out text the state machine already owns.

use super::surface::{ACCENT, MUTED, inset};
use crate::app::Startup;
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

pub(super) fn draw(frame: &mut Frame<'_>, startup: &Startup) {
    let area = inset(frame.area());
    let regions = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "TARS",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}  ·  startup check", env!("CARGO_PKG_VERSION")),
                    Style::default().fg(MUTED),
                ),
            ]),
            Line::default(),
        ]),
        regions[0],
    );
    let tools_style = if startup.downloading() {
        Style::default().fg(ACCENT)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(vec![
            row("Sandbox", startup.sandbox_row(), Style::default()),
            row("ripgrep", startup.tools_row(), tools_style),
        ]),
        regions[1],
    );
    frame.render_widget(
        Paragraph::new(startup.action())
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(ACCENT)),
        regions[2],
    );
    let hint = startup.hint().map(|hint| {
        if startup.asking() {
            format!("{hint} · keys are ignored for a moment after the question appears")
        } else {
            hint.to_owned()
        }
    });
    frame.render_widget(
        Paragraph::new(hint.unwrap_or_default())
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(MUTED)),
        regions[3],
    );
}

/// One label/value row; the label is padded so the values line up.
fn row(label: &'static str, value: String, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(MUTED)),
        Span::styled(value, style),
    ])
}
