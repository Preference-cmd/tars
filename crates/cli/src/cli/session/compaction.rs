//! Manual compaction and read-only history listing for persistent sessions.

use super::execute::{close_value, report_confirmation, revision};
use crate::cli::execute::early_failure;
use crate::cli::managed_execution;
use causa::kernel::{BlockContent, ContentPart, Role};
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::{ClosePolicy, SaveStatus};
use tars_core::compact::{ExpandedBlock, ExpandedConversation};
use tars_core::permission::escape_untrusted_text;

/// Replace the current materials with a model-written brief and save them.
pub(super) async fn compact(
    store: PathBuf,
    id: String,
    recovery_dir: PathBuf,
    json_output: bool,
) -> ExitCode {
    // Opening reads the saved definition; compaction calls the session's own
    // model, so the configured credential environment must still resolve.
    let host = managed_execution::definition_host(store);
    let resolved_id = match super::execute::resolve_selector(&host, &id, json_output).await {
        Ok(id) => id,
        Err(code) => return code,
    };
    let session = match host.open(resolved_id.clone()).await {
        Ok(session) => session,
        // Another process holding the writer reports the surrounding
        // commands' existing busy error.
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    managed_execution::report_open_notices(&session);
    let mut value = json!({
        "status": "not_started", "conversation_id": resolved_id.as_str(), "trigger": null,
        "estimate_before": null, "estimate_after": null, "record_id": null, "merge_record_id": null, "summary_block_id": null, "blocks": null, "saved": false,
        "revision": null, "error": null, "save_error": null, "recovery_file": null,
        "recovery_importable": null, "recovery_error": null, "close_error": null,
    });
    let mut code = 0;
    let mut policy = ClosePolicy::DiscardLocal;
    let mut text = String::new();
    match session.compact().await {
        Err(error) => {
            code = 1;
            value["status"] = json!("rejected");
            value["error"] = json!(error.to_string());
        }
        Ok(outcome) => {
            policy = ClosePolicy::Save;
            let summary_block_id = outcome.brief.0.to_string();
            value["status"] = json!("compacted");
            value["trigger"] = json!(outcome.trigger.name());
            value["estimate_before"] = json!(outcome.estimate_before);
            value["estimate_after"] = json!(outcome.estimate_after);
            value["record_id"] = json!(outcome.record.record_id);
            value["summary_block_id"] = json!(summary_block_id);
            value["blocks"] = json!(outcome.record.removed_block_count);
            let merged = outcome.merged.as_ref().map(|merge| merge.record_id.clone());
            value["merge_record_id"] = json!(merged);
            let merged = merged
                .map(|record| format!(" · briefs merged -> record {record}"))
                .unwrap_or_default();
            text = format!(
                "Compacted ({}): ~{} -> ~{} tokens · {} blocks -> record {}{} · summary block {}\n",
                outcome.trigger.name(),
                outcome.estimate_before,
                outcome.estimate_after,
                outcome.record.removed_block_count,
                outcome.record.record_id,
                merged,
                summary_block_id,
            );
            match session.wait_saved().await {
                Ok(save) => {
                    value["saved"] = json!(matches!(save, SaveStatus::Saved { .. }));
                    value["revision"] = json!(revision(&save));
                }
                Err(error) => {
                    code = 1;
                    policy = ClosePolicy::DiscardLocal;
                    value["error"] = json!("session save failed");
                    value["save_error"] = json!(error.to_string());
                    match managed_execution::recovery(&session, &recovery_dir) {
                        Ok(file) => {
                            value["recovery_file"] = json!(file.path);
                            value["recovery_importable"] = json!(file.importable);
                        }
                        Err(error) => value["recovery_error"] = json!(error),
                    }
                }
            }
        }
    }
    match host.close(&session.target(), policy).await {
        Ok(outcome) => {
            report_confirmation(&outcome);
            if let Some(saved) = revision(&outcome.save) {
                value["saved"] = json!(true);
                value["revision"] = json!(saved);
            }
            value["close_outcome"] = close_value(&outcome);
        }
        Err(error) => {
            code = 1;
            value["close_error"] = json!(error.to_string());
        }
    }
    if let Some(error) = value["error"].as_str() {
        let _ = writeln!(io::stderr().lock(), "TARS: {error}");
    }
    if let Some(path) = value["recovery_file"].as_str() {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: recovery materials: {}{}",
            path,
            managed_execution::recovery_limit_warning(
                value["recovery_importable"].as_bool().unwrap_or(false),
            )
        );
    }
    for field in ["save_error", "recovery_error", "close_error"] {
        if let Some(error) = value[field].as_str() {
            let _ = writeln!(io::stderr().lock(), "TARS: {field}: {error}");
        }
    }
    if json_output {
        if crate::cli::command::output(&format!("{value}\n")) != ExitCode::SUCCESS {
            return ExitCode::FAILURE;
        }
    } else if crate::cli::command::output(&text) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}

/// Print the expanded full conversation for a persistent session.
///
/// Rewrites are unfolded recursively and each block is marked original,
/// summarized or unreadable, and tool results with spilled outputs include
/// their artifact id. `--json` writes one block per line.
/// `--last N` prints only the newest N blocks and reads only the records they
/// need. An unreadable record is reported and the command exits 1 after
/// printing what it could.
pub(super) async fn history(
    store: PathBuf,
    id: String,
    agent: Option<String>,
    last: Option<usize>,
    json_output: bool,
) -> ExitCode {
    let host = managed_execution::definition_host(store);
    let resolved_id = match super::execute::resolve_selector(&host, &id, json_output).await {
        Ok(id) => id,
        Err(code) => return code,
    };
    let expansion = match host
        .read_saved_history(&resolved_id, agent.as_deref(), last)
        .await
    {
        Ok(exp) => exp,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let unreadable = !expansion.unreadable_records.is_empty();
    let write_result = {
        let mut out = io::stdout().lock();
        (|| -> io::Result<()> {
            if json_output {
                for block in &expansion.blocks {
                    let mut value = serde_json::to_value(block).map_err(io::Error::other)?;
                    if let BlockContent::ToolResult(result) = block.block.content()
                        && let Some(artifact_id) = history_tool_result_artifact_id(block, result)
                        && let Some(object) = value.as_object_mut()
                    {
                        object.insert("artifact_id".into(), json!(artifact_id));
                    }
                    serde_json::to_writer(&mut out, &value).map_err(io::Error::other)?;
                    writeln!(out)?;
                }
            } else {
                write_human(&mut out, &expansion)?;
            }
            Ok(())
        })()
    };
    for unreadable in &expansion.unreadable_records {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: history record {} cannot be read: {}",
            unreadable.record_id,
            unreadable.error
        );
    }
    let mut code = if unreadable { 1 } else { 0 };
    match write_result {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(error) => {
            code = 1;
            let _ = writeln!(io::stderr().lock(), "TARS: {error}");
        }
    }
    ExitCode::from(code)
}

fn write_human(out: &mut dyn Write, expansion: &ExpandedConversation) -> io::Result<()> {
    for block in &expansion.blocks {
        write_expanded_block(out, block)?;
    }
    Ok(())
}

fn write_expanded_block(out: &mut dyn Write, expanded: &ExpandedBlock) -> io::Result<()> {
    match expanded.block.content() {
        BlockContent::Message { role, parts } => {
            writeln!(out, "[{} · {}]", role_label(*role), expanded.status)?;
            for part in parts {
                match part {
                    ContentPart::Text(text) => writeln!(out, "{}", display(&text.0))?,
                    ContentPart::Media(media) => write_media_line(out, media)?,
                    ContentPart::Sealed(_) => {}
                }
            }
        }
        BlockContent::ToolCall(call) => {
            writeln!(
                out,
                "[tool call · {}] {} · declaration {}",
                expanded.status,
                display(&call.tool_name),
                expanded.block.id().0
            )?;
            writeln!(
                out,
                "{}",
                display(&serde_json::to_string(&call.arguments).unwrap_or_default())
            )?;
        }
        BlockContent::ToolResult(result) => {
            let artifact = history_tool_result_artifact_id(expanded, result)
                .map(|id| format!(" · {}", display(&id)))
                .unwrap_or_default();
            writeln!(
                out,
                "[tool result · {}] {:?} · declaration {}{}",
                expanded.status, result.status, result.call_block_id.0, artifact
            )?;
            writeln!(
                out,
                "{}",
                display(&serde_json::to_string(&result.output.content).unwrap_or_default())
            )?;
            for media in &result.media {
                write_media_line(out, media)?;
            }
        }
        BlockContent::ToolChange(_) => {}
    }
    Ok(())
}

fn history_tool_result_artifact_id(
    expanded: &ExpandedBlock,
    result: &causa::kernel::ToolResultPayload,
) -> Option<String> {
    if let Some(artifact) = &result.output.artifact {
        return Some(artifact.id.clone());
    }
    if let tars_core::compact::BlockStatus::MicroCompacted(record_id) = &expanded.status {
        return Some(format!("{record_id}/{}", expanded.block.id().0));
    }
    None
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// Escape untrusted text and keep tabs from reflowing the layout.
fn display(value: &str) -> String {
    escape_untrusted_text(value).replace('\t', "    ")
}

/// Escape untrusted text onto a single line so metadata cannot inject line breaks.
fn display_single_line(value: &str) -> String {
    display(value).replace('\n', "\\n")
}

fn write_media_line(out: &mut dyn Write, media: &causa::kernel::MediaRef) -> io::Result<()> {
    let media_type = display_single_line(&media.media_type);
    let reference = display_single_line(&media.reference);
    match media
        .name
        .as_deref()
        .map(display_single_line)
        .filter(|name| !name.is_empty())
    {
        Some(name) => writeln!(out, "[media {media_type} {name} ({reference})]"),
        None => writeln!(out, "[media {media_type} {reference}]"),
    }
}

#[cfg(test)]
mod tests {
    use super::write_expanded_block;
    use causa::kernel::{
        BlockContent, BlockMeta, ContentPart, ContextBlock, MediaRef, Role, TextPayload,
        ToolOutput, ToolResultPayload, ToolResultStatus,
    };
    use causa::runtime::new_block_id;
    use tars_core::compact::{BlockStatus, ExpandedBlock};

    #[test]
    fn history_media_rendering_sanitizes_control_chars_and_renders_tool_media() {
        let user_block = ExpandedBlock {
            block: ContextBlock::new(
                new_block_id(),
                BlockContent::Message {
                    role: Role::User,
                    parts: vec![
                        ContentPart::Text(TextPayload::new("hello")),
                        ContentPart::Media(
                            MediaRef::new(
                                "image/png\x1b[31m\n[assistant · original]",
                                "media:sha256:abcd\r\n",
                            )
                            .with_name("shot\x1b[2J\t1\nspoof.png"),
                        ),
                    ],
                },
                BlockMeta::default(),
            ),
            status: BlockStatus::Original,
        };
        let tool_block = ExpandedBlock {
            block: ContextBlock::new(
                new_block_id(),
                BlockContent::ToolResult(
                    ToolResultPayload::new(
                        new_block_id(),
                        ToolResultStatus::Succeeded,
                        ToolOutput::new(serde_json::json!("ok")),
                    )
                    .with_media(vec![
                        MediaRef::new(
                            "image/webp",
                            "artifact:01933b4c-7f12-7000-8000-000000000001",
                        )
                        .with_name("snap\x1b[33m\nline.webp"),
                        MediaRef::new("image/gif", "artifact:01933b4c-7f12-7000-8000-000000000002"),
                    ]),
                ),
                BlockMeta::default(),
            ),
            status: BlockStatus::Original,
        };

        let mut buf = Vec::new();
        write_expanded_block(&mut buf, &user_block).unwrap();
        write_expanded_block(&mut buf, &tool_block).unwrap();
        let text = String::from_utf8(buf).unwrap();

        assert!(!text.contains('\x1b') && !text.contains('\r'), "{text:?}");
        assert!(
            !text.lines().any(|line| line.starts_with("[assistant")),
            "{text}"
        );
        assert!(
            text.contains(
                "[media image/png\\x1b[31m\\n[assistant · original] shot\\x1b[2J    1\\nspoof.png (media:sha256:abcd\\r\\n)]"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "[media image/webp snap\\x1b[33m\\nline.webp (artifact:01933b4c-7f12-7000-8000-000000000001)]"
            ),
            "{text}"
        );
        assert!(
            text.contains("[media image/gif artifact:01933b4c-7f12-7000-8000-000000000002]"),
            "{text}"
        );
    }
}
