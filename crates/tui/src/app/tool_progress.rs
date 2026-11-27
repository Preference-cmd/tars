//! Bounded transient tool state; complete content comes from material cache.
use crate::render::theme;
use causa::kernel::ToolResultStatus;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::collections::HashMap;
use std::time::Instant;
use tars_core::session::{ToolPhase, ToolProgress, ToolProgressSnapshot};

const MAX_CALLS: usize = 128;

#[derive(Clone, Copy, Debug)]
struct ToolTiming {
    started_at: Instant,
    finished_at: Option<Instant>,
}

fn is_terminal_phase(phase: ToolPhase) -> bool {
    matches!(
        phase,
        ToolPhase::Returned | ToolPhase::Committed | ToolPhase::NotStarted | ToolPhase::Unknown
    )
}

#[derive(Default)]
pub(crate) struct ToolView {
    pub calls: Vec<ToolProgress>,
    timings: Vec<ToolTiming>,
    pub lost: u64,
    pub truncated: bool,
    pub corrected: bool,
}

impl ToolView {
    pub fn apply(&mut self, call: &ToolProgress, now: Instant) {
        if let Some(idx) = self.calls.iter().position(|previous| {
            previous.invocation_id == call.invocation_id
                && previous.call_block_id == call.call_block_id
        }) {
            // A delayed advisory return must never undo observed commit.
            if self.calls[idx].phase == ToolPhase::Committed && call.phase != ToolPhase::Committed {
                return;
            }
            self.calls[idx] = call.clone();
            if is_terminal_phase(call.phase) {
                self.timings[idx].finished_at.get_or_insert(now);
            } else {
                self.timings[idx].finished_at = None;
            }
        } else if self.calls.len() < MAX_CALLS {
            self.calls.push(call.clone());
            self.timings.push(ToolTiming {
                started_at: now,
                finished_at: is_terminal_phase(call.phase).then_some(now),
            });
        } else {
            self.truncated = true;
        }
    }

    pub fn reconcile(&mut self, snapshot: &ToolProgressSnapshot, now: Instant) {
        let mut previous_timings: HashMap<_, ToolTiming> = self
            .calls
            .drain(..)
            .zip(self.timings.drain(..))
            .map(|(call, timing)| ((call.invocation_id, call.call_block_id), timing))
            .collect();
        self.truncated = snapshot.truncated || snapshot.calls.len() > MAX_CALLS;
        for call in snapshot.calls.iter().take(MAX_CALLS) {
            let mut timing = previous_timings
                .remove(&(call.invocation_id.clone(), call.call_block_id))
                .unwrap_or(ToolTiming {
                    started_at: now,
                    finished_at: None,
                });
            if is_terminal_phase(call.phase) {
                timing.finished_at.get_or_insert(now);
            } else {
                timing.finished_at = None;
            }
            self.calls.push(call.clone());
            self.timings.push(timing);
        }
        self.corrected = true;
    }

    pub fn has_active_calls(&self) -> bool {
        self.calls
            .iter()
            .any(|call| matches!(call.phase, ToolPhase::Waiting | ToolPhase::Running))
    }

    pub fn active_calls(&self) -> impl Iterator<Item = &ToolProgress> {
        self.calls
            .iter()
            .filter(|call| matches!(call.phase, ToolPhase::Waiting | ToolPhase::Running))
    }

    pub fn line_count(&self) -> usize {
        usize::from(self.lost > 0 || self.truncated) + self.calls.len()
    }

    pub fn styled_lines(&self, now: Instant, spinner_frame: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.line_count());
        if self.lost > 0 || self.truncated {
            lines.push(Line::styled(
                format!(
                    "Tool observations incomplete · {} lost{}{}",
                    self.lost,
                    if self.truncated {
                        " · bounded view"
                    } else {
                        ""
                    },
                    if self.corrected {
                        " · corrected from final facts"
                    } else {
                        ""
                    }
                ),
                Style::default().fg(theme::WARN),
            ));
        }
        let spinner = theme::SPINNER_FRAMES[spinner_frame % theme::SPINNER_FRAMES.len()];
        for (idx, call) in self.calls.iter().enumerate() {
            let timing = self.timings.get(idx).copied().unwrap_or(ToolTiming {
                started_at: now,
                finished_at: None,
            });
            let elapsed = timing
                .finished_at
                .unwrap_or(now)
                .saturating_duration_since(timing.started_at);
            let elapsed_str = format!("{:.1}s", elapsed.as_secs_f64());

            let parsed_args =
                serde_json::from_str::<serde_json::Value>(&call.arguments_summary).ok();
            let (title, target) = crate::transcript::friendly_tool_title_and_target(
                &call.tool_name,
                parsed_args.as_ref(),
                Some(&call.arguments_summary),
                None,
            );

            let (glyph, glyph_color, phase_label, is_active) = match call.phase {
                ToolPhase::Waiting => (spinner, theme::ACCENT, "Waiting", true),
                ToolPhase::Running => (spinner, theme::ACCENT, "Running", true),
                ToolPhase::Returned if call.operation_not_started => (
                    theme::TOOL_WARN_GLYPH,
                    theme::WARN,
                    "Not executed · Pending material commit",
                    false,
                ),
                ToolPhase::Returned if call.status == Some(ToolResultStatus::UnknownOutcome) => (
                    theme::TOOL_WARN_GLYPH,
                    theme::WARN,
                    "Execution result unknown · Pending material commit",
                    false,
                ),
                ToolPhase::Returned => {
                    let (g, c) = terminal_status_glyph(&call.status);
                    (g, c, "Returned · Pending material commit", false)
                }
                ToolPhase::Committed if call.operation_not_started => (
                    theme::TOOL_WARN_GLYPH,
                    theme::WARN,
                    "Committed · Not executed",
                    false,
                ),
                ToolPhase::Committed if call.status == Some(ToolResultStatus::UnknownOutcome) => (
                    theme::TOOL_WARN_GLYPH,
                    theme::WARN,
                    "Committed · Execution result unknown",
                    false,
                ),
                ToolPhase::Committed => {
                    let (g, c) = terminal_status_glyph(&call.status);
                    (g, c, "Committed", false)
                }
                ToolPhase::NotStarted => {
                    (theme::TOOL_WARN_GLYPH, theme::WARN, "Not executed", false)
                }
                ToolPhase::Unknown => (
                    theme::TOOL_WARN_GLYPH,
                    theme::WARN,
                    "Execution result unknown",
                    false,
                ),
            };

            let status = call
                .status
                .as_ref()
                .filter(|status| {
                    !call.operation_not_started || **status != ToolResultStatus::UnknownOutcome
                })
                .map(|s| format!(" · {s:?}"))
                .unwrap_or_default();
            let provenance = call
                .permission_decision
                .as_ref()
                .and_then(|decision| decision.rule_summary())
                .map(|summary| format!(" · {}", crate::transcript::display_single_line(&summary)))
                .unwrap_or_default();

            let mut spans = vec![
                Span::styled(format!("{glyph} "), Style::default().fg(glyph_color)),
                Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
            ];
            if let Some(target) = target
                && !target.is_empty()
            {
                spans.push(Span::raw(format!("  {target}")));
            }
            if is_active {
                spans.push(Span::styled(
                    format!(" · {phase_label}"),
                    Style::default().fg(theme::ACCENT),
                ));
                spans.push(Span::styled(
                    format!(" · {elapsed_str}{provenance}"),
                    Style::default().fg(theme::MUTED),
                ));
            } else {
                let meta_color = if glyph_color == theme::ERR {
                    theme::ERR
                } else if glyph_color == theme::WARN {
                    theme::WARN
                } else {
                    theme::MUTED
                };
                spans.push(Span::styled(
                    format!(" · {phase_label}{status} · {elapsed_str}{provenance}"),
                    Style::default().fg(meta_color),
                ));
            }
            lines.push(Line::from(spans));
        }
        lines
    }

    #[cfg(test)]
    pub fn lines(&self) -> Vec<String> {
        let now = self
            .timings
            .iter()
            .filter_map(|t| t.finished_at.or(Some(t.started_at)))
            .max()
            .unwrap_or_else(Instant::now);
        self.styled_lines(now, 0)
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|s| s.content.into_owned())
                    .collect::<String>()
            })
            .collect()
    }
}

fn terminal_status_glyph(
    status: &Option<ToolResultStatus>,
) -> (&'static str, ratatui::style::Color) {
    match status {
        Some(
            ToolResultStatus::Failed
            | ToolResultStatus::Rejected
            | ToolResultStatus::Cancelled
            | ToolResultStatus::TimedOut,
        ) => (theme::TOOL_ERR_GLYPH, theme::ERR),
        Some(ToolResultStatus::UnknownOutcome) => (theme::TOOL_WARN_GLYPH, theme::WARN),
        _ => (theme::TOOL_DONE_GLYPH, theme::OK),
    }
}
