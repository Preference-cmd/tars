use causa::kernel::{BlockContent, ContentPart, Context};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(crate) struct Transcript {
    pub text: String,
    pub tool_details: Vec<String>,
}

impl Transcript {
    pub fn from_context(context: &Context) -> Self {
        let mut text = String::new();
        let mut tool_details = Vec::new();
        for block in context.blocks() {
            match block.meta().source.as_deref() {
                Some("user") => text.push_str("❯ You\n"),
                Some(source) => {
                    text.push_str("● ");
                    text.push_str(source);
                    text.push('\n');
                }
                None => text.push_str("●\n"),
            }
            match block.content() {
                BlockContent::Parts(parts) => {
                    for part in parts {
                        match part {
                            ContentPart::Text(value) => text.push_str(&value.0),
                            ContentPart::Media(_) => text.push_str("[media reference]"),
                        }
                    }
                }
                BlockContent::ToolCall(call) => {
                    text.push_str(&format!(
                        "Tool {} · recorded declaration · {}\n/tools expands actual material",
                        display(&call.tool_name),
                        block.id().0
                    ));
                    tool_details.push(format!(
                        "Declaration {} · {}\n{}",
                        block.id().0,
                        display(&call.tool_name),
                        display(&serde_json::to_string_pretty(&call.arguments).unwrap_or_default())
                    ));
                }
                BlockContent::ToolResult(result) => {
                    let body =
                        serde_json::to_string_pretty(&result.output.content).unwrap_or_default();
                    text.push_str(&format!(
                        "Tool result · {:?} · {}\n{}\n/tools expands actual material",
                        result.status,
                        result.call_block_id.0,
                        summary(&body)
                    ));
                    tool_details.push(format!(
                        "Result for {} · {:?}\n{}",
                        result.call_block_id.0,
                        result.status,
                        display(&serde_json::to_string_pretty(result).unwrap_or_default())
                    ));
                }
            }
            text.push_str("\n\n");
        }
        // Provider text is content, never a stream of terminal control commands.
        text.retain(|c| !c.is_control() || matches!(c, '\n' | '\t'));
        Self {
            text: text.replace('\t', "    "),
            tool_details,
        }
    }
}

pub(crate) fn display(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace('\t', "    ")
}
fn summary(value: &str) -> String {
    let clean = display(value).replace('\n', " ");
    let mut end = clean.len().min(160);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}{}",
        &clean[..end],
        if end < clean.len() { "…" } else { "" }
    )
}

// Detail panels use the same grapheme/cell wrapping for drawing and scrolling.
// Yield borrowed visible rows; don't join or duplicate full result history on draw.
pub(crate) fn detail_rows(lines: &[String], width: u16) -> impl Iterator<Item = &str> {
    lines
        .iter()
        .flat_map(|text| text.split('\n'))
        .flat_map(move |line| {
            let mut remaining = Some(line);
            std::iter::from_fn(move || {
                let line = remaining.take()?;
                let mut columns = 0;
                for (offset, grapheme) in line.grapheme_indices(true) {
                    let cells = grapheme.width();
                    if columns > 0 && columns + cells > usize::from(width.max(1)) {
                        remaining = Some(&line[offset..]);
                        return Some(&line[..offset]);
                    }
                    columns += cells;
                }
                Some(line)
            })
        })
}

pub(crate) struct Rows {
    pub transcript: Arc<Transcript>,
    pub width: u16,
    pub ranges: Vec<Range<usize>>,
}
impl Rows {
    // Run on the blocking pool on new materials/width, never for every frame.
    pub fn wrap(transcript: Arc<Transcript>, width: u16) -> Self {
        let max = usize::from(width.max(1));
        let mut ranges = Vec::new();
        let mut start = 0;
        let mut columns = 0;
        for (offset, grapheme) in transcript.text.grapheme_indices(true) {
            if grapheme == "\n" {
                ranges.push(start..offset);
                start = offset + 1;
                columns = 0;
                continue;
            }
            let cells = grapheme.width();
            if columns > 0 && columns + cells > max {
                ranges.push(start..offset);
                start = offset;
                columns = 0;
            }
            columns += cells;
        }
        ranges.push(start..transcript.text.len());
        Self {
            transcript,
            width,
            ranges,
        }
    }
}
