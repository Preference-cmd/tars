//! Unified diff generation and line change counting.

use similar::{ChangeTag, TextDiff};

/// Summary of a unified diff and line change statistics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffSummary {
    /// Rendered unified diff text with 3 lines of context.
    pub diff: String,
    /// Number of lines added in the new text.
    pub lines_added: usize,
    /// Number of lines removed from the old text.
    pub lines_removed: usize,
}

/// Compute a unified diff with 3 lines of context and count added and removed lines.
pub fn compute_diff(path: &str, old: &str, new: &str) -> DiffSummary {
    let diff = TextDiff::from_lines(old, new);
    let mut lines_added = 0;
    let mut lines_removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => lines_added += 1,
            ChangeTag::Delete => lines_removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let old_header = format!("a/{path}");
    let new_header = format!("b/{path}");
    let diff_text = diff
        .unified_diff()
        .context_radius(3)
        .header(&old_header, &new_header)
        .to_string();
    DiffSummary {
        diff: diff_text,
        lines_added,
        lines_removed,
    }
}
