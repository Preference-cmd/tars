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
    let session = match host.open(id.clone().into()).await {
        Ok(session) => session,
        // Another process holding the writer reports the surrounding
        // commands' existing busy error.
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    managed_execution::report_open_notices(&session);
    let mut value = json!({
        "status": "not_started", "conversation_id": id, "trigger": null,
        "estimate_before": null, "estimate_after": null, "record_id": null, "summary_block_id": null, "blocks": null, "saved": false,
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
            text = format!(
                "Compacted ({}): ~{} -> ~{} tokens · {} blocks -> record {} · summary block {}\n",
                outcome.trigger.name(),
                outcome.estimate_before,
                outcome.estimate_after,
                outcome.record.removed_block_count,
                outcome.record.record_id,
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
/// summarized, elided or unreadable. `--json` writes one block per line.
/// `--last N` prints only the newest N blocks and reads only the records they
/// need. An unreadable record is reported and the command exits 1 after
/// printing what it could.
pub(super) async fn history(
    store: PathBuf,
    id: String,
    last: Option<usize>,
    json_output: bool,
) -> ExitCode {
    let host = managed_execution::definition_host(store);
    let session = match host.open(id.clone().into()).await {
        Ok(session) => session,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    managed_execution::report_open_notices(&session);
    let expansion = match session.expanded_conversation(last) {
        Ok(exp) => exp,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let unreadable = !expansion.unreadable_records.is_empty();
    let write_result = {
        let mut out = io::stdout().lock();
        (|| -> io::Result<()> {
            if json_output {
                for block in &expansion.blocks {
                    serde_json::to_writer(&mut out, block).map_err(io::Error::other)?;
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
    if let Err(error) = host
        .close(&session.target(), ClosePolicy::DiscardLocal)
        .await
    {
        code = 1;
        let _ = writeln!(io::stderr().lock(), "TARS: close_error: {error}");
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
                    ContentPart::Media(media) => writeln!(
                        out,
                        "[media {} {}]",
                        display(&media.media_type),
                        display(&media.reference)
                    )?,
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
            writeln!(
                out,
                "[tool result · {}] {:?} · declaration {}",
                expanded.status, result.status, result.call_block_id.0
            )?;
            writeln!(
                out,
                "{}",
                display(&serde_json::to_string(&result.output.content).unwrap_or_default())
            )?;
        }
        BlockContent::ToolChange(_) => {}
    }
    Ok(())
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
