//! Text lines for session usage, shared by the CLI and the TUI.

use super::types::{PurposeUsage, SessionUsage, UsageFigures};

/// Text lines describing `usage`, shown by `tars session show` and the TUI
/// `/status` panel.
///
/// The first line gives the call counts and input rounds; the next lines give
/// the token figures for the total, each purpose and the last call. A value no
/// call reported reads "not reported", never 0.
pub fn session_usage_lines(usage: &SessionUsage) -> Vec<String> {
    let combined = usage.combined();
    let mut lines = vec![
        format!(
            "Usage: {} call(s) ({} ok, {} failed, {} without usage) · {} input round(s)",
            combined.calls.total(),
            combined.calls.succeeded,
            combined.calls.failed,
            combined.calls.unreported,
            usage.input_rounds,
        ),
        format!("  Total: {}", figures_text(&combined.figures())),
        purpose_line("Conversation", &usage.conversation),
        purpose_line("Compaction", &usage.compaction),
    ];
    if let Some(last) = &usage.last_call {
        lines.push(format!(
            "  Last call ({}): {}",
            last.purpose.as_str(),
            figures_text(&last.figures())
        ));
    }
    lines
}

fn purpose_line(label: &str, usage: &PurposeUsage) -> String {
    if usage.calls.total() == 0 {
        format!("  {label}: no calls")
    } else {
        format!("  {label}: {}", figures_text(&usage.figures()))
    }
}

fn figures_text(figures: &UsageFigures) -> String {
    let count = |value: Option<u64>| value.map_or_else(|| "not reported".into(), |v| v.to_string());
    let rate = figures.cache_hit_rate.map_or_else(
        || "not reported".into(),
        |rate| format!("{:.1}%", rate * 100.0),
    );
    format!(
        "input {} · cache read {} · cache write {} · output {} · reasoning {} · prompt {} · total {} · cache hit {rate}",
        count(figures.input_tokens),
        count(figures.cache_read_tokens),
        count(figures.cache_write_tokens),
        count(figures.output_tokens),
        count(figures.reasoning_tokens),
        count(figures.prompt_tokens),
        count(figures.total_tokens),
    )
}
