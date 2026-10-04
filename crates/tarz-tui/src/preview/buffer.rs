use causa::kernel::InvocationId;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::MAX_PREVIEW_BYTES;
const MAX_REFLOW_TAIL: usize = 4096;

#[derive(Default)]
pub(crate) struct Preview {
    pub text: String,
    pub invocation: Option<InvocationId>,
    pub ready: bool,
    pub truncated: bool,
    pub lost: u64,
    ranges: Vec<Range<usize>>,
    width: u16,
    laid_out_bytes: usize,
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
    pub fn append(&mut self, delta: &str, truncated: bool) {
        self.truncated |= truncated;
        for c in delta.chars() {
            let content = if c == '\t' {
                "    "
            } else if c.is_control() && c != '\n' {
                continue;
            } else {
                let mut buffer = [0; 4];
                let encoded = c.encode_utf8(&mut buffer);
                if self.text.len() + encoded.len() > MAX_PREVIEW_BYTES {
                    self.truncated = true;
                    break;
                }
                self.text.push(c);
                continue;
            };
            if self.text.len() + content.len() > MAX_PREVIEW_BYTES {
                self.truncated = true;
                break;
            }
            self.text.push_str(content);
        }
    }
    pub fn layout(&mut self, width: u16) {
        let width = width.max(1);
        if self.width == width && self.laid_out_bytes == self.text.len() {
            return;
        }
        let mut start = 0;
        if self.width == width && !self.ranges.is_empty() {
            // Reflow two last rows, including the grapheme that may have been
            // continued by a combining mark / emoji sequence in the next delta.
            let keep = self.ranges.len().saturating_sub(2);
            start = self.ranges[keep].start;
            self.ranges.truncate(keep);
            // A zero-width combining sequence can otherwise make one row's
            // reflow cost grow with the entire prefix. Limit retained lookback
            // at a UTF-8 boundary; pathological clusters may span visual rows.
            let mut bounded = self.laid_out_bytes.saturating_sub(MAX_REFLOW_TAIL);
            while !self.text.is_char_boundary(bounded) {
                bounded += 1;
            }
            if bounded > start {
                self.ranges.push(start..bounded);
                start = bounded;
            }
        } else {
            self.ranges.clear();
        }
        #[cfg(test)]
        {
            self.reflow_bytes += self.text.len() - start;
        }
        let mut row_start = start;
        let mut columns = 0;
        for (offset, grapheme) in self.text[start..].grapheme_indices(true) {
            let offset = offset + start;
            if grapheme == "\n" {
                self.ranges.push(row_start..offset);
                row_start = offset + 1;
                columns = 0;
                continue;
            }
            let cells = grapheme.width();
            if columns > 0 && columns + cells > usize::from(width) {
                self.ranges.push(row_start..offset);
                row_start = offset;
                columns = 0;
            }
            columns += cells;
        }
        self.ranges.push(row_start..self.text.len());
        self.width = width;
        self.laid_out_bytes = self.text.len();
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
    pub fn line_count(&self) -> usize {
        if !self.is_active() {
            return 0;
        }
        let trailing_blank =
            self.ranges.len() > 1 && self.ranges.last().is_some_and(Range::is_empty);
        1 + usize::from(self.warning().is_some()) + self.ranges.len() - usize::from(trailing_blank)
    }
    pub fn line(&self, index: usize) -> &str {
        if index == 0 {
            return self.label();
        }
        let mut index = index - 1;
        if let Some(warning) = self.warning() {
            if index == 0 {
                return warning;
            }
            index -= 1;
        }
        self.ranges
            .get(index)
            .map_or("", |range| &self.text[range.clone()])
    }
}
