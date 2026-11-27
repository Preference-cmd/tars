use super::theme::{ACCENT, MUTED, SPINNER_FRAMES, THINKING_GLYPH};
use crate::app::{App, Confirmation, Entry};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::path::{Path, PathBuf};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(crate) fn transcript_width(width: u16) -> u16 {
    width.saturating_sub(4).max(1)
}

pub(crate) fn header_height(height: u16) -> u16 {
    if height <= 14 { 1 } else { 2 }
}

pub(crate) const fn footer_base_height() -> u16 {
    4
}

fn is_obscured(app: &App) -> bool {
    app.panel
        .as_ref()
        .is_some_and(|p| !matches!(p, crate::app::Panel::Commands { .. }))
        || app.form.is_some()
        || app.folder_trust.is_some()
        || app.confirmation.is_some()
        || app.entries.get(app.selected).is_some_and(|e| {
            e.workspace_rules_trust.is_some()
                || e.workspace_agents_trust.is_some()
                || !e.approval_queue.is_empty()
        })
}

pub(crate) fn activity_bar_height(app: &App) -> u16 {
    u16::from(
        !is_obscured(app)
            && app
                .entries
                .get(app.selected)
                .is_some_and(Entry::is_animating),
    )
}

pub(crate) fn scroll_bar_height(app: &App) -> u16 {
    u16::from(
        !is_obscured(app)
            && app
                .entries
                .get(app.selected)
                .is_some_and(|e| e.scroll.is_some()),
    )
}

fn attachment_bar_height(app: &App) -> u16 {
    u16::from(
        app.entries
            .get(app.selected)
            .is_some_and(|e| !e.pending_images.is_empty()),
    )
}

fn queue_bar_height(app: &App) -> u16 {
    u16::from(
        app.entries
            .get(app.selected)
            .is_some_and(|e| !e.queued_prompts.is_empty()),
    )
}

struct LayoutHeights {
    header: u16,
    scroll_bar: u16,
    activity_bar: u16,
    attachment_bar: u16,
    input: u16,
    queue_bar: u16,
    footer_base: u16,
}

fn layout_heights(app: &App, width: u16, height: u16) -> LayoutHeights {
    let header = header_height(height);
    let footer_base = footer_base_height();
    // Reserve at least 1 row for conversation and 3 rows for the input box.
    let slack = height.saturating_sub(header + footer_base + 1 + 3);
    let attachment_bar = attachment_bar_height(app).min(slack);
    let rem = slack - attachment_bar;
    let queue_bar = queue_bar_height(app).min(rem);
    let rem = rem - queue_bar;
    let activity_bar = activity_bar_height(app).min(rem);
    let rem = rem - activity_bar;
    let scroll_bar = scroll_bar_height(app).min(rem);
    let extra_bars = attachment_bar + queue_bar + activity_bar + scroll_bar;

    let cells = usize::from(width.saturating_sub(6).max(1));
    let rows = app
        .active_editor()
        .lines()
        .iter()
        .map(|line| line.width().div_ceil(cells).max(1))
        .sum::<usize>()
        .clamp(1, 6) as u16;
    let max_input = height
        .saturating_sub(header + footer_base + 1 + extra_bars)
        .max(3);
    let input = (rows + 2).min(max_input);
    LayoutHeights {
        header,
        scroll_bar,
        activity_bar,
        attachment_bar,
        input,
        queue_bar,
        footer_base,
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn tail_slice_display_width(text: &str, max_width: usize) -> &str {
    if text.width() <= max_width {
        return text;
    }
    let mut cols = 0usize;
    let mut start = text.len();
    for (offset, grapheme) in text.grapheme_indices(true).rev() {
        let w = grapheme.width();
        if cols + w > max_width {
            break;
        }
        cols += w;
        start = offset;
    }
    &text[start..]
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }
    let limit = max_width - 1;
    let mut cols = 0usize;
    let mut end = 0usize;
    for (offset, grapheme) in text.grapheme_indices(true) {
        let w = grapheme.width();
        if cols + w > limit {
            break;
        }
        cols += w;
        end = offset + grapheme.len();
    }
    format!("{}…", &text[..end])
}

pub(crate) fn truncate_spans_display_width(
    spans: Vec<Span<'static>>,
    max_width: usize,
) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    if total <= max_width {
        return spans;
    }
    if max_width == 0 {
        return Vec::new();
    }
    if max_width == 1 {
        let style = spans
            .last()
            .map_or_else(|| Style::default().fg(MUTED), |s| s.style);
        return vec![Span::styled("…", style)];
    }
    let limit = max_width - 1;
    let mut cols = 0usize;
    let mut out = Vec::new();
    let mut last_style = Style::default().fg(MUTED);
    for span in spans {
        last_style = span.style;
        let w = span.content.width();
        if cols + w <= limit {
            cols += w;
            out.push(span);
        } else {
            let remaining = limit.saturating_sub(cols);
            let mut end = 0usize;
            for (offset, grapheme) in span.content.grapheme_indices(true) {
                let gw = grapheme.width();
                if cols + gw > limit {
                    break;
                }
                cols += gw;
                end = offset + grapheme.len();
            }
            if end > 0 && remaining > 0 {
                out.push(Span::styled(span.content[..end].to_owned(), span.style));
            }
            out.push(Span::styled("…", span.style));
            return out;
        }
    }
    out.push(Span::styled("…", last_style));
    out
}

/// Format a workspace path with `$HOME` -> `~` substitution and tail-preserving ellipsis.
pub(crate) fn format_workspace_label(path: &Path, max_width: usize) -> String {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    format_workspace_label_with_home(path, home.as_deref(), max_width)
}

/// Format a workspace path using an explicit home directory (for deterministic tests).
pub(crate) fn format_workspace_label_with_home(
    path: &Path,
    home: Option<&Path>,
    max_width: usize,
) -> String {
    if max_width == 0 {
        return String::new();
    }
    let home_replaced = match home {
        Some(home) if !home.as_os_str().is_empty() => {
            if path == home {
                "~".to_owned()
            } else if let Ok(rel) = path.strip_prefix(home) {
                let rel_str = rel.to_string_lossy();
                if rel_str.is_empty() {
                    "~".to_owned()
                } else {
                    format!("~/{rel_str}")
                }
            } else {
                path.display().to_string()
            }
        }
        _ => path.display().to_string(),
    };
    let escaped = crate::transcript::display_single_line(&home_replaced);
    if escaped.width() <= max_width {
        return escaped;
    }

    let parts: Vec<&str> = escaped
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    let has_tilde = escaped == "~" || escaped.starts_with("~/") || escaped.starts_with("~\\");

    if has_tilde && parts.len() > 2 {
        let mut tail_parts: Vec<&str> = Vec::new();
        for &part in parts[1..].iter().rev() {
            let tail_width: usize = tail_parts.iter().map(|p| 1 + p.width()).sum();
            let candidate_len = "~/…".width() + tail_width + 1 + part.width();
            if candidate_len <= max_width {
                tail_parts.push(part);
            } else {
                break;
            }
        }
        if !tail_parts.is_empty() {
            tail_parts.reverse();
            return format!("~/…/{}", tail_parts.join("/"));
        }
    }

    if parts.len() >= 2 {
        let mut tail_parts: Vec<&str> = Vec::new();
        for &part in parts.iter().rev() {
            if has_tilde && part == "~" {
                break;
            }
            let tail_width: usize = tail_parts.iter().map(|p| 1 + p.width()).sum();
            let candidate_len = "…".width() + tail_width + 1 + part.width();
            if candidate_len <= max_width {
                tail_parts.push(part);
            } else {
                break;
            }
        }
        if !tail_parts.is_empty() {
            tail_parts.reverse();
            return format!("…/{}", tail_parts.join("/"));
        }
    }

    if max_width == 1 {
        return "…".to_owned();
    }
    let limit = max_width - 1;
    let mut cols = 0usize;
    let mut start = escaped.len();
    for (offset, grapheme) in escaped.grapheme_indices(true).rev() {
        let w = grapheme.width();
        if cols + w > limit {
            break;
        }
        cols += w;
        start = offset;
    }
    format!("…{}", &escaped[start..])
}

pub(crate) fn format_attachment_bar(
    images: &[tars_core::media::PreparedImage],
    max_width: usize,
) -> String {
    let mut joined = String::new();
    for (idx, img) in images.iter().enumerate() {
        if idx > 0 {
            joined.push(' ');
        }
        let name = img.name.as_deref().unwrap_or("image");
        let escaped = tars_core::permission::escape_untrusted_text(name);
        let size = crate::input::image::format_human_size(img.size_bytes);
        joined.push_str(&format!("[{}: {escaped} ({size})]", idx + 1));
    }
    truncate_display_width(&joined, max_width)
}

pub(crate) fn format_queue_preview(text: &str, max_chars: usize) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    let escaped = tars_core::permission::escape_untrusted_text(collapsed.trim());
    truncate_chars(&escaped, max_chars)
}

pub(crate) fn format_queued_prompt_preview(
    prompt: &crate::app::QueuedPrompt,
    max_chars: usize,
) -> String {
    let badge = match prompt.images.len() {
        0 => String::new(),
        1 => "[1 image]".to_owned(),
        n => format!("[{n} images]"),
    };
    if prompt.text.trim().is_empty() {
        return if badge.is_empty() {
            String::new()
        } else {
            truncate_chars(&badge, max_chars)
        };
    }
    if badge.is_empty() {
        return format_queue_preview(&prompt.text, max_chars);
    }
    let badge_prefix = format!("{badge} ");
    let badge_chars = badge_prefix.chars().count();
    if max_chars <= badge_chars {
        return truncate_chars(&badge, max_chars);
    }
    let text_preview = format_queue_preview(&prompt.text, max_chars - badge_chars);
    format!("{badge_prefix}{text_preview}")
}

pub(crate) fn conversation_height(height: u16, width: u16, app: &App) -> usize {
    let h = layout_heights(app, width, height);
    let non_conv = h.header
        + h.scroll_bar
        + h.activity_bar
        + h.attachment_bar
        + h.input
        + h.queue_bar
        + h.footer_base;
    usize::from(height.saturating_sub(non_conv).max(1))
}

pub(super) fn inset(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(4),
        area.height,
    )
}

fn build_header_lines(
    app: &App,
    entry: Option<&Entry>,
    max_width: usize,
    rows: u16,
) -> Vec<Line<'static>> {
    let raw_model = entry
        .map(|e| e.model.as_str())
        .or(app.options.preset.as_ref().map(|p| p.model.as_str()))
        .unwrap_or("No model selected");
    let version = format!(" v{}", env!("CARGO_PKG_VERSION"));
    let mut spans = vec![
        Span::styled(
            "TARS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(version.clone(), Style::default().fg(MUTED)),
    ];
    let mut used = "TARS".width() + version.width();

    let model = crate::transcript::display_single_line(raw_model);
    if used + 3 < max_width {
        let model_cap = model.width().min(max_width - used - 3);
        let model_fit = truncate_display_width(&model, model_cap);
        spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
        spans.push(Span::styled(model_fit.clone(), Style::default().fg(MUTED)));
        used += 3 + model_fit.width();
    }

    if let Some(e) = entry {
        let agent_part = e.viewed_agent_id.as_ref().map(|agent_id| {
            let label = e.viewed_agent_label.as_deref().unwrap_or(agent_id.as_str());
            format!(
                "agent: {} (read-only)",
                crate::transcript::display_single_line(label)
            )
        });
        let short = e.id().get(..8).unwrap_or(e.id());
        let session_base = format!("Session {short}");
        let session_title = e
            .title()
            .map(|t| crate::transcript::display_single_line(&t));
        let workspace_path = e.workspace_root.clone().or_else(|| {
            e.session
                .as_ref()
                .and_then(|s| s.definition().workspace_root)
        });

        let agent_reserve = agent_part.as_ref().map_or(0, |a| 3 + a.width());
        let session_reserve = 3 + session_base.width();

        if let Some(ws) = workspace_path {
            let remaining = max_width.saturating_sub(used);
            if remaining > 6 {
                // Leave room for Session <short> and agent: when wide enough; otherwise give workspace what fits.
                let reserve_others = if remaining >= session_reserve + agent_reserve + 12 {
                    session_reserve + agent_reserve
                } else {
                    0
                };
                let ws_budget = remaining
                    .saturating_sub(3 + reserve_others)
                    .max(8)
                    .min(remaining.saturating_sub(3));
                let ws_label = format_workspace_label(&ws, ws_budget);
                if !ws_label.is_empty() && used + 3 + ws_label.width() <= max_width {
                    spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
                    spans.push(Span::styled(ws_label.clone(), Style::default().fg(MUTED)));
                    used += 3 + ws_label.width();
                }
            }
        }

        if used + 3 < max_width {
            let after_ws = max_width.saturating_sub(used);
            let session_label = if let Some(title) = session_title {
                let avail_for_title =
                    after_ws.saturating_sub(3 + session_base.width() + agent_reserve + 3);
                if avail_for_title >= 4 {
                    let short_title = truncate_display_width(&title, avail_for_title);
                    format!("{session_base} ({short_title})")
                } else {
                    session_base
                }
            } else {
                session_base
            };
            if used + 3 + session_label.width() <= max_width {
                spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
                spans.push(Span::styled(
                    session_label.clone(),
                    Style::default().fg(MUTED),
                ));
                used += 3 + session_label.width();
            }
        }

        if let Some(agent_str) = agent_part
            && used + 3 < max_width
        {
            spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
            spans.push(Span::styled(
                agent_str,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
    } else if used + 3 < max_width {
        spans.push(Span::styled(
            " · Local sessions · /new to begin",
            Style::default().fg(MUTED),
        ));
    }

    let line1 = Line::from(truncate_spans_display_width(spans, max_width));
    if rows > 1 {
        vec![line1, Line::default()]
    } else {
        vec![line1]
    }
}

fn elapsed_suffix(started_at: Option<std::time::Instant>, now: std::time::Instant) -> String {
    started_at
        .map(|t0| format!(" · {:.1}s", now.saturating_duration_since(t0).as_secs_f64()))
        .unwrap_or_default()
}

fn build_activity_line(app: &App, entry: &Entry, max_width: usize) -> Line<'static> {
    let now = app.clock.now();
    let spinner = SPINNER_FRAMES[(app.spinner_tick as usize) % SPINNER_FRAMES.len()];
    let active_tools: Vec<_> = entry.tools.active_calls().collect();

    let spans = if let Some(label) = entry.busy {
        let primary = format!(
            "{spinner} {}…",
            crate::transcript::display_single_line(label)
        );
        let elapsed = elapsed_suffix(entry.busy_started_at, now);
        let mut s = vec![Span::styled(primary, Style::default().fg(ACCENT))];
        if !elapsed.is_empty() {
            s.push(Span::styled(elapsed, Style::default().fg(MUTED)));
        }
        s
    } else if !active_tools.is_empty() {
        let running_tools: Vec<_> = active_tools
            .iter()
            .copied()
            .filter(|call| call.phase == tars_core::session::ToolPhase::Running)
            .collect();
        let (verb, shown_tools) = if running_tools.is_empty() {
            ("Waiting", active_tools.as_slice())
        } else {
            ("Running", running_tools.as_slice())
        };
        let tool_desc = if shown_tools.len() == 1 {
            let call = shown_tools[0];
            let parsed_args =
                serde_json::from_str::<serde_json::Value>(&call.arguments_summary).ok();
            let (title, target) = crate::transcript::friendly_tool_title_and_target(
                &call.tool_name,
                parsed_args.as_ref(),
                Some(&call.arguments_summary),
                None,
            );
            match target {
                Some(target) if !target.is_empty() => format!("{verb} {title} ({target})"),
                _ => format!("{verb} {title}"),
            }
        } else if verb == "Waiting" {
            format!("Waiting on {} tools", shown_tools.len())
        } else {
            format!("Running {} tools", shown_tools.len())
        };
        let elapsed = elapsed_suffix(entry.turn_started_at, now);
        vec![
            Span::styled(
                format!("{spinner} {tool_desc}"),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{elapsed}  ·  Esc to interrupt"),
                Style::default().fg(MUTED),
            ),
        ]
    } else if entry.preview.is_active()
        && !entry.preview.reasoning.is_empty()
        && entry.preview.text.is_empty()
        && !entry.preview.ready
    {
        let elapsed = elapsed_suffix(entry.turn_started_at, now);
        vec![
            Span::styled(
                format!("{THINKING_GLYPH} Thinking"),
                Style::default().fg(ACCENT),
            ),
            Span::styled(
                format!("{elapsed}  ·  Esc to interrupt"),
                Style::default().fg(MUTED),
            ),
        ]
    } else if entry.is_turn_running()
        || entry.preview.is_active()
        || entry.execution == "Running (background wakeup)"
    {
        let elapsed = elapsed_suffix(entry.turn_started_at, now);
        vec![
            Span::styled(format!("{spinner} Generating"), Style::default().fg(ACCENT)),
            Span::styled(
                format!("{elapsed}  ·  Esc to interrupt"),
                Style::default().fg(MUTED),
            ),
        ]
    } else {
        let noun = if entry.active_bg_count == 1 {
            "task"
        } else {
            "tasks"
        };
        vec![Span::styled(
            format!(
                "{spinner} {} background {noun} running",
                entry.active_bg_count
            ),
            Style::default().fg(ACCENT),
        )]
    };

    Line::from(truncate_spans_display_width(spans, max_width))
}

pub(crate) fn draw(frame: &mut Frame<'_>, app: &App) {
    if let Some(startup) = &app.startup {
        super::splash::draw(frame, startup);
        return;
    }
    let area = frame.area();
    let pending = app.entries.get(app.selected).is_some_and(|entry| {
        entry.workspace_rules_trust.is_some()
            || entry.workspace_agents_trust.is_some()
            || !entry.approval_queue.is_empty()
    });
    let approval_visible = pending
        && app.panel.is_none()
        && app.form.is_none()
        && app.folder_trust.is_none()
        && app.confirmation.is_none();
    let minimum_height = if approval_visible { 19 } else { 12 };
    if app.too_small || area.width < 30 || area.height < minimum_height {
        let message = if approval_visible {
            "Resize to at least 30 x 19 to answer the pending confirmation. Esc / Ctrl-C cancels this turn. Ctrl-Q quits."
        } else {
            "Resize to 30 x 12. Ctrl-C / Ctrl-Q quits."
        };
        frame.render_widget(Paragraph::new(message).wrap(Wrap { trim: false }), area);
        return;
    }
    let body = inset(area);
    let heights = layout_heights(app, area.width, area.height);
    let regions = Layout::vertical([
        Constraint::Length(heights.header),
        Constraint::Min(1),
        Constraint::Length(heights.scroll_bar),
        Constraint::Length(heights.activity_bar),
        Constraint::Length(heights.attachment_bar),
        Constraint::Length(heights.input),
        Constraint::Length(heights.footer_base + heights.queue_bar),
    ])
    .split(body);
    let entry = app.entries.get(app.selected);
    frame.render_widget(
        Paragraph::new(build_header_lines(
            app,
            entry,
            usize::from(regions[0].width),
            heights.header,
        )),
        regions[0],
    );
    let overlay_height = area.height.saturating_sub(header_height(area.height) + 1);
    if let Some(confirmation) = &app.confirmation {
        super::panels::confirm(
            frame,
            confirmation,
            Rect {
                y: regions[1].y,
                height: overlay_height,
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
                height: overlay_height,
                ..body
            },
        );
        return;
    }
    if app.folder_trust.is_some() {
        super::panels::folder_trust(
            frame,
            app,
            Rect {
                y: regions[1].y,
                height: overlay_height,
                ..body
            },
        );
        return;
    }
    let between_header_and_footer = regions[6].y.saturating_sub(regions[1].y);
    if app.panel.is_none() && entry.is_some_and(|e| e.workspace_rules_trust.is_some()) {
        super::panels::workspace_rules_trust(
            frame,
            app,
            entry.unwrap(),
            Rect {
                y: regions[1].y,
                height: between_header_and_footer,
                ..body
            },
        );
        footer(frame, app, regions[6], heights.queue_bar > 0);
        return;
    }
    if app.panel.is_none() && entry.is_some_and(|e| e.workspace_agents_trust.is_some()) {
        super::panels::workspace_agents_trust(
            frame,
            app,
            entry.unwrap(),
            Rect {
                y: regions[1].y,
                height: between_header_and_footer,
                ..body
            },
        );
        footer(frame, app, regions[6], heights.queue_bar > 0);
        return;
    }
    if app.panel.is_none() && entry.is_some_and(|e| !e.approval_queue.is_empty()) {
        super::panels::approval(
            frame,
            app,
            entry.unwrap(),
            Rect {
                y: regions[1].y,
                height: between_header_and_footer,
                ..body
            },
        );
        footer(frame, app, regions[6], heights.queue_bar > 0);
        return;
    }
    let mut active_viewport_stats: Option<(usize, usize)> = None;
    if let Some(entry) = entry {
        let history = entry
            .history_rows
            .as_ref()
            .map_or(0, crate::transcript::Rows::len);
        let current = entry.rows.as_ref().map_or(0, crate::transcript::Rows::len);
        let committed = history + current;
        let height = usize::from(regions[1].height);
        let viewport = entry.viewport(height);
        active_viewport_stats = Some((viewport.start, viewport.end));
        let start = viewport.start;
        let sticky = viewport.sticky;
        let mut lines = Vec::with_capacity(height);
        if sticky && height == 1 {
            // At minimum terminal height, retain both the temporary label and
            // the latest text instead of spending the sole row on a heading.
            let label = entry.preview.compact_label();
            let text = entry.preview.line(start.saturating_sub(committed));
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
        let show_live_root = entry.viewed_agent_id.is_none();
        let tool_lines = entry
            .tools
            .styled_lines(app.clock.now(), app.spinner_tick as usize);
        for index in start..entry.conversation_len() {
            if lines.len() >= height {
                break;
            }
            if index < history {
                if let Some(line) = entry
                    .history_rows
                    .as_ref()
                    .and_then(|rows| rows.line(index))
                {
                    lines.push(line.clone());
                }
            } else if index < committed {
                if let Some(line) = entry
                    .rows
                    .as_ref()
                    .and_then(|rows| rows.line(index - history))
                {
                    lines.push(line.clone());
                }
            } else if show_live_root && index >= committed + entry.preview.line_count() {
                if let Some(line) = tool_lines.get(index - committed - entry.preview.line_count()) {
                    lines.push(line.clone());
                }
            } else if show_live_root {
                let offset = index - committed;
                let text = entry.preview.line(offset);
                let style = match entry.preview.line_kind(offset) {
                    crate::preview::PreviewLineKind::Label
                    | crate::preview::PreviewLineKind::Reasoning => Style::default().fg(MUTED),
                    crate::preview::PreviewLineKind::Warning => Style::default().fg(Color::Yellow),
                    crate::preview::PreviewLineKind::ToolDraft => Style::default().fg(Color::Cyan),
                    crate::preview::PreviewLineKind::Body => Style::default(),
                };
                lines.push(Line::styled(text, style));
            }
        }
        if entry.viewed_agent_id.is_some() && entry.transcript.text.is_empty() {
            let label = entry
                .viewed_agent_label
                .as_deref()
                .or(entry.viewed_agent_id.as_deref())
                .unwrap_or("child");
            frame.render_widget(
                Paragraph::new(vec![
                    Line::default(),
                    Line::styled(
                        format!(
                            "Child agent {} (read-only)",
                            tars_core::permission::escape_untrusted_text(label)
                        ),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Line::default(),
                    Line::from("Waiting for idle materials, or no conversation recorded yet."),
                    Line::styled(
                        "Run /agents main to return to the entry agent.",
                        Style::default().fg(MUTED),
                    ),
                ])
                .wrap(Wrap { trim: false }),
                regions[1],
            );
        } else if entry.viewed_agent_id.is_none()
            && entry.transcript.is_conversation_empty()
            && entry.history.is_empty()
            && !entry.preview.is_active()
            && entry.tools.calls.is_empty()
        {
            welcome(frame, app, regions[1], Some(entry));
        } else {
            frame.render_widget(Paragraph::new(lines), regions[1]);
        }
    } else {
        welcome(frame, app, regions[1], None);
    }
    if heights.scroll_bar > 0
        && let Some(entry) = entry
        && let Some((start, end)) = active_viewport_stats
    {
        let bar_width = usize::from(regions[2].width);
        let line = if entry.has_unread_output {
            Line::styled(
                truncate_display_width("↓ New output · End/PgDn", bar_width),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )
        } else {
            let pct = (start * 100 / end.max(1)).min(100);
            let label = format!("↑ Scrolled ({pct}%) · End to follow");
            Line::styled(
                truncate_display_width(&label, bar_width),
                Style::default().fg(MUTED),
            )
        };
        frame.render_widget(Paragraph::new(line), regions[2]);
    }
    if heights.activity_bar > 0
        && let Some(entry) = entry
    {
        frame.render_widget(
            Paragraph::new(build_activity_line(
                app,
                entry,
                usize::from(regions[3].width),
            )),
            regions[3],
        );
    }
    if heights.attachment_bar > 0
        && let Some(entry) = entry
    {
        const PREFIX: &str = "Images: ";
        let avail = usize::from(regions[4].width).saturating_sub(PREFIX.len());
        let bar = format_attachment_bar(&entry.pending_images, avail);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(PREFIX, Style::default().fg(ACCENT)),
                Span::styled(bar, Style::default().fg(MUTED)),
            ])),
            regions[4],
        );
    }
    let commands_query = match &app.panel {
        Some(crate::app::Panel::Commands { query, .. }) => Some(query.as_str()),
        _ => None,
    };
    let block = Block::default()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(if commands_query.is_some() {
            ACCENT
        } else {
            MUTED
        }));
    let inner = block.inner(regions[5]);
    frame.render_widget(block, regions[5]);
    frame.render_widget(
        Paragraph::new("❯").style(Style::default().fg(ACCENT)),
        inner,
    );
    let editor_area = Rect {
        x: inner.x + 2,
        width: inner.width.saturating_sub(2),
        ..inner
    };
    if let Some(query) = commands_query {
        let escaped = crate::transcript::display_single_line(query);
        let full = format!("/{escaped}");
        let max_text_width = usize::from(editor_area.width.saturating_sub(1));
        let shown = tail_slice_display_width(&full, max_text_width);
        let mut spans = vec![
            Span::raw(shown.to_owned()),
            Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
        ];
        if query.is_empty() {
            let used = shown.width() + 1;
            let rem = usize::from(editor_area.width).saturating_sub(used);
            if rem > 0 {
                spans.push(Span::styled(
                    truncate_display_width(" type a command…", rem),
                    Style::default().fg(MUTED),
                ));
            }
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), editor_area);
    } else {
        frame.render_widget(app.active_editor(), editor_area);
        if !app.has_active_text_draft() && app.panel.is_none() {
            let placeholder = if let Some(entry) = entry {
                if entry.viewed_agent_id.is_some() {
                    "Read-only child agent view · /agents to return to main"
                } else if entry.session.is_some() {
                    if entry.pending_images.is_empty() {
                        "Ask something… · Ctrl-C exit"
                    } else {
                        "Add a message or press Enter to send image(s)…"
                    }
                } else {
                    "/open this session, or /new · Ctrl-C exit"
                }
            } else {
                "/new to start · /provider · /sessions · Ctrl-C exit"
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
                    Span::styled(placeholder, Style::default().fg(MUTED)),
                ])),
                editor_area,
            );
        }
    }
    footer(frame, app, regions[6], heights.queue_bar > 0);
    if let Some(crate::app::Panel::Commands { query, selected }) = &app.panel {
        let matches = crate::app::matching_commands(query);
        let desired_rows = if matches.is_empty() {
            if body.width < 33 { 2 } else { 1 }
        } else {
            matches.len().min(8) as u16
        };
        let show_header = regions[1].height >= 2;
        let popup_height = (desired_rows + u16::from(show_header))
            .min(regions[1].height)
            .max(1);
        let show_header = show_header && popup_height >= 2;
        let popup_y = regions[1].y + regions[1].height.saturating_sub(popup_height);
        let popup_area = Rect {
            x: body.x,
            y: popup_y,
            width: body.width,
            height: popup_height,
        };
        super::panels::commands_popup(frame, query, *selected, show_header, popup_area);
    } else if let Some(panel) = &app.panel {
        super::panels::draw(
            frame,
            app,
            panel,
            Rect {
                y: regions[1].y,
                height: between_header_and_footer,
                ..body
            },
        );
    }
}

pub(super) fn approval_agent_prefix(
    origin: &tars_core::permission::ApprovalOrigin,
    entry_id: &str,
) -> String {
    let is_child_id = !origin.agent_id.is_empty() && origin.agent_id != entry_id;
    let is_child_name = !origin.agent_name.is_empty() && origin.agent_name != "main";
    if is_child_id || is_child_name {
        let name = if origin.agent_name.is_empty() {
            origin.agent_id.as_str()
        } else {
            origin.agent_name.as_str()
        };
        format!(
            "[agent: {}] ",
            tars_core::permission::escape_untrusted_text(name)
        )
    } else {
        String::new()
    }
}

/// Footer estimate from `ManagedSession::context_usage`: a percentage of a
/// known window, or a token estimate when the window is unknown, plus an
/// `auto off` marker while automatic compaction is disabled.
pub(super) fn context_label(usage: Option<tars_core::application::ContextUsage>) -> Option<String> {
    let usage = usage?;
    let mut label = match usage.context_window {
        Some(window) if window > 0 => {
            let percent = (u128::from(usage.estimate_tokens) * 100 + u128::from(window) / 2)
                / u128::from(window);
            format!(
                "ctx {percent}% / {}",
                crate::app::history::window_label(window)
            )
        }
        _ => format!(
            "ctx {}",
            crate::app::history::estimate_label(usage.estimate_tokens)
        ),
    };
    if !usage.auto_compact {
        label.push_str(" · auto off");
    }
    Some(label)
}

fn welcome(frame: &mut Frame<'_>, app: &App, area: Rect, entry: Option<&Entry>) {
    let open = entry.is_some_and(|e| e.session.is_some());
    let displayed_workspace = if let Some(entry) = entry {
        entry
            .session
            .as_ref()
            .and_then(|s| s.definition().workspace_root)
            .or_else(|| entry.workspace_root.clone())
    } else {
        app.startup_workspace_root()
    };
    let has_configured_model =
        app.options.preset.is_some() || !app.options.config.model_profiles().is_empty();

    let mut lines = Vec::new();
    if area.height > 4 {
        lines.push(Line::default());
    }
    lines.push(Line::styled(
        if open {
            "Start a conversation"
        } else {
            "Welcome to TARS"
        },
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if area.height > 5 {
        lines.push(Line::default());
    }

    if let Some(ws) = displayed_workspace {
        let is_trusted = app.is_displayed_workspace_trusted(&ws);
        let reserve = if is_trusted { 21 } else { 11 };
        let max_ws_width = usize::from(area.width).saturating_sub(reserve).max(8);
        let ws_label = format_workspace_label(&ws, max_ws_width);
        let mut ws_spans = vec![
            Span::styled("Workspace: ", Style::default().fg(MUTED)),
            Span::raw(ws_label),
        ];
        if is_trusted {
            ws_spans.push(Span::styled(
                " ✓ Trusted",
                Style::default().fg(super::theme::OK),
            ));
        }
        lines.push(Line::from(ws_spans));
    } else {
        lines.push(Line::styled("Workspace: none", Style::default().fg(MUTED)));
    }

    if open {
        lines.push(Line::from("Type below, then press Enter to send."));
        lines.push(Line::from(
            "Ctrl-J adds a new line. Paste stays in your draft.",
        ));
        lines.push(Line::styled(
            "/help      Shortcuts, /trust and recovery options",
            Style::default().fg(MUTED),
        ));
    } else if has_configured_model {
        lines.push(Line::from("/new · /sessions · /commands"));
        lines.push(Line::styled(
            "/help      Shortcuts, /trust and recovery options",
            Style::default().fg(MUTED),
        ));
    } else {
        lines.push(Line::from("/provider to add a model, or /new (F2)"));
        lines.push(Line::styled(
            "/sessions  Find and reopen your saved sessions",
            Style::default().fg(MUTED),
        ));
        lines.push(Line::styled(
            "/help      Shortcuts, /trust and recovery options",
            Style::default().fg(MUTED),
        ));
    }

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
fn build_status_spans(e: &Entry, max_width: usize) -> Vec<Span<'static>> {
    let build = |compact: bool| -> Vec<Span<'static>> {
        let exec_text = if compact && e.execution == "No work in this instance" {
            "Idle".to_owned()
        } else {
            e.execution.clone()
        };
        let exec_style = if e.execution.starts_with("Running") {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };

        let status_text = if compact {
            e.status.replace("unsaved / running", "unsaved")
        } else {
            e.status.clone()
        };
        let status_style = Style::default().fg(if e.status.contains("BLOCKED") {
            Color::Yellow
        } else {
            MUTED
        });

        let mut spans = vec![
            Span::styled(exec_text, exec_style),
            Span::styled(" · ", Style::default().fg(MUTED)),
            Span::styled(status_text, status_style),
        ];
        if let Some(b) = e.busy {
            spans.push(Span::styled(format!(" · {b}"), Style::default().fg(MUTED)));
        }
        if let Some((mode, _)) = e.session.as_ref().and_then(|s| s.permission_mode()) {
            spans.push(Span::styled(" · mode ", Style::default().fg(MUTED)));
            let mode_style = match mode {
                tars_core::permission::PermissionMode::FullAccess => Style::default()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
                tars_core::permission::PermissionMode::AcceptEdits => Style::default().fg(ACCENT),
                _ => Style::default().fg(MUTED),
            };
            spans.push(Span::styled(mode.to_string(), mode_style));
        }
        let usage = e.session.as_ref().and_then(|s| s.context_usage());
        if let Some(mut label) = context_label(usage) {
            if compact && let Some((head, _)) = label.split_once(" / ") {
                let auto_off = label.contains(" · auto off");
                label = if auto_off {
                    format!("{head} · auto off")
                } else {
                    head.to_owned()
                };
            }
            let over = usage.is_some_and(|usage| {
                usage
                    .context_window
                    .is_some_and(|window| usage.estimate_tokens >= window)
            });
            spans.push(Span::styled(" · ", Style::default().fg(MUTED)));
            spans.push(Span::styled(
                label,
                Style::default().fg(if over { Color::Yellow } else { MUTED }),
            ));
        }
        if e.active_bg_count > 0 {
            spans.push(Span::styled(
                format!(" · bg {} running", e.active_bg_count),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        if let Some(agent_id) = &e.viewed_agent_id {
            let name = e.viewed_agent_label.as_deref().unwrap_or(agent_id.as_str());
            let escaped_name = tars_core::permission::escape_untrusted_text(name);
            let display_name = if compact {
                truncate_display_width(&escaped_name, 16)
            } else {
                escaped_name
            };
            spans.push(Span::styled(
                format!(" · viewing agent: {display_name} (read-only)"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        spans
    };

    let full = build(false);
    let full_width: usize = full.iter().map(|s| s.content.width()).sum();
    let chosen = if full_width > max_width && max_width >= 40 {
        build(true)
    } else {
        full
    };
    truncate_spans_display_width(chosen, max_width)
}

fn footer(frame: &mut Frame<'_>, app: &App, area: Rect, show_queue: bool) {
    let entry = app.entries.get(app.selected);
    let message = if let Some(confirmation) = &app.confirmation {
        match confirmation {
            Confirmation::Quit {
                has_queue: true, ..
            } => "Unsubmitted drafts/queued messages will be lost. Save sessions and quit? y / n",
            Confirmation::Quit { .. } => {
                "Unsubmitted drafts will be lost. Save sessions and quit? y / n"
            }
            Confirmation::Close {
                has_queue: true, ..
            } => "Unsubmitted draft/queued messages will be lost. Save and close? y / n",
            Confirmation::Close { .. } => "Unsubmitted draft will be lost. Save and close? y / n",
            Confirmation::Discard => {
                "Discard local unsaved content? Existing commits are NOT rolled back. y / n"
            }
            Confirmation::DeleteSession { .. } => "Permanently delete the selected session? y / n",
        }
    } else if app.quitting {
        "Saving and closing sessions; waiting for actual completion..."
    } else if matches!(app.panel, Some(crate::app::Panel::Repair(_))) {
        "↑↓ / PageUp/PageDown scroll · Esc back to draft"
    } else if matches!(app.panel, Some(crate::app::Panel::Info { .. })) {
        "↑↓ / PageUp PageDown scroll · Esc back"
    } else if matches!(app.panel, Some(crate::app::Panel::Provider(_))) {
        "↑↓ / Tab navigate · Enter confirm · Esc back"
    } else if app.panel.is_some() {
        "Type to filter · ↑↓ select · Enter choose · Esc back"
    } else if app.has_active_text_draft() {
        "Enter send · Ctrl-J newline · F1 commands · Esc interrupt · Ctrl-C exit"
    } else {
        "Enter send · Ctrl-J newline · / commands · Esc interrupt · Ctrl-C exit"
    };
    let has_queue = show_queue && entry.is_some_and(|e| !e.queued_prompts.is_empty());
    let (hint_area, status_area, notice_area) = if has_queue {
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
        ])
        .split(area);
        if let Some(e) = entry {
            let count = e.queued_prompts.len();
            let (prefix, suffix) = if e.queue_paused {
                (
                    format!("Queued ({count}, paused): "),
                    " · /queue pop | /queue clear",
                )
            } else {
                (format!("Queued ({count}): "), " · /queue")
            };
            let avail = usize::from(sections[0].width)
                .saturating_sub(prefix.len() + suffix.len() + 1)
                .clamp(8, 48);
            let preview = e
                .queued_prompts
                .front()
                .map(|front| format_queued_prompt_preview(front, avail))
                .unwrap_or_default();
            let queue_line = format!("{prefix}{preview}{suffix}");
            frame.render_widget(
                Paragraph::new(queue_line).style(Style::default().fg(if e.queue_paused {
                    Color::Yellow
                } else {
                    MUTED
                })),
                sections[0],
            );
        }
        (sections[1], sections[2], sections[3])
    } else {
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
        ])
        .split(area);
        (sections[0], sections[1], sections[2])
    };
    frame.render_widget(
        Paragraph::new(message)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if app.confirmation.is_some() {
                Color::Yellow
            } else {
                MUTED
            })),
        hint_area,
    );
    if let Some(e) = entry {
        let spans = build_status_spans(e, usize::from(status_area.width));
        frame.render_widget(Paragraph::new(Line::from(spans)), status_area);
    } else {
        let summary = truncate_display_width(
            &format!("{} saved sessions · F1 commands", app.entries.len()),
            usize::from(status_area.width),
        );
        frame.render_widget(
            Paragraph::new(summary).style(Style::default().fg(MUTED)),
            status_area,
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

    let self_waiting = if (app.panel.is_some()
        || app.form.is_some()
        || app.folder_trust.is_some()
        || app.confirmation.is_some())
        && entry.is_some_and(|e| !e.approval_queue.is_empty())
    {
        entry.and_then(|e| {
            e.approval_queue.front().map(|req| {
                let prefix = approval_agent_prefix(&req.origin, e.id());
                format!(
                    "{prefix}{} is waiting for confirmation · Esc to return",
                    tars_core::permission::escape_untrusted_text(&req.tool_name)
                )
            })
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
    // Search availability is a startup fact, so it stays in the status area
    // even after later notices replace the transient message above it.
    if let Some(search) = app.search_unavailable_notice() {
        notice = if notice.is_empty() {
            search
        } else {
            format!("{search}\n{notice}")
        };
    }
    let warning = app.search_reason.is_some()
        || notice.contains("failed")
        || notice.contains("Failed")
        || notice.contains("paused")
        || notice.contains("retained")
        || notice.contains("UNRESOLVED")
        || notice.contains("waiting for confirmation");
    frame.render_widget(
        Paragraph::new(notice)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if warning { Color::Yellow } else { MUTED })),
        notice_area,
    );
}

#[cfg(test)]
mod tests {
    use super::context_label;
    use tars_core::application::ContextUsage;

    #[test]
    fn context_label_reports_percent_estimate_and_auto_off() {
        let usage = ContextUsage {
            estimate_tokens: 126_000,
            context_window: Some(200_000),
            auto_compact: true,
            compact_threshold: 70,
        };
        assert_eq!(context_label(Some(usage)).unwrap(), "ctx 63% / 200k");
        let unknown = ContextUsage {
            context_window: None,
            ..usage
        };
        assert_eq!(context_label(Some(unknown)).unwrap(), "ctx ~126k");
        let off = ContextUsage {
            auto_compact: false,
            ..usage
        };
        assert_eq!(
            context_label(Some(off)).unwrap(),
            "ctx 63% / 200k · auto off"
        );
        assert_eq!(context_label(None), None);
    }
}
