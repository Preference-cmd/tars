use causa::kernel::{BlockContent, ContentPart, Context, ContextBlock};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Formatted tool JSON body together with structural line offsets for any
/// expanded unified diff section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FormattedToolJson {
    pub text: String,
    pub diff_header_line: Option<usize>,
    pub diff_lines: Option<Range<usize>>,
}

/// Structural per-item line metadata for [`InfoKind::Tools`](crate::app::InfoKind::Tools)
/// so untrusted tool output can never spoof or break diff/header styling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ToolDetailMeta {
    pub header_line: Option<usize>,
    pub diff_header_lines: Vec<usize>,
    pub diff_ranges: Vec<Range<usize>>,
    /// `(line_index, is_warning)` for status/boundary notes (`Yellow` when `true`, `MUTED` when `false`).
    pub note_lines: Vec<(usize, bool)>,
}

/// Metadata for a committed tool result whose full output spilled to `.artifacts/`.
#[derive(Clone, Debug)]
pub(crate) struct SpilledToolDetail {
    pub call_block_id: causa::kernel::BlockId,
    pub status: causa::kernel::ToolResultStatus,
    pub artifact_id: String,
    pub size_bytes: usize,
    pub preview: FormattedToolJson,
}

/// Logical transcript segment controlling whether Markdown parsing applies.
///
/// Only assistant-authored message bodies (including expanded context summary
/// briefs) use [`TranscriptSegment::MarkdownBody`]; user input, system prompts,
/// tool summaries, expanded raw XML blocks, and folded/status notices remain
/// unparsed plain text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptSegment {
    UserHeader,
    RoleHeader(String),
    MutedNotice(String),
    MarkdownBody(String),
    PlainBody(String),
}

#[derive(Default)]
pub(crate) struct Transcript {
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    pub tool_details: Vec<String>,
    pub tool_detail_meta: Vec<ToolDetailMeta>,
    pub tool_artifacts: Vec<Option<SpilledToolDetail>>,
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
            .filter(|entry| entry.kind != tars_core::compact::RewriteKind::MicroCompact)
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

        // 1. Fixed prefix (leading system text blocks and initial dynamic prelude blocks)
        let mut head = Self::build(fixed_prefix, true, true);

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
                let notice = "● Context summary · collapsed · /summary shows the full text";
                head.text.push_str(notice);
                head.text.push_str("\n\n");
                head.segments
                    .push(TranscriptSegment::MutedNotice(notice.to_owned()));
            }
            // Put-back U, stripped replies, and ToolChange in history_run are NOT rendered in current material:
            // the history expansion shows their originals.
        }

        // 3. Current part (blocks after history_run)
        let tail = Self::build(current_part, true, true);
        head.text.push_str(&tail.text);
        head.segments.extend(tail.segments);
        head.tool_details.extend(tail.tool_details);
        head.tool_detail_meta.extend(tail.tool_detail_meta);
        head.tool_artifacts.extend(tail.tool_artifacts);
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

    /// Archived conversation from history records keeps any summary and prelude expanded:
    /// it is not the material the model continues from.
    pub fn from_history_blocks(blocks: &[ContextBlock]) -> Self {
        Self::build(blocks, false, false)
    }

    /// Append an accepted user submission to the live transcript while the turn runs.
    ///
    /// Replaced by the authoritative `Transcript::from_materials` snapshot once the
    /// turn's material read lands.
    pub(crate) fn with_appended_user_echo(
        &self,
        text: Option<&str>,
        media: &[causa::kernel::MediaRef],
    ) -> Self {
        let mut next_text = self.text.clone();
        if !next_text.is_empty() && !next_text.ends_with("\n\n") {
            if !next_text.ends_with('\n') {
                next_text.push('\n');
            }
            next_text.push('\n');
        }
        let mut next_segments = self.segments.clone();
        next_text.push_str("❯ You\n");
        next_segments.push(TranscriptSegment::UserHeader);
        let mut body = String::new();
        if let Some(t) = text.filter(|v| !v.trim().is_empty()) {
            next_text.push_str(t);
            body.push_str(t);
        }
        for item in media {
            let badge = format_media_badge(item);
            if !body.is_empty() && !body.ends_with('\n') {
                next_text.push('\n');
                body.push('\n');
            }
            next_text.push_str(&badge);
            body.push_str(&badge);
        }
        next_text.push_str("\n\n");
        if !body.is_empty() {
            next_segments.push(TranscriptSegment::PlainBody(display(&body)));
        }
        Self {
            text: tars_core::permission::escape_untrusted_text(&next_text).replace('\t', "    "),
            segments: next_segments,
            tool_details: self.tool_details.clone(),
            tool_detail_meta: self.tool_detail_meta.clone(),
            tool_artifacts: self.tool_artifacts.clone(),
            summary: self.summary.clone(),
            history_range: self.history_range.clone(),
        }
    }

    fn build(blocks: &[ContextBlock], collapse_summary: bool, collapse_prelude: bool) -> Self {
        let mut text = String::new();
        let mut segments = Vec::new();
        let mut tool_details = Vec::new();
        let mut tool_detail_meta = Vec::new();
        let mut tool_artifacts = Vec::new();
        let mut summary_text: Option<String> = None;
        let mut index = 0;
        while index < blocks.len() {
            let block = &blocks[index];
            // The offered tool set is configuration the runner records, not
            // conversation; it has no transcript entry.
            if matches!(block.content(), BlockContent::ToolChange(_)) {
                index += 1;
                continue;
            }
            if collapse_prelude
                && block
                    .meta()
                    .source
                    .as_deref()
                    .is_some_and(tars_core::prelude::is_prelude_prefix_source)
            {
                let mut count = 0usize;
                while index < blocks.len() {
                    let candidate = &blocks[index];
                    if matches!(candidate.content(), BlockContent::ToolChange(_)) {
                        index += 1;
                        continue;
                    }
                    if candidate
                        .meta()
                        .source
                        .as_deref()
                        .is_some_and(tars_core::prelude::is_prelude_prefix_source)
                    {
                        count += 1;
                        index += 1;
                    } else {
                        break;
                    }
                }
                let unit = if count == 1 { "block" } else { "blocks" };
                let notice = format!("● Session prelude · {count} {unit} · folded");
                text.push_str(&notice);
                text.push_str("\n\n");
                segments.push(TranscriptSegment::MutedNotice(notice));
                continue;
            }
            if collapse_prelude
                && let Some(source) = block.meta().source.as_deref()
                && tars_core::prelude::is_prelude_update_source(source)
            {
                let section = source
                    .strip_prefix(tars_core::prelude::PRELUDE_UPDATE_PREFIX)
                    .unwrap_or(source);
                let notice = format!("● Prelude updated ({section}) · folded");
                text.push_str(&notice);
                text.push_str("\n\n");
                segments.push(TranscriptSegment::MutedNotice(display(&notice)));
                index += 1;
                continue;
            }
            if collapse_prelude
                && block.meta().source.as_deref() == Some(tars_core::skills::SKILL_SOURCE)
                && matches!(
                    block.content(),
                    BlockContent::Message {
                        role: causa::kernel::Role::User,
                        ..
                    }
                )
            {
                let notice = match tars_core::skills::skill_block_name(block) {
                    Some(name) => format!("● Skill preserved ({name}) · folded"),
                    None => "● Skill preserved · folded".to_owned(),
                };
                text.push_str(&notice);
                text.push_str("\n\n");
                segments.push(TranscriptSegment::MutedNotice(display(&notice)));
                index += 1;
                continue;
            }
            if collapse_prelude
                && block.meta().source.as_deref()
                    == Some(tars_core::events::TASK_NOTIFICATION_SOURCE)
                && let BlockContent::Message { parts, .. } = block.content()
            {
                let mut raw = String::new();
                for part in parts {
                    if let ContentPart::Text(value) = part {
                        raw.push_str(&value.0);
                    }
                }
                for notice in format_task_notifications_compact(&raw) {
                    text.push_str(&notice);
                    text.push_str("\n\n");
                    segments.push(TranscriptSegment::MutedNotice(display(&notice)));
                }
                index += 1;
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
                    let notice = "● Context summary · collapsed · /summary shows the full text";
                    text.push_str(notice);
                    text.push_str("\n\n");
                    segments.push(TranscriptSegment::MutedNotice(notice.to_owned()));
                    index += 1;
                    continue;
                }
            }
            match block.meta().source.as_deref() {
                Some("user") => {
                    text.push_str("❯ You\n");
                    segments.push(TranscriptSegment::UserHeader);
                }
                Some(_) if is_summary => {
                    text.push_str("● Context summary\n");
                    segments.push(TranscriptSegment::RoleHeader("● Context summary".into()));
                }
                Some(source) if tars_core::prelude::is_prelude_prefix_source(source) => {
                    let section = source
                        .strip_prefix(tars_core::prelude::PRELUDE_SOURCE_PREFIX)
                        .unwrap_or(source);
                    let header = format!("● Session prelude ({section}) · {source}");
                    text.push_str(&header);
                    text.push('\n');
                    segments.push(TranscriptSegment::RoleHeader(display(&header)));
                }
                Some(source) if tars_core::prelude::is_prelude_update_source(source) => {
                    let section = source
                        .strip_prefix(tars_core::prelude::PRELUDE_UPDATE_PREFIX)
                        .unwrap_or(source);
                    let header = format!("● Prelude update ({section}) · {source}");
                    text.push_str(&header);
                    text.push('\n');
                    segments.push(TranscriptSegment::RoleHeader(display(&header)));
                }
                Some(source) if source == tars_core::skills::SKILL_SOURCE => {
                    let header = if let Some(name) = tars_core::skills::skill_block_name(block) {
                        format!("● Skill ({name}) · {source}")
                    } else {
                        format!("● Skill · {source}")
                    };
                    text.push_str(&header);
                    text.push('\n');
                    segments.push(TranscriptSegment::RoleHeader(display(&header)));
                }
                Some(source) if source == tars_core::events::TASK_NOTIFICATION_SOURCE => {
                    let header = format!("● Task notification · {source}");
                    text.push_str(&header);
                    text.push('\n');
                    segments.push(TranscriptSegment::RoleHeader(display(&header)));
                }
                Some(source) => {
                    let header = format!("● {source}");
                    text.push_str(&header);
                    text.push('\n');
                    segments.push(TranscriptSegment::RoleHeader(display(&header)));
                }
                None => {
                    text.push_str("●\n");
                    segments.push(TranscriptSegment::RoleHeader("●".into()));
                }
            }
            match block.content() {
                BlockContent::Message { role, parts } => {
                    let is_markdown = *role == causa::kernel::Role::Assistant || is_summary;
                    if is_markdown {
                        let mut md_buf = String::new();
                        let mut last_was_media = false;
                        for part in parts {
                            match part {
                                ContentPart::Text(value) => {
                                    if last_was_media
                                        && !value.0.starts_with('\n')
                                        && !text.ends_with('\n')
                                    {
                                        text.push('\n');
                                    }
                                    last_was_media = false;
                                    text.push_str(&value.0);
                                    md_buf.push_str(&value.0);
                                }
                                ContentPart::Media(media) => {
                                    let badge = format_media_badge(media);
                                    if !text.is_empty() && !text.ends_with('\n') {
                                        text.push('\n');
                                    }
                                    text.push_str(&badge);
                                    if !md_buf.is_empty() {
                                        segments.push(TranscriptSegment::MarkdownBody(display(
                                            &std::mem::take(&mut md_buf),
                                        )));
                                    }
                                    segments.push(TranscriptSegment::PlainBody(badge));
                                    last_was_media = true;
                                }
                                ContentPart::Sealed(_) => {}
                            }
                        }
                        if !md_buf.is_empty() {
                            segments.push(TranscriptSegment::MarkdownBody(display(&md_buf)));
                        }
                    } else {
                        let mut plain_buf = String::new();
                        let mut last_was_media = false;
                        for part in parts {
                            match part {
                                ContentPart::Text(value) => {
                                    if last_was_media && !value.0.starts_with('\n') {
                                        text.push('\n');
                                        plain_buf.push('\n');
                                    }
                                    last_was_media = false;
                                    text.push_str(&value.0);
                                    plain_buf.push_str(&value.0);
                                }
                                ContentPart::Media(media) => {
                                    let badge = format_media_badge(media);
                                    if !plain_buf.is_empty() && !plain_buf.ends_with('\n') {
                                        text.push('\n');
                                        plain_buf.push('\n');
                                    }
                                    text.push_str(&badge);
                                    plain_buf.push_str(&badge);
                                    last_was_media = true;
                                }
                                ContentPart::Sealed(_) => {}
                            }
                        }
                        if !plain_buf.is_empty() {
                            segments.push(TranscriptSegment::PlainBody(display(&plain_buf)));
                        }
                    }
                }
                BlockContent::ToolCall(call) => {
                    let tool_name = display_single_line(&call.tool_name);
                    let body = format!(
                        "Tool {tool_name} · recorded declaration · {}\n/tools expands actual material",
                        block.id().0
                    );
                    text.push_str(&body);
                    segments.push(TranscriptSegment::PlainBody(body));
                    let mut builder = ToolDetailBuilder::new();
                    builder.push_header(&format!("Declaration {} · {tool_name}", block.id().0));
                    builder.push_plain(&display(
                        &serde_json::to_string_pretty(&call.arguments).unwrap_or_default(),
                    ));
                    let (detail_text, detail_meta) = builder.finish();
                    tool_details.push(detail_text);
                    tool_detail_meta.push(detail_meta);
                    tool_artifacts.push(None);
                }
                BlockContent::ToolResult(result) => {
                    let preview_summary = tool_result_summary(&result.output.content);
                    let preview = format_tool_result_payload(result);
                    let media_section = if result.media.is_empty() {
                        String::new()
                    } else {
                        let badges: Vec<String> =
                            result.media.iter().map(format_media_badge).collect();
                        format!("\n{}", badges.join("\n"))
                    };
                    if let Some(artifact) = &result.output.artifact {
                        let artifact_id = display_single_line(&artifact.id);
                        let kind_label = if artifact.id.starts_with("artifact:") {
                            "spilled preview"
                        } else {
                            "cleared preview"
                        };
                        let body = format!(
                            "Tool result · {:?} · {} · {kind_label} ({artifact_id})\n{}{media_section}\n/tools expands full output",
                            result.status, result.call_block_id.0, preview_summary
                        );
                        text.push_str(&body);
                        segments.push(TranscriptSegment::PlainBody(body));
                        let mut builder = ToolDetailBuilder::new();
                        builder.push_header(&format!(
                            "Result for {} · {:?} · {artifact_id}",
                            result.call_block_id.0, result.status
                        ));
                        builder.push_formatted_json(&preview);
                        let (detail_text, detail_meta) = builder.finish();
                        tool_details.push(detail_text);
                        tool_detail_meta.push(detail_meta);
                        tool_artifacts.push(Some(SpilledToolDetail {
                            call_block_id: result.call_block_id,
                            status: result.status.clone(),
                            artifact_id: artifact.id.clone(),
                            size_bytes: artifact.size_bytes,
                            preview,
                        }));
                    } else {
                        let body = format!(
                            "Tool result · {:?} · {}\n{}{media_section}\n/tools expands actual material",
                            result.status, result.call_block_id.0, preview_summary
                        );
                        text.push_str(&body);
                        segments.push(TranscriptSegment::PlainBody(body));
                        let mut builder = ToolDetailBuilder::new();
                        builder.push_header(&format!(
                            "Result for {} · {:?}",
                            result.call_block_id.0, result.status
                        ));
                        builder.push_formatted_json(&preview);
                        let (detail_text, detail_meta) = builder.finish();
                        tool_details.push(detail_text);
                        tool_detail_meta.push(detail_meta);
                        tool_artifacts.push(None);
                    }
                }
                BlockContent::ToolChange(_) => {}
            }
            text.push_str("\n\n");
            index += 1;
        }
        // Provider text is content, never a stream of terminal control commands.
        // The same escaping as confirmation panels keeps bidi controls and line
        // separators visible without changing ordinary text or newlines.
        let text = tars_core::permission::escape_untrusted_text(&text).replace('\t', "    ");
        Self {
            text,
            segments,
            tool_details,
            tool_detail_meta,
            tool_artifacts,
            summary: summary_text,
            history_range: 0..0,
        }
    }
}

fn tool_result_summary(content: &serde_json::Value) -> String {
    if let Some(stats) = content.get("stats").and_then(|s| s.as_object())
        && let (Some(subcalls), Some(max_in_flight), Some(duration_ms)) = (
            stats.get("subcalls").and_then(|v| v.as_u64()),
            stats.get("max_in_flight").and_then(|v| v.as_u64()),
            stats.get("duration_ms").and_then(|v| v.as_u64()),
        )
    {
        let body = if let Some(err) = content.get("error").and_then(|v| v.as_str()) {
            format!("error: {err}")
        } else if let Some(out) = content.get("output").and_then(|v| v.as_str()) {
            out.to_owned()
        } else {
            serde_json::to_string_pretty(content).unwrap_or_default()
        };
        return summary(&format!(
            "subcalls={subcalls} · max_in_flight={max_in_flight} · {duration_ms}ms · {body}"
        ));
    }
    let body = serde_json::to_string_pretty(content).unwrap_or_default();
    summary(&body)
}

pub(crate) fn display(value: &str) -> String {
    tars_core::permission::escape_untrusted_text(value).replace('\t', "    ")
}

/// Escape untrusted text and collapse line breaks into spaces for single-line headers/labels.
pub(crate) fn display_single_line(value: &str) -> String {
    display(value).replace('\n', " ")
}

fn short_media_reference(raw: &str) -> String {
    let clean = display_single_line(raw);
    if clean.starts_with("data:") {
        return "data:<redacted>".to_owned();
    }
    if let Some(hex) = raw.strip_prefix(tars_core::media::MEDIA_REF_PREFIX) {
        let clean_hex = display_single_line(hex);
        return format!("media:sha256:{}", truncate_chars(&clean_hex, 12));
    }
    truncate_chars(&clean, 48)
}

/// Render a sanitized, single-line media badge without raw bytes or terminal escapes.
pub(crate) fn format_media_badge(media: &causa::kernel::MediaRef) -> String {
    let media_type = truncate_chars(&display_single_line(&media.media_type), 48);
    let reference = short_media_reference(&media.reference);
    match media
        .name
        .as_deref()
        .map(|name| truncate_chars(&display_single_line(name), 64))
        .filter(|name| !name.trim().is_empty())
    {
        Some(name) => format!("[media {media_type} {name} ({reference})]"),
        None => format!("[media {media_type} ({reference})]"),
    }
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

/// Structural builder for `/tools` detail items so line indices in [`ToolDetailMeta`]
/// match the formatted item text 1:1 without string-prefix heuristics.
pub(crate) struct ToolDetailBuilder {
    text: String,
    line_count: usize,
    meta: ToolDetailMeta,
}

impl ToolDetailBuilder {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            line_count: 0,
            meta: ToolDetailMeta::default(),
        }
    }

    fn push_block(&mut self, block: &str) -> Range<usize> {
        let start = self.line_count;
        let lines_in_block = block.split('\n').count();
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(block);
        self.line_count += lines_in_block;
        start..self.line_count
    }

    pub(crate) fn push_header(&mut self, header: &str) {
        let clean = display_single_line(header);
        let range = self.push_block(&clean);
        self.meta.header_line = Some(range.start);
    }

    pub(crate) fn push_note(&mut self, note: &str, is_warning: bool) {
        let clean = display_single_line(note);
        let range = self.push_block(&clean);
        self.meta.note_lines.push((range.start, is_warning));
    }

    pub(crate) fn push_blank(&mut self) {
        self.push_block("");
    }

    pub(crate) fn push_plain(&mut self, body: &str) {
        self.push_block(body);
    }

    pub(crate) fn push_formatted_json(&mut self, formatted: &FormattedToolJson) {
        let range = self.push_block(&formatted.text);
        if let Some(rel_hdr) = formatted.diff_header_line {
            self.meta.diff_header_lines.push(range.start + rel_hdr);
        }
        if let Some(rel_diff) = &formatted.diff_lines {
            self.meta
                .diff_ranges
                .push((range.start + rel_diff.start)..(range.start + rel_diff.end));
        }
    }

    pub(crate) fn finish(self) -> (String, ToolDetailMeta) {
        (self.text, self.meta)
    }
}

/// Return `true` when `value` (at top level or under `output.content`) contains
/// a non-empty string `"diff"` field.
pub(crate) fn has_non_empty_diff(value: &serde_json::Value) -> bool {
    extract_diff_target(value).is_some()
}

fn extract_diff_target(value: &serde_json::Value) -> Option<(Option<&str>, &str)> {
    if let Some(obj) = value.as_object()
        && let Some(diff) = obj.get("diff").and_then(|v| v.as_str())
        && !diff.is_empty()
    {
        let path = obj
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty());
        return Some((path, diff));
    }
    if let Some(content) = value
        .get("output")
        .and_then(|o| o.get("content"))
        .and_then(|c| c.as_object())
        && let Some(diff) = content.get("diff").and_then(|v| v.as_str())
        && !diff.is_empty()
    {
        let path = content
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.trim().is_empty());
        return Some((path, diff));
    }
    None
}

/// Format a tool result JSON value, expanding any embedded `"diff"` string field
/// into readable multi-line unified diff text while recording exact diff line
/// offsets and keeping metadata readable.
pub(crate) fn format_tool_output_json(value: &serde_json::Value) -> FormattedToolJson {
    let Some((path, diff)) = extract_diff_target(value) else {
        return FormattedToolJson {
            text: display(&serde_json::to_string_pretty(value).unwrap_or_default()),
            diff_header_line: None,
            diff_lines: None,
        };
    };
    let path_owned = path.map(str::to_owned);
    let stripped = diff
        .strip_suffix("\r\n")
        .or_else(|| diff.strip_suffix('\n'))
        .unwrap_or(diff);
    let normalized = if stripped.contains("\r\n") {
        stripped.replace("\r\n", "\n")
    } else {
        stripped.to_owned()
    };
    let mut cloned = value.clone();
    if let Some(obj) = cloned.as_object_mut()
        && obj.get("diff").and_then(|v| v.as_str()).is_some()
    {
        obj.remove("diff");
    } else if let Some(content) = cloned
        .get_mut("output")
        .and_then(|o| o.get_mut("content"))
        .and_then(|c| c.as_object_mut())
    {
        content.remove("diff");
    }
    let meta = display(&serde_json::to_string_pretty(&cloned).unwrap_or_default());
    let meta_line_count = meta.split('\n').count();
    let header = match path_owned {
        Some(path) => format!("Diff ({}):", display_single_line(&path)),
        None => "Diff:".to_owned(),
    };
    let diff_body = display(&normalized);
    let diff_line_count = diff_body.split('\n').count();
    let diff_header_line = meta_line_count;
    let diff_start = diff_header_line + 1;
    FormattedToolJson {
        text: format!("{meta}\n{header}\n{diff_body}"),
        diff_header_line: Some(diff_header_line),
        diff_lines: Some(diff_start..(diff_start + diff_line_count)),
    }
}

fn format_tool_result_payload(result: &causa::kernel::ToolResultPayload) -> FormattedToolJson {
    match serde_json::to_value(result) {
        Ok(value) => format_tool_output_json(&value),
        Err(_) => FormattedToolJson {
            text: display(&serde_json::to_string_pretty(result).unwrap_or_default()),
            diff_header_line: None,
            diff_lines: None,
        },
    }
}

fn format_task_notifications_compact(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = raw;
    let mut found_envelope = false;
    while let Some(start) = rest.find("<task-notification") {
        let after_tag = &rest[start + "<task-notification".len()..];
        let Some(open_end) = after_tag.find('>') else {
            break;
        };
        let attrs = &after_tag[..open_end];
        let content_start = &after_tag[open_end + 1..];
        let (body, next_rest) = match content_start.find("</task-notification>") {
            Some(close_idx) => (
                &content_start[..close_idx],
                &content_start[close_idx + "</task-notification>".len()..],
            ),
            None => (content_start, ""),
        };
        found_envelope = true;
        out.push(format_compact_task_notification(attrs, body));
        rest = next_rest;
    }
    if !found_envelope {
        let mut entry = String::from("● Task notification");
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            entry.push_str("\n  ");
            entry.push_str(&summary(trimmed));
        }
        out.push(entry);
    }
    out
}

fn format_compact_task_notification(attrs: &str, body: &str) -> String {
    let mut out = String::new();
    let agent = xml_attr(attrs, "agent_name")
        .or_else(|| xml_attr(attrs, "agent"))
        .or_else(|| xml_tag(body, "agent_name"))
        .or_else(|| xml_tag(body, "agent"))
        .or_else(|| xml_attr(attrs, "agent_id"))
        .or_else(|| xml_attr(attrs, "source_id"))
        .or_else(|| xml_attr(attrs, "source"));
    let status = xml_attr(attrs, "status").or_else(|| xml_tag(body, "status"));
    let operation_id = xml_attr(attrs, "operation_id")
        .or_else(|| xml_attr(attrs, "operation-id"))
        .or_else(|| xml_tag(body, "operation-id"))
        .or_else(|| xml_tag(body, "operation_id"));

    let agent_label = agent
        .map(|v| display(&v).replace('\n', " "))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "task".to_owned());
    let status_part = status
        .map(|v| display(&v).replace('\n', " "))
        .filter(|v| !v.trim().is_empty())
        .map(|v| format!(" ({v})"))
        .unwrap_or_default();
    let op_part = operation_id
        .map(|v| display(&v).replace('\n', " "))
        .filter(|v| !v.trim().is_empty())
        .map(|v| format!(" · op {}", truncate_chars(&v, 32)))
        .unwrap_or_default();

    out.push_str(&format!(
        "● Task notification · {agent_label}{status_part}{op_part}"
    ));

    let snippet = xml_tag(body, "summary")
        .or_else(|| xml_tag(body, "result"))
        .or_else(|| {
            let trimmed = body.trim();
            (!trimmed.is_empty()).then(|| unescape_xml_entities(trimmed))
        });
    if let Some(snippet) = snippet {
        let preview = summary(snippet.trim());
        if !preview.is_empty() {
            out.push_str("\n  ");
            out.push_str(&preview);
        }
    }
    out
}

fn xml_attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(pos) = rest.find(name) {
        let before_ok = pos == 0 || rest.as_bytes()[pos - 1].is_ascii_whitespace();
        let after = &rest[pos + name.len()..];
        let trimmed = after.trim_start();
        if before_ok && let Some(after_eq) = trimmed.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if let Some(quote) = after_eq.chars().next().filter(|&c| c == '"' || c == '\'') {
                let value_start = &after_eq[quote.len_utf8()..];
                if let Some(end) = value_start.find(quote) {
                    return Some(unescape_xml_entities(&value_start[..end]));
                }
            }
        }
        rest = &rest[pos + name.len()..];
    }
    None
}

fn xml_tag(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)?;
    let content = &body[start + open.len()..];
    let end = content.find(&close)?;
    Some(unescape_xml_entities(content[..end].trim()))
}

fn unescape_xml_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after.find(';')
            && semi <= 6
        {
            let entity = &after[1..semi];
            let decoded = match entity {
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[amp + 1..];
    }
    out.push_str(rest);
    out
}

fn truncate_chars(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
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

/// Render `/tools` detail entries using structural [`ToolDetailMeta`] line ranges
/// (zero string-prefix heuristics) and wrap to `width` display columns so drawing
/// and scroll bounds match 1:1.
pub(crate) fn tools_detail_lines(
    items: &[String],
    meta: &[ToolDetailMeta],
    width: u16,
) -> Vec<Line<'static>> {
    use crate::render::{ACCENT, MUTED};
    let max_width = usize::from(width.max(1));
    let mut out = Vec::new();
    for (item_idx, item) in items.iter().enumerate() {
        let item_meta = meta.get(item_idx);
        for (line_idx, raw_line) in item.split('\n').enumerate() {
            let line = display(raw_line);
            let styled_line = if item_meta.is_some_and(|m| m.header_line == Some(line_idx)) {
                Line::from(vec![Span::styled(
                    line,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )])
            } else if item_meta.is_some_and(|m| m.diff_header_lines.contains(&line_idx)) {
                Line::from(vec![Span::styled(
                    line,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )])
            } else if let Some((_, is_warning)) = item_meta
                .and_then(|m| m.note_lines.iter().find(|(i, _)| *i == line_idx))
                .copied()
            {
                let color = if is_warning { Color::Yellow } else { MUTED };
                Line::from(vec![Span::styled(line, Style::default().fg(color))])
            } else if item_meta.is_some_and(|m| m.diff_ranges.iter().any(|r| r.contains(&line_idx)))
            {
                crate::markdown::highlight_diff_line(&line)
            } else {
                Line::raw(line)
            };
            out.extend(crate::markdown::wrap_styled_line(
                styled_line,
                max_width,
                &[],
            ));
        }
    }
    out
}

/// Render an [`InfoKind`](crate::app::InfoKind) panel body at `width` so
/// drawing and scroll-bound calculations stay 1:1.
pub(crate) fn info_panel_lines(
    kind: &crate::app::InfoKind,
    lines: &[String],
    width: u16,
) -> Vec<Line<'static>> {
    match kind {
        crate::app::InfoKind::Plain => detail_rows(lines, width)
            .map(|row| Line::from(row.to_owned()))
            .collect(),
        crate::app::InfoKind::Markdown => {
            crate::markdown::render_markdown(&lines.join("\n"), width)
        }
        crate::app::InfoKind::Tools(meta) => tools_detail_lines(lines, meta, width),
    }
}

/// Exact row count for an [`InfoKind`](crate::app::InfoKind) panel at `width`.
pub(crate) fn info_panel_row_count(
    kind: &crate::app::InfoKind,
    lines: &[String],
    width: u16,
) -> usize {
    match kind {
        crate::app::InfoKind::Plain => detail_rows(lines, width).count(),
        crate::app::InfoKind::Markdown | crate::app::InfoKind::Tools(_) => {
            info_panel_lines(kind, lines, width).len()
        }
    }
}

pub(crate) struct Rows {
    pub transcript: Arc<Transcript>,
    pub width: u16,
    pub lines: Vec<Line<'static>>,
}
impl Rows {
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn line(&self, index: usize) -> Option<&Line<'static>> {
        self.lines.get(index)
    }

    pub fn row_text(&self, index: usize) -> Option<String> {
        let line = self.lines.get(index)?;
        let mut out = String::new();
        for span in &line.spans {
            out.push_str(&span.content);
        }
        Some(out)
    }

    // Run on the blocking pool on new materials/width, never for every frame.
    pub fn wrap(transcript: Arc<Transcript>, width: u16) -> Self {
        let lines = if transcript.segments.is_empty() {
            if transcript.text.is_empty() {
                Vec::new()
            } else {
                wrap_legacy_text(&transcript.text, width)
            }
        } else {
            wrap_segments(&transcript.segments, width)
        };
        Self {
            transcript,
            width,
            lines,
        }
    }
}

fn wrap_segments(segments: &[TranscriptSegment], width: u16) -> Vec<Line<'static>> {
    use crate::render::{ACCENT, MUTED};
    let max_width = usize::from(width.max(1));
    let user_header_style = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
    let muted_style = Style::default().fg(MUTED);
    let mut out = Vec::new();

    for (idx, segment) in segments.iter().enumerate() {
        let next_is_body = matches!(
            segments.get(idx + 1),
            Some(TranscriptSegment::PlainBody(_) | TranscriptSegment::MarkdownBody(_))
        );
        match segment {
            TranscriptSegment::UserHeader => {
                out.extend(crate::markdown::wrap_styled_line(
                    Line::from(vec![Span::styled("❯ You", user_header_style)]),
                    max_width,
                    &[],
                ));
                if !next_is_body {
                    push_blank_separator(&mut out);
                }
            }
            TranscriptSegment::RoleHeader(header) => {
                for line in header.split('\n') {
                    out.extend(crate::markdown::wrap_styled_line(
                        Line::from(vec![Span::styled(line.to_owned(), muted_style)]),
                        max_width,
                        &[],
                    ));
                }
                if !next_is_body {
                    push_blank_separator(&mut out);
                }
            }
            TranscriptSegment::MutedNotice(notice) => {
                for line in notice.split('\n') {
                    out.extend(crate::markdown::wrap_styled_line(
                        Line::from(vec![Span::styled(line.to_owned(), muted_style)]),
                        max_width,
                        &[],
                    ));
                }
                push_blank_separator(&mut out);
            }
            TranscriptSegment::PlainBody(body) => {
                wrap_plain_body(body, max_width, &mut out);
                if !next_is_body {
                    push_blank_separator(&mut out);
                }
            }
            TranscriptSegment::MarkdownBody(body) => {
                out.extend(crate::markdown::render_markdown(body, width));
                if !next_is_body {
                    push_blank_separator(&mut out);
                }
            }
        }
    }
    out
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.is_empty() || line.spans.iter().all(|s| s.content.is_empty())
}

fn push_blank_separator(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() && !lines.last().is_some_and(is_blank_line) {
        lines.push(Line::default());
    }
}

fn wrap_plain_body(body: &str, max_width: usize, out: &mut Vec<Line<'static>>) {
    for raw_line in body.split('\n') {
        wrap_grapheme_line(raw_line, max_width, Style::default(), out);
    }
}

fn wrap_grapheme_line(line: &str, max_width: usize, style: Style, out: &mut Vec<Line<'static>>) {
    if line.is_empty() {
        out.push(Line::default());
        return;
    }
    let mut start = 0;
    let mut columns = 0;
    for (offset, grapheme) in line.grapheme_indices(true) {
        let cells = grapheme.width();
        if columns > 0 && columns + cells > max_width {
            out.push(Line::styled(line[start..offset].to_owned(), style));
            start = offset;
            columns = 0;
        }
        columns += cells;
    }
    out.push(Line::styled(line[start..].to_owned(), style));
}

fn wrap_legacy_text(text: &str, width: u16) -> Vec<Line<'static>> {
    let max_width = usize::from(width.max(1));
    let mut out = Vec::new();
    let mut start = 0;
    let mut columns = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            let slice = &text[start..offset];
            if slice.is_empty() {
                out.push(Line::default());
            } else {
                out.push(Line::styled(
                    slice.to_owned(),
                    legacy_conversation_style(slice),
                ));
            }
            start = offset + 1;
            columns = 0;
            continue;
        }
        let cells = grapheme.width();
        if columns > 0 && columns + cells > max_width {
            let slice = &text[start..offset];
            out.push(Line::styled(
                slice.to_owned(),
                legacy_conversation_style(slice),
            ));
            start = offset;
            columns = 0;
        }
        columns += cells;
    }
    let tail = &text[start..];
    if tail.is_empty() {
        out.push(Line::default());
    } else {
        out.push(Line::styled(
            tail.to_owned(),
            legacy_conversation_style(tail),
        ));
    }
    out
}

fn legacy_conversation_style(text: &str) -> Style {
    use crate::render::{ACCENT, MUTED};
    if text.starts_with("❯ ") {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else if text.starts_with("── ")
        || text.starts_with("↑ ")
        || text.starts_with("loading earlier conversation")
        || text.starts_with("this history record cannot be read")
        || text.starts_with("start of conversation")
        || text.starts_with("● Context summary")
        || text.starts_with("● Task notification")
    {
        Style::default().fg(MUTED)
    } else {
        Style::default()
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

    #[test]
    fn from_materials_folds_prelude_prefix_updates_and_preserved_skills() {
        use causa::kernel::{BlockMeta, ToolChange, ToolDefinition};
        use tars_core::prelude::{
            PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE, PRELUDE_STATIC_CUSTOM_SOURCE,
            PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
        };

        let base = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::System, "BASE_CONTRACT_HIDDEN"),
            BlockMeta::from_source(PRELUDE_BASE_SOURCE),
        );
        let static_custom = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::System, "STATIC_CUSTOM_HIDDEN"),
            BlockMeta::from_source(PRELUDE_STATIC_CUSTOM_SOURCE),
        );
        let ws_inst = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<workspace_instructions>\nAGENTS_SECRET_LINE\n</workspace_instructions>",
            ),
            BlockMeta::from_source(PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE),
        );
        let tool_change = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolChange(ToolChange::between(
                &[],
                &[ToolDefinition::new(
                    "read_file",
                    "read",
                    serde_json::json!({}),
                )],
            )),
            Default::default(),
        );
        let env = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "<env>\nENV_BODY_HIDDEN\n</env>"),
            BlockMeta::from_source(PRELUDE_ENV_SOURCE),
        );
        let user_msg = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "hello model"),
            BlockMeta::from_source("user"),
        );
        let env_update = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<env updated=\"true\">\nENV_UPDATED_HIDDEN\n</env>",
            ),
            BlockMeta::from_source("tars:prelude:update:env"),
        );
        let preserved_skill = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<skill_content name=\"my-skill\" sha256=\"deadbeef\">\nSKILL_BODY_HIDDEN\n</skill_content>",
            ),
            BlockMeta::from_source(tars_core::skills::SKILL_SOURCE),
        );
        let assistant_msg = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, "visible assistant reply"),
            BlockMeta::from_source("assistant"),
        );

        let context = Context::from_blocks(vec![
            base,
            static_custom,
            ws_inst,
            tool_change,
            env,
            user_msg,
            env_update,
            preserved_skill,
            assistant_msg,
        ])
        .unwrap();

        let transcript = Transcript::from_materials(&context, &[]);
        assert!(
            transcript
                .text
                .contains("● Session prelude · 4 blocks · folded"),
            "{}",
            transcript.text
        );
        assert!(
            transcript.text.contains("● Prelude updated (env) · folded"),
            "{}",
            transcript.text
        );
        assert!(
            transcript
                .text
                .contains("● Skill preserved (my-skill) · folded"),
            "{}",
            transcript.text
        );
        assert!(transcript.text.contains("❯ You\nhello model"));
        assert!(transcript.text.contains("visible assistant reply"));

        for hidden in [
            "BASE_CONTRACT_HIDDEN",
            "STATIC_CUSTOM_HIDDEN",
            "AGENTS_SECRET_LINE",
            "ENV_BODY_HIDDEN",
            "ENV_UPDATED_HIDDEN",
            "SKILL_BODY_HIDDEN",
        ] {
            assert!(
                !transcript.text.contains(hidden),
                "unexpectedly leaked {hidden}: {}",
                transcript.text
            );
        }
    }

    #[test]
    fn from_materials_keeps_non_prelude_system_blocks_verbatim() {
        use causa::kernel::BlockMeta;

        let plain_sys = ContextBlock::text(
            causa::runtime::new_block_id(),
            Role::System,
            "CUSTOM_SYSTEM_PROMPT",
        );
        let labeled_sys = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::System, "LABELED_SYSTEM_PROMPT"),
            BlockMeta::from_source("system"),
        );
        let user = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "user prompt"),
            BlockMeta::from_source("user"),
        );

        let context = Context::from_blocks(vec![plain_sys, labeled_sys, user]).unwrap();
        let transcript = Transcript::from_materials(&context, &[]);
        assert!(transcript.text.contains("CUSTOM_SYSTEM_PROMPT"));
        assert!(transcript.text.contains("LABELED_SYSTEM_PROMPT"));
        assert!(!transcript.text.contains("Session prelude"));
    }

    #[test]
    fn from_history_blocks_keeps_prelude_updates_and_skills_expanded() {
        use causa::kernel::BlockMeta;
        use tars_core::prelude::PRELUDE_BASE_SOURCE;

        let base = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::System, "HISTORY_BASE_BODY"),
            BlockMeta::from_source(PRELUDE_BASE_SOURCE),
        );
        let update = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<workspace_instructions updated=\"true\">\nHISTORY_UPDATE_BODY\n</workspace_instructions>",
            ),
            BlockMeta::from_source("tars:prelude:update:workspace-instructions"),
        );
        let skill = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<skill_content name=\"hist-skill\" sha256=\"abc\">\nHISTORY_SKILL_BODY\n</skill_content>",
            ),
            BlockMeta::from_source(tars_core::skills::SKILL_SOURCE),
        );

        let transcript = Transcript::from_history_blocks(&[base, update, skill]);
        assert!(
            transcript
                .text
                .contains("● Session prelude (base) · tars:prelude:base"),
            "{}",
            transcript.text
        );
        assert!(transcript.text.contains("HISTORY_BASE_BODY"));
        assert!(
            transcript
                .text
                .contains("● Prelude update (workspace-instructions) · tars:prelude:update:workspace-instructions"),
            "{}",
            transcript.text
        );
        assert!(transcript.text.contains("HISTORY_UPDATE_BODY"));
        assert!(
            transcript
                .text
                .contains("● Skill (hist-skill) · tars:skill"),
            "{}",
            transcript.text
        );
        assert!(transcript.text.contains("HISTORY_SKILL_BODY"));
        assert!(!transcript.text.contains("· folded"));
    }

    #[test]
    fn from_materials_compacts_task_notification_and_escapes_controls() {
        use causa::kernel::BlockMeta;

        let long_body = "x".repeat(300);
        let xml = format!(
            "<task-notification source=\"agent_operation\" id=\"7\" operation_id=\"op-12345\" agent_id=\"agent-2\" agent_name=\"bad\u{202e}\x1b[31magent\" status=\"completed\" priority=\"normal\">\n<summary>Finished\u{202e}\x1b[0m scan: {long_body}</summary>\n</task-notification>"
        );
        let block = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, &xml),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        let context = Context::from_blocks(vec![block]).unwrap();
        let compact = Transcript::from_materials(&context, &[]);

        assert!(
            compact.text.contains(
                "● Task notification · bad\\u{202e}\\x1b[31magent (completed) · op op-12345"
            ),
            "{}",
            compact.text
        );
        assert!(
            compact.text.contains("Finished\\u{202e}\\x1b[0m scan:"),
            "{}",
            compact.text
        );
        assert!(compact.text.contains('…'), "{}", compact.text);
        assert!(
            !compact.text.contains("<task-notification"),
            "raw XML envelope must be compacted: {}",
            compact.text
        );
        assert!(!compact.text.contains('\x1b'));
        assert!(!compact.text.contains('\u{202e}'));
    }

    #[test]
    fn from_history_blocks_keeps_task_notification_expanded() {
        use causa::kernel::BlockMeta;

        let xml = "<task-notification source=\"agent_operation\" id=\"9\" operation_id=\"op-9\" agent_id=\"a1\" agent_name=\"explore\" status=\"completed\" priority=\"normal\">\nfull archived body\n</task-notification>";
        let block = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, xml),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        let expanded = Transcript::from_history_blocks(&[block]);
        assert!(
            expanded
                .text
                .contains("● Task notification · tars:task-notification"),
            "{}",
            expanded.text
        );
        assert!(
            expanded.text.contains("full archived body"),
            "{}",
            expanded.text
        );
        assert!(
            expanded.text.contains("<task-notification"),
            "{}",
            expanded.text
        );
    }

    #[test]
    fn from_materials_compacts_malformed_task_notification_and_multiline_attrs() {
        use causa::kernel::BlockMeta;

        let malformed = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "plain fallback\nbody\x1b[31m"),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        let multiline_attr = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<task-notification agent_name=\"alpha\nbeta\" status=\"done\nnow\" operation_id=\"op\n1\">\n<summary>ok</summary>\n</task-notification>",
            ),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        let context = Context::from_blocks(vec![malformed, multiline_attr]).unwrap();
        let transcript = Transcript::from_materials(&context, &[]);

        assert!(
            transcript
                .text
                .contains("● Task notification\n  plain fallback body\\x1b[31m"),
            "{}",
            transcript.text
        );
        assert!(
            transcript
                .text
                .contains("● Task notification · alpha beta (done now) · op op 1\n  ok"),
            "{}",
            transcript.text
        );
    }

    #[test]
    fn from_materials_truncates_multibyte_utf8_task_notification_safely() {
        use causa::kernel::BlockMeta;

        let utf8_op = "操作-".repeat(12);
        let utf8_body = "中文测试内容🎉".repeat(30);
        let xml = format!(
            "<task-notification agent_name=\"测试代理\" status=\"completed\" operation_id=\"{utf8_op}\">\n{utf8_body}\n</task-notification>"
        );
        let block = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, &xml),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        let context = Context::from_blocks(vec![block]).unwrap();
        let transcript = Transcript::from_materials(&context, &[]);

        assert!(
            transcript
                .text
                .contains("● Task notification · 测试代理 (completed) · op "),
            "{}",
            transcript.text
        );
        assert!(transcript.text.contains('…'), "{}", transcript.text);
    }

    #[test]
    fn md05_segmented_isolation_parses_only_assistant_and_expanded_summary_bodies() {
        use super::{Rows, TranscriptSegment};
        use crate::render::{ACCENT, MUTED};
        use causa::kernel::{
            BlockMeta, ToolCallPayload, ToolOutput, ToolResultPayload, ToolResultStatus,
        };
        use ratatui::style::{Color, Modifier};
        use std::sync::Arc;

        let md_payload =
            "# Heading\n\n**bold**\n\n```rust\nlet x = 1;\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |";
        let sys = ContextBlock::text(
            causa::runtime::new_block_id(),
            Role::System,
            "# System Heading\n**sys_bold**",
        );
        let user = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, md_payload),
            BlockMeta::from_source("user"),
        );
        let call_id = causa::runtime::new_block_id();
        let tool_call = ContextBlock::new(
            call_id,
            BlockContent::ToolCall(ToolCallPayload::new(
                "write_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            Default::default(),
        );
        let tool_res = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                call_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({
                    "note": "# NotHeading **not_bold**"
                })),
            )),
            Default::default(),
        );
        let assistant = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, md_payload),
            BlockMeta::from_source("assistant"),
        );

        let context =
            Context::from_blocks(vec![sys, user, tool_call, tool_res, assistant]).unwrap();
        let transcript = Arc::new(Transcript::from_materials(&context, &[]));

        // Only the assistant block produced MarkdownBody; everything else is PlainBody/headers.
        let md_count = transcript
            .segments
            .iter()
            .filter(|s| matches!(s, TranscriptSegment::MarkdownBody(_)))
            .count();
        assert_eq!(md_count, 1, "{:?}", transcript.segments);
        assert!(transcript.segments.contains(&TranscriptSegment::UserHeader));
        assert!(
            transcript
                .segments
                .contains(&TranscriptSegment::PlainBody(md_payload.to_owned()))
        );

        let rows = Rows::wrap(transcript, 80);
        let all_row_text: Vec<String> =
            (0..rows.len()).map(|i| rows.row_text(i).unwrap()).collect();

        // User/System/Tool lines must retain literal `#`, `**`, and ```` ```rust ```` with default style.
        let sys_line_idx = all_row_text
            .iter()
            .position(|l| l == "# System Heading")
            .expect("system heading kept verbatim");
        assert!(
            rows.line(sys_line_idx)
                .unwrap()
                .spans
                .iter()
                .all(|s| s.style == ratatui::style::Style::default())
        );

        let user_heading_idx = all_row_text
            .iter()
            .position(|l| l == "# Heading")
            .expect("user # Heading kept verbatim");
        assert!(
            rows.line(user_heading_idx)
                .unwrap()
                .spans
                .iter()
                .all(|s| s.style == ratatui::style::Style::default())
        );
        let user_bold_idx = all_row_text
            .iter()
            .position(|l| l == "**bold**")
            .expect("user **bold** kept verbatim");
        assert!(
            rows.line(user_bold_idx)
                .unwrap()
                .spans
                .iter()
                .all(|s| s.style == ratatui::style::Style::default())
        );
        assert!(all_row_text.iter().any(|l| l == "```rust"));
        assert!(all_row_text.iter().any(|l| l == "| A | B |"));

        // User header uses ACCENT + BOLD; assistant header uses MUTED.
        let user_hdr = rows
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "❯ You"))
            .unwrap();
        assert!(
            user_hdr.spans.iter().any(
                |s| s.style.fg == Some(ACCENT) && s.style.add_modifier.contains(Modifier::BOLD)
            )
        );
        let asst_hdr = rows
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == "● assistant"))
            .unwrap();
        assert!(asst_hdr.spans.iter().any(|s| s.style.fg == Some(MUTED)));

        // Assistant Markdown heading/bold/code/table are rendered with rich styles.
        let asst_heading = rows
            .lines
            .iter()
            .find(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains("Heading")
                        && s.style.fg == Some(ACCENT)
                        && s.style
                            .add_modifier
                            .contains(Modifier::BOLD | Modifier::UNDERLINED)
                })
            })
            .expect("assistant heading styled");
        assert!(!asst_heading.spans.iter().any(|s| s.content.contains('#')));

        assert!(rows.lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "bold" && s.style.add_modifier.contains(Modifier::BOLD))
        }));
        assert!(rows.lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "let" && s.style.fg == Some(Color::Magenta))
        }));
        assert!(all_row_text.iter().any(|l| l.contains('┌')));
    }

    #[test]
    fn md05_assistant_mixed_text_and_media_preserves_media_reference_cleanly() {
        use super::{Rows, TranscriptSegment};
        use causa::kernel::{BlockMeta, ContentPart, MediaRef, TextPayload};
        use ratatui::style::Modifier;
        use std::sync::Arc;

        let assistant = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Text(TextPayload::new("# Chart\n\nSee image:")),
                    ContentPart::Media(MediaRef::new("image/png", "img-1")),
                    ContentPart::Text(TextPayload::new("And **summary**.")),
                ],
            },
            BlockMeta::from_source("assistant"),
        );
        let context = Context::from_blocks(vec![assistant]).unwrap();
        let transcript = Arc::new(Transcript::from_materials(&context, &[]));
        assert!(transcript.text.contains("[media image/png (img-1)]"));
        assert_eq!(
            transcript.segments,
            vec![
                TranscriptSegment::RoleHeader("● assistant".into()),
                TranscriptSegment::MarkdownBody("# Chart\n\nSee image:".into()),
                TranscriptSegment::PlainBody("[media image/png (img-1)]".into()),
                TranscriptSegment::MarkdownBody("And **summary**.".into()),
            ]
        );
        let rows = Rows::wrap(transcript, 80);
        let row_texts: Vec<String> = (0..rows.len()).map(|i| rows.row_text(i).unwrap()).collect();
        assert!(row_texts.iter().any(|l| l == "Chart"));
        assert!(row_texts.iter().any(|l| l == "See image:"));
        assert!(row_texts.iter().any(|l| l == "[media image/png (img-1)]"));
        assert!(rows.lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "summary" && s.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn md05_expanded_history_prelude_skill_task_notification_and_summary_isolation() {
        use super::{Rows, TranscriptSegment};
        use causa::kernel::BlockMeta;
        use ratatui::style::Modifier;
        use std::sync::Arc;

        let prelude = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::System, "# RawPrelude\n**not_bold_prelude**"),
            BlockMeta::from_source(tars_core::prelude::PRELUDE_BASE_SOURCE),
        );
        let skill = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<skill_content name=\"s1\" sha256=\"abc\"># RawSkill\n**not_bold_skill**</skill_content>",
            ),
            BlockMeta::from_source(tars_core::skills::SKILL_SOURCE),
        );
        let task_notif = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(
                Role::User,
                "<task-notification agent_name=\"a1\" status=\"completed\"># RawTask\n**not_bold_task**</task-notification>",
            ),
            BlockMeta::from_source(tars_core::events::TASK_NOTIFICATION_SOURCE),
        );
        // Real compaction summary blocks use Role::User + SUMMARY_SOURCE, yet are
        // assistant-written Markdown briefs and must render as Markdown when expanded.
        let summary_block = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "## Expanded Brief\n\n- **Goal**: ship"),
            BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
        );

        let transcript = Arc::new(Transcript::from_history_blocks(&[
            prelude,
            skill,
            task_notif,
            summary_block,
        ]));
        let md_segments: Vec<&str> = transcript
            .segments
            .iter()
            .filter_map(|s| match s {
                TranscriptSegment::MarkdownBody(b) => Some(b.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(md_segments, vec!["## Expanded Brief\n\n- **Goal**: ship"]);

        let rows = Rows::wrap(transcript, 80);
        let row_texts: Vec<String> = (0..rows.len()).map(|i| rows.row_text(i).unwrap()).collect();
        assert!(row_texts.iter().any(|l| l == "# RawPrelude"));
        assert!(row_texts.iter().any(|l| l == "**not_bold_prelude**"));
        assert!(row_texts.iter().any(|l| l.contains("# RawSkill")));
        assert!(row_texts.iter().any(|l| l.contains("# RawTask")));
        assert!(row_texts.iter().any(|l| l == "Expanded Brief"));
        assert!(rows.lines.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "Goal" && s.style.add_modifier.contains(Modifier::BOLD))
        }));
    }

    #[test]
    fn md08_tool_result_diff_expansion_and_styling() {
        use super::tools_detail_lines;
        use crate::render::{ACCENT, MUTED};
        use causa::kernel::{
            BlockMeta, ToolCallPayload, ToolOutput, ToolResultPayload, ToolResultStatus,
        };
        use ratatui::style::{Color, Modifier};

        let call_id = causa::runtime::new_block_id();
        let call = ContextBlock::new(
            call_id,
            BlockContent::ToolCall(ToolCallPayload::new(
                "edit_file",
                serde_json::json!({ "path": "src/lib.rs" }),
            )),
            BlockMeta::default(),
        );
        let res = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                call_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({
                    "path": "src/lib.rs",
                    "action": "updated",
                    "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n-old_line\n+new_line\n context\n",
                })),
            )),
            BlockMeta::default(),
        );
        let ctx = Context::from_blocks(vec![call, res]).unwrap();
        let transcript = Transcript::from_materials(&ctx, &[]);
        assert_eq!(transcript.tool_details.len(), 2);
        let detail = &transcript.tool_details[1];
        assert!(detail.contains("Diff (src/lib.rs):"), "{detail}");
        assert!(detail.contains("+new_line"), "{detail}");
        assert!(!detail.contains("\\n+new_line"), "{detail}");

        let styled = tools_detail_lines(&transcript.tool_details, &transcript.tool_detail_meta, 80);
        assert!(styled.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.starts_with("Declaration ")
                    && s.style.fg == Some(ACCENT)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(styled.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content.starts_with("Result for ")
                    && s.style.fg == Some(ACCENT)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(styled.iter().any(|l| {
            l.spans.iter().any(|s| {
                s.content == "Diff (src/lib.rs):"
                    && s.style.fg == Some(Color::Cyan)
                    && s.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
        assert!(styled.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "--- a/src/lib.rs" && s.style.fg == Some(MUTED))
        }));
        assert!(styled.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "@@ -1,2 +1,2 @@" && s.style.fg == Some(Color::Cyan))
        }));
        assert!(styled.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "-old_line" && s.style.fg == Some(Color::Red))
        }));
        assert!(styled.iter().any(|l| {
            l.spans
                .iter()
                .any(|s| s.content == "+new_line" && s.style.fg == Some(Color::Green))
        }));
    }

    #[test]
    fn mm08_transcript_and_history_view_render_media_badges_and_sanitize_controls() {
        use super::Rows;
        use crate::app::history::HistoryView;
        use causa::kernel::{
            BlockMeta, ContentPart, MediaRef, TextPayload, ToolCallPayload, ToolOutput,
            ToolResultPayload, ToolResultStatus,
        };
        use std::collections::HashSet;
        use std::sync::Arc;
        use tars_core::compact::{
            HistoryRecord, HistoryRecordEntry, HistoryTrigger, RangeReplacement, RewriteKind,
        };

        let valid_sha = "a".repeat(64);
        let valid_ref = format!("{}{valid_sha}", tars_core::media::MEDIA_REF_PREFIX);

        let user_text_and_img = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::User,
                parts: vec![
                    ContentPart::Text(TextPayload::new("inspect this")),
                    ContentPart::Media(
                        MediaRef::new("image/png", &valid_ref).with_name("shot.png"),
                    ),
                ],
            },
            BlockMeta::from_source("user"),
        );
        let user_image_only = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::User,
                parts: vec![ContentPart::Media(
                    MediaRef::new(
                        "image/jpeg\x1b[31m\nspoof",
                        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==",
                    )
                    .with_name("bad\u{202e}\x1b[2J\r\nname.jpg"),
                )],
            },
            BlockMeta::from_source("user"),
        );

        let call_id = causa::runtime::new_block_id();
        let tool_call = ContextBlock::new(
            call_id,
            BlockContent::ToolCall(ToolCallPayload::new("mcp_shot", serde_json::json!({}))),
            BlockMeta::default(),
        );
        let tool_res = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(
                ToolResultPayload::new(
                    call_id,
                    ToolResultStatus::Succeeded,
                    ToolOutput::new(serde_json::json!({ "ok": true })),
                )
                .with_media(vec![
                    MediaRef::new(
                        "image/webp",
                        "artifact:01933b4c-7f12-7000-8000-000000000001",
                    )
                    .with_name("mcp\x1b[33m\nsnap.webp"),
                ]),
            ),
            BlockMeta::default(),
        );

        let context = Context::from_blocks(vec![
            user_text_and_img.clone(),
            user_image_only.clone(),
            tool_call.clone(),
            tool_res.clone(),
        ])
        .unwrap();
        let transcript = Arc::new(Transcript::from_materials(&context, &[]));
        assert!(
            transcript
                .text
                .contains("inspect this\n[media image/png shot.png (media:sha256:aaaaaaaaaaaa…)]"),
            "{}",
            transcript.text
        );
        assert!(
            transcript
                .text
                .contains("[media image/jpeg\\x1b[31m spoof bad\\u{202e}\\x1b[2J\\r name.jpg (data:<redacted>)]"),
            "{}",
            transcript.text
        );
        assert!(
            transcript.text.contains(
                "[media image/webp mcp\\x1b[33m snap.webp (artifact:01933b4c-7f12-7000-8000-000000000001)]"
            ),
            "{}",
            transcript.text
        );
        assert!(!transcript.text.contains('\x1b'));
        assert!(!transcript.text.contains('\r'));
        assert!(!transcript.text.contains('\u{202e}'));
        assert!(!transcript.text.contains("iVBORw0KGgo"));

        let rows = Rows::wrap(transcript, 100);
        let joined: String = (0..rows.len())
            .map(|i| rows.row_text(i).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("[media image/png shot.png (media:sha256:aaaaaaaaaaaa…)]"));
        assert!(joined.contains("data:<redacted>"));
        assert!(joined.contains("artifact:01933b4c-7f12-7000-8000-000000000001"));

        // Verify HistoryView expansion renders the exact same sanitized badges for archived media.
        let brief = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "summary brief"),
            BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
        );
        let rec_id = "0190a000-0000-7000-8000-000000000099".to_owned();
        let record = HistoryRecord {
            format: "tars-history-v1.2".into(),
            conversation_id: "conv".into(),
            record_id: rec_id.clone(),
            created_at_ms: 1,
            trigger: HistoryTrigger::Manual,
            kind: RewriteKind::Summarize,
            estimate_before: 1000,
            estimate_after: 100,
            replacements: vec![RangeReplacement {
                range: 0..4,
                originals: vec![user_text_and_img, user_image_only, tool_call, tool_res],
                inserted_ids: vec![brief.id()],
            }],
        };
        let index = vec![HistoryRecordEntry {
            record_id: rec_id,
            created_at_ms: 1,
            trigger: HistoryTrigger::Manual,
            kind: RewriteKind::Summarize,
            removed_block_count: 4,
            inserted_block_ids: vec![brief.id()],
        }];
        let mut history_view = HistoryView::new();
        history_view.refresh(index, vec![brief], &HashSet::new(), &[Arc::new(record)]);
        let archived = history_view.display().unwrap();
        assert!(
            archived
                .text
                .contains("[media image/png shot.png (media:sha256:aaaaaaaaaaaa…)]"),
            "{}",
            archived.text
        );
        assert!(
            archived.text.contains(
                "[media image/webp mcp\\x1b[33m snap.webp (artifact:01933b4c-7f12-7000-8000-000000000001)]"
            ),
            "{}",
            archived.text
        );
        assert!(!archived.text.contains('\x1b'));
        assert!(!archived.text.contains("iVBORw0KGgo"));
    }
}
