//! Manual compaction and read-only history listing for persistent sessions.

use super::execute::{close_value, report_confirmation, revision};
use crate::cli::execute::early_failure;
use crate::cli::managed_execution;
use causa::kernel::{BlockContent, ContentPart, Context, Role};
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::{ClosePolicy, ManagedSession, SaveStatus};
use tars_core::compact::{HistorySegment, HistorySegmentDocument};
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
        "estimate_before": null, "estimate_after": null, "segment_id": null,
        "summary_block_id": null, "blocks": null, "saved": false, "revision": null,
        "error": null, "save_error": null, "recovery_file": null,
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
            value["status"] = json!("compacted");
            value["trigger"] = json!(outcome.trigger.name());
            value["estimate_before"] = json!(outcome.estimate_before);
            value["estimate_after"] = json!(outcome.estimate_after);
            value["segment_id"] = json!(outcome.segment.id);
            value["summary_block_id"] = json!(outcome.segment.summary_block_id.0);
            value["blocks"] = json!(outcome.segment.blocks);
            text = format!(
                "Compacted ({}): ~{} -> ~{} tokens · {} blocks -> segment {} · summary block {}\n",
                outcome.trigger.name(),
                outcome.estimate_before,
                outcome.estimate_after,
                outcome.segment.blocks,
                outcome.segment.id,
                outcome.segment.summary_block_id.0,
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

/// List the replaced conversation segments in order, then the current material.
///
/// Segments are read one at a time and written straight to stdout, so one
/// segment is the most that is held in memory. `--json` writes one
/// `tars-history-v1` document per line.
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
    let all = session.history();
    let total = all.len();
    let skip = match last {
        Some(count) => total.saturating_sub(count),
        None => 0,
    };
    let visible = &all[skip..];
    let mut unreadable = false;
    let write_result = {
        let mut out = io::stdout().lock();
        (|| -> io::Result<()> {
            if json_output {
                for segment in visible {
                    match session.read_history_segment(&segment.id) {
                        Ok(document) => {
                            serde_json::to_writer(&mut out, &document).map_err(io::Error::other)?;
                            writeln!(out)?;
                        }
                        Err(error) => {
                            unreadable = true;
                            let _ = writeln!(
                                io::stderr().lock(),
                                "TARS: history segment {}: {error}",
                                segment.id
                            );
                        }
                    }
                }
            } else {
                write_human(&mut out, &session, visible, total, skip)?;
            }
            Ok(())
        })()
    };
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

fn write_human(
    out: &mut dyn Write,
    session: &ManagedSession,
    segments: &[HistorySegment],
    total: usize,
    skipped: usize,
) -> io::Result<()> {
    if total == 0 {
        writeln!(out, "History: no replaced segments.")?;
    } else if skipped == 0 {
        writeln!(out, "History: {total} segment(s).")?;
    } else {
        writeln!(
            out,
            "History: {total} segment(s); showing the newest {}; {skipped} earlier not shown.",
            segments.len()
        )?;
    }
    let first = skipped + 1;
    for (index, segment) in segments.iter().enumerate() {
        let number = first + index;
        writeln!(out)?;
        match session.read_history_segment(&segment.id) {
            Ok(document) => {
                write_segment_header(out, number, total, &document.segment_id, &document)?;
                write_context(out, &document.context)?;
            }
            Err(error) => {
                writeln!(
                    out,
                    "--- Segment {number} of {total} · {} · {} · {} block(s) · estimates unavailable",
                    segment.trigger.name(),
                    format_timestamp(segment.created_at_ms),
                    segment.blocks,
                )?;
                writeln!(out, "[this history segment cannot be read: {error}]")?;
            }
        }
    }
    match session.read_materials() {
        Ok(snapshot) => {
            let estimate = session
                .context_usage()
                .map(|usage| format!(" · ~{} tokens", usage.estimate_tokens))
                .unwrap_or_default();
            writeln!(out)?;
            writeln!(
                out,
                "--- Current material · {} block(s){estimate}",
                snapshot.context.blocks().len()
            )?;
            write_context(out, &snapshot.context)?;
        }
        Err(error) => {
            writeln!(out)?;
            writeln!(out, "[current material cannot be read: {error}]")?;
        }
    }
    Ok(())
}

fn write_segment_header(
    out: &mut dyn Write,
    number: usize,
    total: usize,
    segment_id: &str,
    document: &HistorySegmentDocument,
) -> io::Result<()> {
    writeln!(
        out,
        "--- Segment {number} of {total} · {} · {} · {} block(s) · ~{} -> ~{} tokens · segment {} · summary {}",
        document.trigger.name(),
        format_timestamp(document.created_at_ms),
        document.context.blocks().len(),
        document.estimate_before,
        document.estimate_after,
        segment_id,
        document.summary_block_id.0,
    )
}

/// Write one Context as display text, never as terminal control instructions.
fn write_context(out: &mut dyn Write, context: &Context) -> io::Result<()> {
    for block in context.blocks() {
        match block.content() {
            BlockContent::Message { role, parts } => {
                writeln!(out, "[{}]", role_label(*role))?;
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
                    "[tool call] {} · declaration {}",
                    display(&call.tool_name),
                    block.id().0
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
                    "[tool result] {:?} · declaration {}",
                    result.status, result.call_block_id.0
                )?;
                writeln!(
                    out,
                    "{}",
                    display(&serde_json::to_string(&result.output.content).unwrap_or_default())
                )?;
            }
            BlockContent::ToolChange(_) => {}
        }
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

/// Milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// A tiny civil-date conversion avoids a date dependency for one display line.
fn format_timestamp(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian calendar date.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year + i64::from(month <= 2);
    (
        year,
        u64::try_from(month).unwrap_or(0),
        u64::try_from(day).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, format_timestamp};

    #[test]
    fn history_timestamps_are_utc_dates() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_000), "1970-01-01T00:00:01Z");
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-11-01T12:34:56Z
        assert_eq!(format_timestamp(1_793_536_496_000), "2026-11-01T12:34:56Z");
    }
}
