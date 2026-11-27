use causa::kernel::InvocationId;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{MAX_PREVIEW_BYTES, MAX_REASONING_BYTES};
const MAX_REFLOW_TAIL: usize = 4096;
const MAX_STREAMING_TOOL_CALLS: usize = 32;
const MAX_TOOL_NAME_BYTES: usize = 128;
const MAX_TOOL_ARGS_BYTES: usize = 1024;
const MAX_REASONING_VISIBLE_ROWS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewLineKind {
    Label,
    Warning,
    Reasoning,
    ToolDraft,
    Body,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StreamingToolCall {
    pub call_index: usize,
    pub name: String,
    pub arguments: String,
}

#[derive(Default)]
pub(crate) struct Preview {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<StreamingToolCall>,
    pub invocation: Option<InvocationId>,
    pub ready: bool,
    pub truncated: bool,
    pub lost: u64,
    ranges: Vec<Range<usize>>,
    reasoning_ranges: Vec<Range<usize>>,
    reasoning_header: String,
    tool_call_lines: Vec<String>,
    width: u16,
    laid_out_bytes: usize,
    reasoning_laid_out_bytes: usize,
    reasoning_collapsed_laid_out: bool,
    tool_calls_dirty: bool,
    layout_ready: bool,
    #[cfg(test)]
    pub reflow_bytes: usize,
}

impl Preview {
    pub fn is_active(&self) -> bool {
        self.invocation.is_some()
    }

    pub fn start(&mut self, invocation: InvocationId) {
        let lost = self.lost;
        *self = Self {
            invocation: Some(invocation),
            lost,
            ..Self::default()
        };
    }

    pub fn mark_ready(&mut self) {
        self.ready = true;
        self.tool_call_lines.clear();
        self.refresh_reasoning_header();
    }

    pub fn append(&mut self, delta: &str, truncated: bool) {
        append_escaped_multiline(
            &mut self.text,
            delta,
            MAX_PREVIEW_BYTES,
            truncated,
            &mut self.truncated,
        );
        self.refresh_reasoning_header();
    }

    pub fn append_reasoning(&mut self, delta: &str, truncated: bool) {
        append_escaped_multiline(
            &mut self.reasoning,
            delta,
            MAX_REASONING_BYTES,
            truncated,
            &mut self.truncated,
        );
        self.refresh_reasoning_header();
    }

    pub fn append_tool_call_delta(
        &mut self,
        call_index: usize,
        name_delta: Option<&str>,
        arguments_delta: Option<&str>,
        truncated: bool,
    ) {
        self.truncated |= truncated;
        let pos = match self
            .tool_calls
            .binary_search_by_key(&call_index, |c| c.call_index)
        {
            Ok(idx) => idx,
            Err(idx) => {
                if self.tool_calls.len() >= MAX_STREAMING_TOOL_CALLS {
                    self.truncated = true;
                    return;
                }
                self.tool_calls.insert(
                    idx,
                    StreamingToolCall {
                        call_index,
                        name: String::new(),
                        arguments: String::new(),
                    },
                );
                idx
            }
        };
        let call = &mut self.tool_calls[pos];
        if let Some(name) = name_delta {
            append_escaped_compact(
                &mut call.name,
                name,
                MAX_TOOL_NAME_BYTES,
                &mut self.truncated,
            );
        }
        if let Some(arguments) = arguments_delta {
            append_escaped_compact(
                &mut call.arguments,
                arguments,
                MAX_TOOL_ARGS_BYTES,
                &mut self.truncated,
            );
        }
        self.tool_calls_dirty = true;
    }

    fn is_reasoning_collapsed(&self) -> bool {
        !self.text.is_empty() || self.ready
    }

    fn refresh_reasoning_header(&mut self) {
        if self.reasoning.is_empty() {
            self.reasoning_header.clear();
            return;
        }
        let glyph = crate::render::theme::THINKING_GLYPH;
        self.reasoning_header = if !self.is_reasoning_collapsed() {
            format!("{glyph} Thinking…")
        } else if self.ready {
            format!("{glyph} Thought ({} chars)", self.reasoning.chars().count())
        } else {
            format!(
                "{glyph} Thinking ({} chars)",
                self.reasoning.chars().count()
            )
        };
    }

    pub fn layout(&mut self, width: u16) {
        let width = width.max(1);
        let collapsed = self.is_reasoning_collapsed();
        let text_dirty = self.width != width || self.laid_out_bytes != self.text.len();
        let reasoning_dirty = !self.reasoning.is_empty()
            && !collapsed
            && (self.width != width
                || self.reasoning_laid_out_bytes != self.reasoning.len()
                || self.reasoning_collapsed_laid_out);
        let tool_dirty =
            self.width != width || self.tool_calls_dirty || self.layout_ready != self.ready;

        if !text_dirty
            && !reasoning_dirty
            && !tool_dirty
            && self.reasoning_collapsed_laid_out == collapsed
        {
            return;
        }

        if text_dirty {
            #[cfg(test)]
            let reflow = Some(&mut self.reflow_bytes);
            #[cfg(not(test))]
            let reflow = None;
            wrap_incremental(
                &self.text,
                &mut self.ranges,
                self.width,
                self.laid_out_bytes,
                width,
                reflow,
            );
        }

        self.refresh_reasoning_header();
        if self.reasoning.is_empty() || collapsed {
            self.reasoning_ranges.clear();
        } else if reasoning_dirty {
            let prev_width = if self.reasoning_collapsed_laid_out {
                0
            } else {
                self.width
            };
            wrap_incremental(
                &self.reasoning,
                &mut self.reasoning_ranges,
                prev_width,
                self.reasoning_laid_out_bytes,
                width,
                None,
            );
        }

        if self.ready || self.tool_calls.is_empty() {
            self.tool_call_lines.clear();
        } else if tool_dirty {
            self.tool_call_lines = self
                .tool_calls
                .iter()
                .map(|call| format_tool_draft(call, usize::from(width)))
                .collect();
        }

        self.width = width;
        self.laid_out_bytes = self.text.len();
        self.reasoning_laid_out_bytes = self.reasoning.len();
        self.reasoning_collapsed_laid_out = collapsed;
        self.tool_calls_dirty = false;
        self.layout_ready = self.ready;
    }

    #[cfg(test)]
    pub fn index_bytes(&self) -> usize {
        self.ranges.capacity() * std::mem::size_of::<Range<usize>>()
    }

    pub fn label(&self) -> &'static str {
        if self.ready {
            "● Temporary output · awaiting result"
        } else {
            "● Generating · temporary"
        }
    }

    pub fn compact_label(&self) -> &'static str {
        match (self.lost > 0, self.truncated) {
            (true, true) => "● Temp lost/cut: ",
            (true, false) => "● Temp lost: ",
            (false, true) => "● Temp cut: ",
            _ if self.ready => "● Temp ready: ",
            _ => "● Generating temp: ",
        }
    }

    pub fn warning(&self) -> Option<&'static str> {
        match (self.lost > 0, self.truncated) {
            (true, true) => Some("Preview incomplete: updates lost and text truncated"),
            (true, false) => Some("Preview incomplete: updates lost"),
            (false, true) => Some("Preview incomplete: text truncated"),
            _ => None,
        }
    }

    fn reasoning_tail_window(&self) -> (usize, usize) {
        if self.reasoning.is_empty() || self.is_reasoning_collapsed() {
            return (0, 0);
        }
        let trailing_blank = self.reasoning_ranges.len() > 1
            && self.reasoning_ranges.last().is_some_and(Range::is_empty);
        let available = self.reasoning_ranges.len() - usize::from(trailing_blank);
        let count = available.min(MAX_REASONING_VISIBLE_ROWS);
        let start = available.saturating_sub(count);
        (start, count)
    }

    fn visible_body_rows(&self) -> usize {
        let has_other_stream =
            !self.reasoning.is_empty() || (!self.ready && !self.tool_calls.is_empty());
        if self.text.is_empty() && has_other_stream {
            return 0;
        }
        let trailing_blank =
            self.ranges.len() > 1 && self.ranges.last().is_some_and(Range::is_empty);
        self.ranges.len() - usize::from(trailing_blank)
    }

    fn visible_tool_draft_rows(&self) -> usize {
        if self.ready {
            0
        } else {
            self.tool_call_lines.len()
        }
    }

    pub fn line_count(&self) -> usize {
        if !self.is_active() {
            return 0;
        }
        let reasoning_rows = if self.reasoning.is_empty() {
            0
        } else {
            1 + self.reasoning_tail_window().1
        };
        1 + usize::from(self.warning().is_some())
            + reasoning_rows
            + self.visible_body_rows()
            + self.visible_tool_draft_rows()
    }

    fn resolve_line(&self, index: usize) -> (&str, PreviewLineKind) {
        if !self.is_active() {
            return ("", PreviewLineKind::Body);
        }
        if index == 0 {
            return (self.label(), PreviewLineKind::Label);
        }
        let mut index = index - 1;
        if let Some(warning) = self.warning() {
            if index == 0 {
                return (warning, PreviewLineKind::Warning);
            }
            index -= 1;
        }
        if !self.reasoning.is_empty() {
            if index == 0 {
                return (&self.reasoning_header, PreviewLineKind::Reasoning);
            }
            index -= 1;
            let (tail_start, tail_count) = self.reasoning_tail_window();
            if index < tail_count {
                let text = self
                    .reasoning_ranges
                    .get(tail_start + index)
                    .map_or("", |range| &self.reasoning[range.clone()]);
                return (text, PreviewLineKind::Reasoning);
            }
            index -= tail_count;
        }
        let body_rows = self.visible_body_rows();
        if index < body_rows {
            let text = self
                .ranges
                .get(index)
                .map_or("", |range| &self.text[range.clone()]);
            return (text, PreviewLineKind::Body);
        }
        index -= body_rows;
        if !self.ready
            && let Some(line) = self.tool_call_lines.get(index)
        {
            return (line.as_str(), PreviewLineKind::ToolDraft);
        }
        ("", PreviewLineKind::Body)
    }

    pub fn line(&self, index: usize) -> &str {
        self.resolve_line(index).0
    }

    pub fn line_kind(&self, index: usize) -> PreviewLineKind {
        self.resolve_line(index).1
    }
}

fn append_escaped_multiline(
    buffer: &mut String,
    delta: &str,
    max_bytes: usize,
    incoming_truncated: bool,
    truncated: &mut bool,
) {
    *truncated |= incoming_truncated;
    let escaped = tars_core::permission::escape_untrusted_text(delta);
    for c in escaped.chars() {
        let content = if c == '\t' {
            "    "
        } else if c.is_control() && c != '\n' {
            continue;
        } else {
            let mut scratch = [0; 4];
            let encoded = c.encode_utf8(&mut scratch);
            if buffer.len() + encoded.len() > max_bytes {
                *truncated = true;
                break;
            }
            buffer.push(c);
            continue;
        };
        if buffer.len() + content.len() > max_bytes {
            *truncated = true;
            break;
        }
        buffer.push_str(content);
    }
}

fn append_escaped_compact(
    buffer: &mut String,
    delta: &str,
    max_bytes: usize,
    truncated: &mut bool,
) {
    let escaped = tars_core::permission::escape_untrusted_text(delta);
    for c in escaped.chars() {
        let mapped = if c.is_control() { ' ' } else { c };
        if mapped == ' ' && buffer.ends_with(' ') {
            continue;
        }
        let mut scratch = [0; 4];
        let encoded = mapped.encode_utf8(&mut scratch);
        if buffer.len() + encoded.len() > max_bytes {
            *truncated = true;
            break;
        }
        buffer.push(mapped);
    }
}

fn format_tool_draft(call: &StreamingToolCall, max_cells: usize) -> String {
    let name = call.name.trim();
    let display_name = if name.is_empty() { "tool" } else { name };
    let args = call.arguments.trim();
    let raw = if args.is_empty() {
        format!("⠋ Preparing {display_name}")
    } else {
        format!("⠋ Preparing {display_name}: {args}")
    };
    truncate_cells(&raw, max_cells)
}

fn truncate_cells(text: &str, max_cells: usize) -> String {
    if text.width() <= max_cells {
        return text.to_owned();
    }
    if max_cells == 0 {
        return String::new();
    }
    let budget = max_cells.saturating_sub(1);
    let mut end = 0;
    let mut cells = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        let w = grapheme.width();
        if cells + w > budget {
            break;
        }
        cells += w;
        end = offset + grapheme.len();
    }
    let mut out = text[..end].to_owned();
    out.push('…');
    out
}

fn wrap_incremental(
    text: &str,
    ranges: &mut Vec<Range<usize>>,
    laid_out_width: u16,
    laid_out_bytes: usize,
    width: u16,
    reflow_counter: Option<&mut usize>,
) {
    let mut start = 0;
    if laid_out_width == width && !ranges.is_empty() {
        // Reflow two last rows, including the grapheme that may have been
        // continued by a combining mark / emoji sequence in the next delta.
        let keep = ranges.len().saturating_sub(2);
        start = ranges[keep].start;
        ranges.truncate(keep);
        // A zero-width combining sequence can otherwise make one row's
        // reflow cost grow with the entire prefix. Limit retained lookback
        // at a UTF-8 boundary; pathological clusters may span visual rows.
        let mut bounded = laid_out_bytes.saturating_sub(MAX_REFLOW_TAIL);
        while !text.is_char_boundary(bounded) {
            bounded += 1;
        }
        if bounded > start {
            ranges.push(start..bounded);
            start = bounded;
        }
    } else {
        ranges.clear();
    }
    if let Some(counter) = reflow_counter {
        *counter += text.len() - start;
    }
    let mut row_start = start;
    let mut columns = 0;
    for (offset, grapheme) in text[start..].grapheme_indices(true) {
        let offset = offset + start;
        if grapheme == "\n" {
            ranges.push(row_start..offset);
            row_start = offset + 1;
            columns = 0;
            continue;
        }
        let cells = grapheme.width();
        if columns > 0 && columns + cells > usize::from(width) {
            ranges.push(row_start..offset);
            row_start = offset;
            columns = 0;
        }
        columns += cells;
    }
    ranges.push(row_start..text.len());
}

#[cfg(test)]
mod escape_tests {
    use super::Preview;

    #[test]
    fn preview_escapes_controls_bidi_and_separators_but_keeps_unicode() {
        let mut preview = Preview::default();
        preview.append("中文 😀\nline\u{202e}\u{2066}\u{2028}\tend", false);
        assert!(preview.text.contains("中文 😀"));
        assert!(preview.text.contains('\n'));
        assert!(preview.text.contains("\\u{202e}"));
        assert!(preview.text.contains("\\u{2066}"));
        assert!(preview.text.contains("\\u{2028}"));
        assert!(!preview.text.contains('\u{202e}'));
        assert!(!preview.text.contains('\u{2066}'));
        assert!(!preview.text.contains('\u{2028}'));
        assert!(preview.text.contains("    end"));
    }
}
