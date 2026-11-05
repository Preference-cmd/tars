use causa::kernel::{BlockContent, ContentPart, Context, ContextBlock};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(crate) struct Transcript {
    pub text: String,
    pub tool_details: Vec<String>,
    /// Full text of the current Context's summary blocks (the brief chain), when it has any.
    ///
    /// The current material collapses those blocks to folded rows; the panel opened
    /// by `/summary` reads this raw copy (the whole chain, oldest first) instead of the rendered rows.
    pub summary: Option<String>,
    /// Range of blocks in the Context that form the history run.
    pub history_range: Range<usize>,
}

impl Transcript {
    /// The current Context, splitting off its fixed prefix and the history run.
    pub fn from_materials(
        context: &Context,
        index: &[tars_core::compact::HistoryRecordEntry],
    ) -> Self {
        let blocks = context.blocks();
        let fixed_end = tars_core::compact::fixed_prefix_end(blocks);
        let fixed_prefix = &blocks[..fixed_end];

        let inserted = index
            .iter()
            .flat_map(|entry| entry.inserted_block_ids.iter().copied())
            .collect::<std::collections::HashSet<_>>();

        let history_run_len = if index.is_empty() {
            0
        } else {
            let candidates = &blocks[fixed_end..];
            let mut len = 0;
            while len < candidates.len() {
                let block = &candidates[len];
                if inserted.contains(&block.id()) {
                    len += 1;
                } else if matches!(block.content(), BlockContent::ToolChange(_)) {
                    // A ToolChange that no index entry inserted is part of the run
                    // only if a brief comes after it in the run; otherwise the run ends before it.
                    let has_following_brief = {
                        let mut found = false;
                        for future in &candidates[len + 1..] {
                            if future.meta().source.as_deref()
                                == Some(tars_core::compact::SUMMARY_SOURCE)
                            {
                                found = true;
                                break;
                            }
                            if !inserted.contains(&future.id())
                                && !matches!(future.content(), BlockContent::ToolChange(_))
                            {
                                break;
                            }
                        }
                        found
                    };
                    if has_following_brief {
                        len += 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            len
        };

        let history_run = &blocks[fixed_end..fixed_end + history_run_len];
        let current_part = &blocks[fixed_end + history_run_len..];

        // 1. Fixed prefix (leading system text blocks)
        let mut head = Self::build(fixed_prefix, true);

        // 2. The brief chain in history_run: one folded row per brief, and collect their bodies into summary
        for block in history_run {
            if block.meta().source.as_deref() == Some(tars_core::compact::SUMMARY_SOURCE) {
                if let BlockContent::Message { parts, .. } = block.content() {
                    let mut body = String::new();
                    for part in parts {
                        if let ContentPart::Text(value) = part {
                            body.push_str(&value.0);
                        }
                    }
                    if let Some(existing) = &mut head.summary {
                        existing.push_str("\n\n");
                        existing.push_str(&body);
                    } else {
                        head.summary = Some(body);
                    }
                }
                head.text
                    .push_str("● Context summary · collapsed · /summary shows the full text\n\n");
            }
            // Put-back U, stripped replies, and ToolChange in history_run are NOT rendered in current material:
            // the history expansion shows their originals.
        }

        // 3. Current part (blocks after history_run)
        let tail = Self::build(current_part, true);
        head.text.push_str(&tail.text);
        head.tool_details.extend(tail.tool_details);
        if let Some(tail_summary) = tail.summary {
            if let Some(existing) = &mut head.summary {
                existing.push_str("\n\n");
                existing.push_str(&tail_summary);
            } else {
                head.summary = Some(tail_summary);
            }
        }
        head.history_range = fixed_end..fixed_end + history_run_len;
        head
    }

    /// Archived conversation from history records keeps any summary expanded:
    /// it is not the material the model continues from.
    pub fn from_history_blocks(blocks: &[ContextBlock]) -> Self {
        Self::build(blocks, false)
    }

    fn build(blocks: &[ContextBlock], collapse_summary: bool) -> Self {
        let mut text = String::new();
        let mut tool_details = Vec::new();
        let mut summary_text: Option<String> = None;
        for block in blocks {
            // The offered tool set is configuration the runner records, not
            // conversation; it has no transcript entry.
            if matches!(block.content(), BlockContent::ToolChange(_)) {
                continue;
            }
            let is_summary =
                block.meta().source.as_deref() == Some(tars_core::compact::SUMMARY_SOURCE);
            if is_summary && let BlockContent::Message { parts, .. } = block.content() {
                let mut body = String::new();
                for part in parts {
                    if let ContentPart::Text(value) = part {
                        body.push_str(&value.0);
                    }
                }
                if let Some(existing) = &mut summary_text {
                    existing.push_str("\n\n");
                    existing.push_str(&body);
                } else {
                    summary_text = Some(body);
                }
                if collapse_summary {
                    text.push_str(
                        "● Context summary · collapsed · /summary shows the full text\n\n",
                    );
                    continue;
                }
            }
            match block.meta().source.as_deref() {
                Some("user") => text.push_str("❯ You\n"),
                Some(_) if is_summary => text.push_str("● Context summary\n"),
                Some(source) => {
                    text.push_str("● ");
                    text.push_str(source);
                    text.push('\n');
                }
                None => text.push_str("●\n"),
            }
            match block.content() {
                BlockContent::Message { parts, .. } => {
                    for part in parts {
                        match part {
                            ContentPart::Text(value) => text.push_str(&value.0),
                            ContentPart::Media(_) => text.push_str("[media reference]"),
                            ContentPart::Sealed(_) => {}
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
                BlockContent::ToolChange(_) => {}
            }
            text.push_str("\n\n");
        }
        // Provider text is content, never a stream of terminal control commands.
        // The same escaping as confirmation panels keeps bidi controls and line
        // separators visible without changing ordinary text or newlines.
        let text = tars_core::permission::escape_untrusted_text(&text).replace('\t', "    ");
        Self {
            text,
            tool_details,
            summary: summary_text,
            history_range: 0..0,
        }
    }
}

pub(crate) fn display(value: &str) -> String {
    tars_core::permission::escape_untrusted_text(value).replace('\t', "    ")
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

#[cfg(test)]
mod tests {
    use super::Transcript;
    use causa::kernel::{BlockContent, Context, ContextBlock, Role};

    #[test]
    fn untrusted_text_escapes_controls_bidi_and_separators_but_keeps_unicode() {
        let raw = "中文 😀\nline\u{202e}\u{2066}\u{2028}\ttabbed";
        let context = Context::from_blocks(vec![ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, raw),
            Default::default(),
        )])
        .unwrap();
        let transcript = Transcript::from_materials(&context, &[]);
        assert!(transcript.text.contains("中文 😀"));
        assert!(transcript.text.contains('\n'));
        assert!(transcript.text.contains("\\u{202e}"));
        assert!(transcript.text.contains("\\u{2066}"));
        assert!(transcript.text.contains("\\u{2028}"));
        assert!(!transcript.text.contains('\u{202e}'));
        assert!(!transcript.text.contains('\u{2066}'));
        assert!(!transcript.text.contains('\u{2028}'));
        assert!(transcript.text.contains("    tabbed"));
    }

    #[test]
    fn tool_change_without_following_brief_is_not_part_of_history_run() {
        use causa::kernel::{BlockMeta, ContentPart, TextPayload, ToolChange, ToolDefinition};
        use tars_core::compact::{HistoryRecordEntry, HistoryTrigger, RewriteKind};

        let brief = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPayload::new("brief text"))],
            },
            BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
        );

        let uninserted_tool_change = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolChange(ToolChange::between(
                &[],
                &[ToolDefinition::new(
                    "custom_tool",
                    "description",
                    serde_json::json!({}),
                )],
            )),
            Default::default(),
        );

        let stripped_reply = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPayload::new("stripped reply text"))],
            },
            Default::default(),
        );

        let user_block = ContextBlock::text(
            causa::runtime::new_block_id(),
            Role::User,
            "tail user message",
        );

        let entry = HistoryRecordEntry {
            record_id: "0190a000-0000-7000-8000-000000000001".into(),
            created_at_ms: 1,
            trigger: HistoryTrigger::Auto,
            kind: RewriteKind::Summarize,
            removed_block_count: 2,
            inserted_block_ids: vec![brief.id(), stripped_reply.id()],
        };

        // Case 1: [brief, uninserted_tool_change, stripped_reply, user_block]
        // The uninserted ToolChange has NO brief following it in the run.
        // Rule: the run must end before it! So history_range is 0..1 (brief only).
        let context1 = Context::from_blocks(vec![
            brief.clone(),
            uninserted_tool_change.clone(),
            stripped_reply.clone(),
            user_block.clone(),
        ])
        .unwrap();
        let transcript1 = Transcript::from_materials(&context1, std::slice::from_ref(&entry));
        assert_eq!(transcript1.history_range, 0..1);
        assert!(transcript1.text.contains("stripped reply text"));
        assert!(transcript1.text.contains("tail user message"));

        // Case 2: [uninserted_tool_change, brief, stripped_reply, user_block]
        // The uninserted ToolChange DOES have a brief following it in the run.
        // Rule: it is part of the run! So history_range is 0..3.
        let context2 = Context::from_blocks(vec![
            uninserted_tool_change,
            brief,
            stripped_reply,
            user_block,
        ])
        .unwrap();
        let transcript2 = Transcript::from_materials(&context2, &[entry]);
        assert_eq!(transcript2.history_range, 0..3);
        assert!(!transcript2.text.contains("stripped reply text"));
        assert!(transcript2.text.contains("tail user message"));
    }
}
