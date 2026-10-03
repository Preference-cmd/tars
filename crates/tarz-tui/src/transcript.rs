use causa::kernel::{BlockContent, ContentPart, Context};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(crate) struct Transcript {
    pub text: String,
}

impl Transcript {
    pub fn from_context(context: &Context) -> Self {
        let mut text = String::new();
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
                BlockContent::ToolCall(_) => {
                    text.push_str("[recorded tool call; not executed by viewing]")
                }
                BlockContent::ToolResult(_) => text.push_str("[recorded tool result]"),
            }
            text.push_str("\n\n");
        }
        // Provider text is content, never a stream of terminal control commands.
        text.retain(|c| !c.is_control() || matches!(c, '\n' | '\t'));
        Self {
            text: text.replace('\t', "    "),
        }
    }
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
