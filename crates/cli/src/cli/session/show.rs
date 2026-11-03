//! Read-only report of one saved session's current state.

use super::execute::revision;
use crate::cli::command::output;
use crate::cli::execute::early_failure;
use crate::cli::managed_execution;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::ClosePolicy;

/// Report the saved revision, materials, context usage, retained-work
/// capacity and history count.
///
/// Existing top-level keys stay stable; new facts are added beside them.
pub(super) async fn show(store: PathBuf, id: String, json_output: bool) -> ExitCode {
    let host = managed_execution::definition_host(store);
    let session = match host.open(id.clone().into()).await {
        Ok(session) => session,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    managed_execution::report_open_notices(&session);
    let definition = session.definition();
    let revision = revision(&session.status().save);
    let usage = session.context_usage();
    let session_usage = session.session_usage();
    let history = session.history().len();
    let work_capacity = session.work_capacity();
    let mut code = 0;
    let block_count = match session.read_materials() {
        Ok(snapshot) => snapshot.context.blocks().len(),
        Err(error) => {
            code = 1;
            let _ = writeln!(std::io::stderr().lock(), "TARS: materials: {error}");
            0
        }
    };
    let value = json!({
        "conversation_id": id,
        "title": session.title(),
        "created_at_ms": session.created_at_ms(),
        "updated_at_ms": session.updated_at_ms(),
        "prompt_cache": definition.prompt_cache,
        "revision": revision,
        "saved": revision.is_some(),
        "definition": definition,
        "block_count": block_count,
        "context_estimate_tokens": usage.map(|usage| usage.estimate_tokens),
        "context_window": usage.and_then(|usage| usage.context_window),
        "auto_compact": usage.map(|usage| usage.auto_compact),
        "compact_threshold": usage.map(|usage| usage.compact_threshold),
        "history_records": history,
        "work_capacity": work_capacity.map(|capacity| json!({
            "used": capacity.used, "capacity": capacity.capacity,
        })),
        "usage": session_usage.as_ref().map(crate::cli::usage_json::session_usage_value),
    });
    if let Err(error) = host
        .close(&session.target(), ClosePolicy::DiscardLocal)
        .await
    {
        code = 1;
        let _ = writeln!(std::io::stderr().lock(), "TARS: close_error: {error}");
    }
    let rendered = if json_output {
        format!("{value}\n")
    } else {
        let mut text = format!("Conversation {id}\n");
        if let Some(title) = session.title() {
            text.push_str(&format!("Title: {title}\n"));
        }
        if let Some(created) = session.created_at_ms().map(tars_core::format_timestamp_utc) {
            text.push_str(&format!("Created: {created}\n"));
        }
        text.push_str(&format!(
            "Last activity: {}\n",
            tars_core::format_timestamp_utc(session.updated_at_ms())
        ));
        text.push_str(&format!(
            "Saved revision {} · {block_count} block(s) · {history} history record(s)\n",
            revision.map_or_else(|| "none".to_owned(), |value| value.to_string()),
        ));
        match usage {
            Some(usage) => {
                let size = match usage.context_window {
                    Some(window) => format!("~{} tokens of {window}", usage.estimate_tokens),
                    None => format!("~{} tokens (window unknown)", usage.estimate_tokens),
                };
                text.push_str(&format!(
                    "Context: {size} · auto {} · threshold {}%\n",
                    if usage.auto_compact { "on" } else { "off" },
                    usage.compact_threshold,
                ));
            }
            None => text.push_str("Context: usage unavailable for this session\n"),
        }
        match work_capacity {
            Some(capacity) => text.push_str(&format!(
                "Works: {} / {}\n",
                capacity.used, capacity.capacity
            )),
            None => text.push_str("Works: unavailable for this session\n"),
        }
        text.push_str(&format!(
            "Model: {} {}\n",
            provider_name(&definition),
            definition.model
        ));
        text.push_str(&format!(
            "Prompt cache: {}\n",
            if definition.prompt_cache { "on" } else { "off" }
        ));
        match session_usage {
            Some(usage) => {
                for line in tars_core::usage::session_usage_lines(&usage) {
                    text.push_str(&line);
                    text.push('\n');
                }
            }
            None => text.push_str("Usage: unavailable for this session\n"),
        }
        text
    };
    if output(&rendered) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}

fn provider_name(definition: &tars_core::definition::SessionDefinition) -> &'static str {
    use tars_core::definition::Provider;
    match definition.provider {
        Provider::OpenAiChat => "openai-chat",
        Provider::OpenAiResponses => "openai-responses",
        Provider::Anthropic => "anthropic",
    }
}
