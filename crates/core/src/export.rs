//! Readable GitHub-Flavored Markdown (GFM) conversation export and safe file creation.

use crate::compact::{BlockStatus, ExpandedBlock, ExpandedConversation, SUMMARY_SOURCE};
use crate::fs_atomic::{ReplaceMode, atomic_write_bytes};
use crate::policy::escape_untrusted_text;
use crate::prelude::PRELUDE_SOURCE_PREFIX;
use crate::session::ConversationId;
use crate::time::format_timestamp_utc;
use causa::kernel::{BlockContent, ContentPart, MediaRef, Role, ToolResultStatus};
use std::fmt::Write;
use std::io;
use std::path::{Path, PathBuf};

/// Configuration options for rendering a conversation as GitHub-Flavored Markdown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkdownExportOptions {
    /// Optional session title rendered as the top-level `#` heading.
    pub title: Option<String>,
    /// Optional model identifier displayed in the metadata header.
    pub model: Option<String>,
    /// Optional agent display label (for example, `"main"` or a child agent name).
    pub agent_label: Option<String>,
    /// Optional export timestamp in Unix epoch milliseconds, formatted as UTC.
    pub exported_at_ms: Option<u64>,
    /// Whether to include internal `tars:prelude:*` prompt-injection blocks.
    pub include_prelude: bool,
}

/// Errors returned when saving exported Markdown to disk.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Destination path already exists; export refuses to overwrite existing files.
    #[error("export file already exists: {}", .0.display())]
    AlreadyExists(PathBuf),
    /// Filesystem I/O failure while creating or writing the export file.
    #[error("failed to write export file {}: {source}", path.display())]
    Io {
        /// Target path for the export operation.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

/// Compute a Markdown backtick fence longer than any consecutive backtick run in `content`.
///
/// Always returns at least 3 backticks (` ``` `). If `content` contains runs of
/// `k` consecutive backticks (`k >= 3`), returns `k + 1` backticks so nested
/// fenced code blocks cannot prematurely close the outer fence.
pub fn safe_backtick_fence(content: &str) -> String {
    let max_run = max_consecutive_backticks(content);
    "`".repeat(max_run.max(2) + 1)
}

/// Render an [`ExpandedConversation`] as sanitized GitHub-Flavored Markdown.
///
/// Internal `ToolChange` declarations and (unless `options.include_prelude` is `true`)
/// `tars:prelude:*` prompt-injection blocks are omitted. Untrusted text undergoes
/// line-ending normalization (`\r\n` / `\r` -> `\n`) and control/BiDi escaping while
/// preserving valid UTF-8, CJK characters, emoji, and newlines. Tool payloads use
/// dynamic backtick fences so nested code blocks cannot break the Markdown structure.
pub fn render_conversation_markdown(
    conversation_id: &ConversationId,
    expanded: &ExpandedConversation,
    options: &MarkdownExportOptions,
) -> String {
    let mut out = String::new();
    let short_id: String = conversation_id.as_str().chars().take(8).collect();
    let heading = options
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(sanitize_single_line)
        .unwrap_or_else(|| format!("Session {}", sanitize_single_line(&short_id)));

    let _ = writeln!(out, "# {heading}");
    let _ = writeln!(out);

    let sanitized_id = sanitize_single_line(conversation_id.as_str());
    let _ = writeln!(
        out,
        "- **Conversation:** {}",
        format_inline_code(&sanitized_id)
    );

    if let Some(model) = options
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let sanitized_model = sanitize_single_line(model);
        let _ = writeln!(out, "- **Model:** {}", format_inline_code(&sanitized_model));
    }

    if let Some(agent) = options
        .agent_label
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        let sanitized_agent = sanitize_single_line(agent);
        let _ = writeln!(out, "- **Agent:** {sanitized_agent}");
    }

    if let Some(exported_at_ms) = options.exported_at_ms {
        let _ = writeln!(
            out,
            "- **Exported:** {}",
            format_timestamp_utc(exported_at_ms)
        );
    }

    for expanded_block in &expanded.blocks {
        if !options.include_prelude && is_prelude_block(expanded_block) {
            continue;
        }
        render_expanded_block(&mut out, expanded_block);
    }

    out
}

/// Atomically write `markdown` to a brand-new file at `path`, refusing to overwrite.
///
/// Uses create-new atomic persistence so existing files (or symlinks) fail with
/// [`ExportError::AlreadyExists`] without modification.
pub fn save_markdown_new(path: &Path, markdown: &str) -> Result<(), ExportError> {
    atomic_write_bytes(
        path,
        Some(".tars-export-"),
        markdown.as_bytes(),
        ReplaceMode::CreateNew,
    )
    .map(|_| ())
    .map_err(|source| {
        if source.kind() == io::ErrorKind::AlreadyExists {
            ExportError::AlreadyExists(path.to_path_buf())
        } else {
            ExportError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

fn is_prelude_block(expanded_block: &ExpandedBlock) -> bool {
    expanded_block
        .block
        .meta()
        .source
        .as_deref()
        .is_some_and(|source| source.starts_with(PRELUDE_SOURCE_PREFIX))
}

fn render_expanded_block(out: &mut String, expanded_block: &ExpandedBlock) {
    let status_note = status_suffix(&expanded_block.status);
    match expanded_block.block.content() {
        BlockContent::ToolChange(_) => {}
        BlockContent::Message { role, parts } => {
            let rendered_parts: Vec<String> = parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(sanitize_multiline(&text.0)),
                    ContentPart::Media(media) => Some(format_media_badge(media)),
                    ContentPart::Sealed(_) => None,
                })
                .collect();
            if rendered_parts.is_empty() {
                return;
            }
            let is_summary = expanded_block.block.meta().source.as_deref() == Some(SUMMARY_SOURCE);
            let title = if is_summary {
                "Context Summary"
            } else {
                match role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::System => "System",
                }
            };
            let _ = writeln!(out, "\n## {title}{status_note}");
            for part in rendered_parts {
                out.push('\n');
                out.push_str(&part);
                if !part.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        BlockContent::ToolCall(call) => {
            let tool_name = sanitize_single_line(&call.tool_name);
            let _ = writeln!(
                out,
                "\n### Tool Call: {}{status_note}\n",
                format_inline_code(&tool_name)
            );
            let raw_json =
                serde_json::to_string_pretty(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
            let sanitized = sanitize_multiline(&raw_json);
            push_fenced_block(out, "json", &sanitized);
        }
        BlockContent::ToolResult(result) => {
            let status_label = tool_result_status_label(&result.status);
            let artifact_id = tool_result_artifact_id(expanded_block, result);
            match artifact_id {
                Some(artifact) => {
                    let sanitized_artifact = sanitize_single_line(&artifact);
                    let _ = writeln!(
                        out,
                        "\n### Tool Result ({status_label} · artifact {}){status_note}\n",
                        format_inline_code(&sanitized_artifact)
                    );
                }
                None => {
                    let _ = writeln!(out, "\n### Tool Result ({status_label}){status_note}\n");
                }
            }
            match &result.output.content {
                serde_json::Value::String(text) => {
                    let sanitized = sanitize_multiline(text);
                    push_fenced_block(out, "text", &sanitized);
                }
                other => {
                    let raw_json =
                        serde_json::to_string_pretty(other).unwrap_or_else(|_| "null".to_owned());
                    let sanitized = sanitize_multiline(&raw_json);
                    push_fenced_block(out, "json", &sanitized);
                }
            }
            for media in &result.media {
                out.push('\n');
                out.push_str(&format_media_badge(media));
                out.push('\n');
            }
        }
    }
}

fn status_suffix(status: &BlockStatus) -> &'static str {
    match status {
        BlockStatus::Original => "",
        BlockStatus::Summarized(_) => " *(summarized)*",
        BlockStatus::MicroCompacted(_) => " *(micro-compacted)*",
        BlockStatus::Unreadable(_) => " *(unreadable history)*",
    }
}

fn tool_result_status_label(status: &ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Succeeded => "ok",
        ToolResultStatus::Failed => "failed",
        ToolResultStatus::Rejected => "rejected",
        ToolResultStatus::Cancelled => "cancelled",
        ToolResultStatus::TimedOut => "timed out",
        ToolResultStatus::UnknownOutcome => "unknown outcome",
    }
}

fn tool_result_artifact_id(
    expanded_block: &ExpandedBlock,
    result: &causa::kernel::ToolResultPayload,
) -> Option<String> {
    if let Some(artifact) = &result.output.artifact {
        return Some(artifact.id.clone());
    }
    if let BlockStatus::MicroCompacted(record_id) = &expanded_block.status {
        return Some(format!("{record_id}/{}", expanded_block.block.id().0));
    }
    None
}

fn push_fenced_block(out: &mut String, info: &str, sanitized_body: &str) {
    let fence = safe_backtick_fence(sanitized_body);
    out.push_str(&fence);
    out.push_str(info);
    out.push('\n');
    out.push_str(sanitized_body);
    if !sanitized_body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
}

fn max_consecutive_backticks(text: &str) -> usize {
    let mut max_run = 0usize;
    let mut current_run = 0usize;
    for byte in text.bytes() {
        if byte == b'`' {
            current_run += 1;
            max_run = max_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    max_run
}

fn format_inline_code(sanitized_single_line: &str) -> String {
    let max_run = max_consecutive_backticks(sanitized_single_line);
    if max_run == 0 {
        return format!("`{sanitized_single_line}`");
    }
    let fence = "`".repeat(max_run + 1);
    if sanitized_single_line.starts_with('`')
        || sanitized_single_line.ends_with('`')
        || (sanitized_single_line.starts_with(' ') && sanitized_single_line.ends_with(' '))
    {
        format!("{fence} {sanitized_single_line} {fence}")
    } else {
        format!("{fence}{sanitized_single_line}{fence}")
    }
}

fn format_media_badge(media: &MediaRef) -> String {
    let mime = sanitize_single_line(&media.media_type);
    let reference = sanitize_media_reference(&media.reference);
    let name = media
        .name
        .as_deref()
        .map(sanitize_media_name)
        .filter(|n| !n.is_empty());
    match name {
        Some(name) => format!("> [Media: {name} ({mime}, {reference})]"),
        None => format!("> [Media: {mime} ({reference})]"),
    }
}

fn sanitize_media_name(name: &str) -> String {
    let trimmed = name.trim();
    if looks_like_host_path(trimmed)
        && let Some(file_name) = trimmed
            .rsplit(['/', '\\'])
            .find(|segment| !segment.is_empty())
    {
        return sanitize_single_line(file_name);
    }
    sanitize_single_line(trimmed)
}

fn sanitize_media_reference(reference: &str) -> String {
    let trimmed = reference.trim();
    if trimmed.to_ascii_lowercase().starts_with("data:") {
        return "[redacted data URI]".to_owned();
    }
    if looks_like_host_path(trimmed) {
        return "[redacted path]".to_owned();
    }
    if trimmed.len() > 160 && !trimmed.starts_with("media:") && !trimmed.starts_with("artifact:") {
        return "[redacted ref]".to_owned();
    }
    sanitize_single_line(trimmed)
}

fn looks_like_host_path(value: &str) -> bool {
    if value.starts_with('/')
        || value.starts_with('\\')
        || value.to_ascii_lowercase().starts_with("file://")
    {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn normalize_line_endings(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

fn sanitize_multiline(text: &str) -> String {
    let normalized = normalize_line_endings(text);
    escape_untrusted_text(&normalized)
}

fn sanitize_single_line(text: &str) -> String {
    sanitize_multiline(text)
        .replace('\n', "\\n")
        .replace('\t', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::BlockStatus;
    use crate::prelude::{PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE};
    use causa::kernel::{
        ArtifactKind, ArtifactRef, BlockMeta, ContextBlock, MediaRef, SealedPart, TextPayload,
        ToolCallPayload, ToolChange, ToolOutput, ToolResultPayload, ToolResultStatus,
    };
    use causa::runtime::new_block_id;
    use serde_json::json;

    fn msg_block(
        role: Role,
        parts: Vec<ContentPart>,
        source: Option<&str>,
        status: BlockStatus,
    ) -> ExpandedBlock {
        let meta = match source {
            Some(src) => BlockMeta::from_source(src),
            None => BlockMeta::default(),
        };
        ExpandedBlock {
            status,
            block: ContextBlock::new(new_block_id(), BlockContent::Message { role, parts }, meta),
        }
    }

    #[test]
    fn safe_backtick_fence_scales_past_longest_backtick_run() {
        assert_eq!(safe_backtick_fence(""), "```");
        assert_eq!(safe_backtick_fence("no ticks"), "```");
        assert_eq!(safe_backtick_fence("inline `code` and ``two``"), "```");
        assert_eq!(
            safe_backtick_fence("contains ```rust\nfn x() {}\n```"),
            "````"
        );
        assert_eq!(
            safe_backtick_fence("contains ```` nested ``` ticks"),
            "`````"
        );
    }

    #[test]
    fn header_uses_sanitized_title_or_short_id_and_metadata() {
        let id = ConversationId::from("01936b12-3456-7890-abcd-ef1234567890");
        let empty = ExpandedConversation {
            blocks: Vec::new(),
            unreadable_records: Vec::new(),
        };

        let without_title =
            render_conversation_markdown(&id, &empty, &MarkdownExportOptions::default());
        assert!(without_title.starts_with("# Session 01936b12\n\n"));
        assert!(
            without_title.contains("- **Conversation:** `01936b12-3456-7890-abcd-ef1234567890`\n")
        );
        assert!(!without_title.contains("**Model:**"));
        assert!(!without_title.contains("**Agent:**"));
        assert!(!without_title.contains("**Exported:**"));

        let with_meta = render_conversation_markdown(
            &id,
            &empty,
            &MarkdownExportOptions {
                title: Some("Line1\nLine2\x1b[31m".into()),
                model: Some("claude-sonnet-4".into()),
                agent_label: Some("worker".into()),
                exported_at_ms: Some(1_700_000_000_000),
                include_prelude: false,
            },
        );
        assert!(with_meta.starts_with("# Line1\\nLine2\\x1b[31m\n\n"));
        assert!(with_meta.contains("- **Model:** `claude-sonnet-4`\n"));
        assert!(with_meta.contains("- **Agent:** worker\n"));
        assert!(with_meta.contains("- **Exported:** 2023-11-14 22:13:20 UTC\n"));
    }

    #[test]
    fn skips_prelude_and_tool_change_by_default_and_includes_when_enabled() {
        let id = ConversationId::from("01936b12-0000-7000-8000-000000000001");
        let expanded = ExpandedConversation {
            blocks: vec![
                ExpandedBlock {
                    status: BlockStatus::Original,
                    block: ContextBlock::new(
                        new_block_id(),
                        BlockContent::ToolChange(ToolChange::between(&[], &[])),
                        BlockMeta::default(),
                    ),
                },
                msg_block(
                    Role::System,
                    vec![ContentPart::Text(TextPayload::new("BASE CONTRACT"))],
                    Some(PRELUDE_BASE_SOURCE),
                    BlockStatus::Original,
                ),
                msg_block(
                    Role::User,
                    vec![ContentPart::Text(TextPayload::new("<env>secret</env>"))],
                    Some(PRELUDE_ENV_SOURCE),
                    BlockStatus::Original,
                ),
                msg_block(
                    Role::User,
                    vec![ContentPart::Text(TextPayload::new("Hello TARS"))],
                    None,
                    BlockStatus::Original,
                ),
            ],
            unreadable_records: Vec::new(),
        };

        let default_md =
            render_conversation_markdown(&id, &expanded, &MarkdownExportOptions::default());
        assert!(!default_md.contains("BASE CONTRACT"));
        assert!(!default_md.contains("<env>secret</env>"));
        assert!(default_md.contains("## User\n\nHello TARS\n"));

        let with_prelude = render_conversation_markdown(
            &id,
            &expanded,
            &MarkdownExportOptions {
                include_prelude: true,
                ..Default::default()
            },
        );
        assert!(with_prelude.contains("## System\n\nBASE CONTRACT\n"));
        assert!(with_prelude.contains("## User\n\n<env>secret</env>\n"));
        assert!(with_prelude.contains("## User\n\nHello TARS\n"));
    }

    #[test]
    fn normalizes_crlf_strips_bidi_and_escapes_controls_while_preserving_cjk_and_emoji() {
        let id = ConversationId::from("01936b12-0000-7000-8000-000000000002");
        let expanded = ExpandedConversation {
            blocks: vec![
                msg_block(
                    Role::User,
                    vec![ContentPart::Text(TextPayload::new(
                        "中文测试 🚀 \u{202E}rtl\u{202C}\r\nline2\rline3\x1b[31m",
                    ))],
                    None,
                    BlockStatus::Original,
                ),
                msg_block(
                    Role::Assistant,
                    vec![
                        ContentPart::Sealed(SealedPart::new(
                            "thinking",
                            json!("internal thinking"),
                        )),
                        ContentPart::Text(TextPayload::new("可见回复 👨‍👩‍👧‍👦")),
                    ],
                    None,
                    BlockStatus::Original,
                ),
                msg_block(
                    Role::Assistant,
                    vec![ContentPart::Sealed(SealedPart::new(
                        "thinking",
                        json!("only sealed"),
                    ))],
                    None,
                    BlockStatus::Original,
                ),
            ],
            unreadable_records: Vec::new(),
        };

        let md = render_conversation_markdown(&id, &expanded, &MarkdownExportOptions::default());
        assert!(md.contains("中文测试 🚀 \\u{202e}rtl\\u{202c}\nline2\nline3\\x1b[31m"));
        assert!(!md.contains('\r'));
        assert!(!md.contains('\u{202E}'));
        assert!(!md.contains('\x1b'));
        assert!(md.contains("可见回复 👨‍👩‍👧‍👦"));
        assert!(!md.contains("internal thinking"));
        assert!(!md.contains("only sealed"));
        assert_eq!(md.matches("## Assistant").count(), 1);
    }

    #[test]
    fn renders_media_badges_without_base64_or_host_paths() {
        let id = ConversationId::from("01936b12-0000-7000-8000-000000000003");
        let expanded = ExpandedConversation {
            blocks: vec![msg_block(
                Role::User,
                vec![
                    ContentPart::Media(
                        MediaRef::new("image/png", "media:sha256:abcdef0123456789")
                            .with_name("shot.png"),
                    ),
                    ContentPart::Media(
                        MediaRef::new("image/jpeg", "data:image/jpeg;base64,/9j/4AAQSkZJRg==")
                            .with_name("/Users/alice/private/photo.jpg"),
                    ),
                    ContentPart::Media(MediaRef::new("application/pdf", "/etc/passwd")),
                ],
                None,
                BlockStatus::Original,
            )],
            unreadable_records: Vec::new(),
        };

        let md = render_conversation_markdown(&id, &expanded, &MarkdownExportOptions::default());
        assert!(md.contains("> [Media: shot.png (image/png, media:sha256:abcdef0123456789)]"));
        assert!(md.contains("> [Media: photo.jpg (image/jpeg, [redacted data URI])]"));
        assert!(md.contains("> [Media: application/pdf ([redacted path])]"));
        assert!(!md.contains("/9j/4AAQSkZJRg=="));
        assert!(!md.contains("/Users/alice/private"));
        assert!(!md.contains("/etc/passwd"));
    }

    #[test]
    fn renders_tool_calls_results_and_compaction_markers_with_dynamic_fences() {
        let id = ConversationId::from("01936b12-0000-7000-8000-000000000004");
        let call_id = new_block_id();
        let rec_id = "01936b12-aaaa-7000-8000-000000000099".to_owned();

        let call_block = ExpandedBlock {
            status: BlockStatus::Summarized(rec_id.clone()),
            block: ContextBlock::new(
                call_id,
                BlockContent::ToolCall(ToolCallPayload::new(
                    "write_file",
                    json!({
                        "path": "README.md",
                        "content": "hello\n```\nrust\n````\nend"
                    }),
                )),
                BlockMeta::default(),
            ),
        };

        let result_block_id = new_block_id();
        let mut output = ToolOutput::new(serde_json::Value::String(
            "line1\n```\ninner\n```\n````\nfour".to_owned(),
        ));
        output.artifact = Some(ArtifactRef::new(
            "artifact:01936b12-bbbb-7000-8000-000000000088",
            512,
            ArtifactKind::FullOutput,
            true,
        ));
        let result_block = ExpandedBlock {
            status: BlockStatus::MicroCompacted(rec_id.clone()),
            block: ContextBlock::new(
                result_block_id,
                BlockContent::ToolResult(
                    ToolResultPayload::new(call_id, ToolResultStatus::Succeeded, output)
                        .with_media(vec![
                            MediaRef::new(
                                "image/png",
                                "artifact:01936b12-cccc-7000-8000-000000000077",
                            )
                            .with_name("chart.png"),
                        ]),
                ),
                BlockMeta::default(),
            ),
        };

        let summary_block = msg_block(
            Role::Assistant,
            vec![ContentPart::Text(TextPayload::new(
                "Earlier work summarized.",
            ))],
            Some(SUMMARY_SOURCE),
            BlockStatus::Original,
        );

        let expanded = ExpandedConversation {
            blocks: vec![summary_block, call_block, result_block],
            unreadable_records: Vec::new(),
        };

        let md = render_conversation_markdown(&id, &expanded, &MarkdownExportOptions::default());
        assert!(md.contains("## Context Summary\n\nEarlier work summarized.\n"));
        assert!(md.contains("### Tool Call: `write_file` *(summarized)*\n"));
        assert!(md.contains("`````json\n"));
        assert!(md.contains("\n`````\n"));
        assert!(md.contains(
            "### Tool Result (ok · artifact `artifact:01936b12-bbbb-7000-8000-000000000088`) *(micro-compacted)*\n"
        ));
        assert!(md.contains("`````text\nline1\n```\ninner\n```\n````\nfour\n`````\n"));
        assert!(md.contains(
            "> [Media: chart.png (image/png, artifact:01936b12-cccc-7000-8000-000000000077)]"
        ));
    }

    #[test]
    fn save_markdown_new_writes_atomically_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("conversation.md");

        save_markdown_new(&target, "# Session\n\nfirst\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "# Session\n\nfirst\n"
        );

        let err = save_markdown_new(&target, "# Session\n\noverwrite\n").unwrap_err();
        assert!(
            matches!(err, ExportError::AlreadyExists(ref p) if p == &target),
            "expected AlreadyExists, got {err:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "# Session\n\nfirst\n"
        );
    }
}
