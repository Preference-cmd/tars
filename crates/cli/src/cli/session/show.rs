//! Read-only report of one saved session's current state.

use crate::cli::command::output;
use crate::cli::execute::early_failure;
use crate::cli::managed_execution;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::prelude::DynamicCustomSlot;

/// Report the saved revision, materials, context usage, retained-work
/// capacity, history count, and prelude/skills summary without resolving
/// credentials, locking, or mutating any files.
///
/// Existing top-level keys stay stable; new facts are added beside them.
pub(super) async fn show(store: PathBuf, id: String, json_output: bool) -> ExitCode {
    let host = managed_execution::definition_host(store);
    let inspection = match host.inspect(&id.clone().into()).await {
        Ok(inspection) => inspection,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let definition = &inspection.definition;
    let prelude = &definition.prelude;
    let (dynamic_custom_kind, dynamic_custom_file) = match &prelude.dynamic_custom {
        None => ("none", None),
        Some(DynamicCustomSlot::Inline(_)) => ("inline", None),
        Some(DynamicCustomSlot::File(path)) => ("file", Some(path)),
    };
    let value = json!({
        "conversation_id": id,
        "title": inspection.title,
        "created_at_ms": inspection.created_at_ms,
        "updated_at_ms": inspection.updated_at_ms,
        "prompt_cache": definition.prompt_cache,
        "code_execution": definition.code_execution.is_some(),
        "revision": inspection.revision,
        "saved": true,
        "definition": definition,
        "block_count": inspection.block_count,
        "context_estimate_tokens": inspection.context_usage.estimate_tokens,
        "context_window": inspection.context_usage.context_window,
        "auto_compact": inspection.context_usage.auto_compact,
        "compact_threshold": inspection.context_usage.compact_threshold,
        "history_records": inspection.history_records,
        "work_capacity": {
            "used": inspection.work_capacity.used,
            "capacity": inspection.work_capacity.capacity,
        },
        "usage": crate::cli::usage_json::session_usage_value(&inspection.usage),
        "prelude": {
            "static_custom": prelude.static_custom.is_some(),
            "dynamic_custom_kind": dynamic_custom_kind,
            "dynamic_custom_file": dynamic_custom_file,
            "dynamic_custom_file_exists": inspection.dynamic_context_file_exists,
            "config_dir": prelude.config_dir,
            "skills_enabled": prelude.skills_enabled,
            "skill_dirs_count": prelude.skill_dirs.len(),
            "include_workspace_skills": prelude.include_workspace_skills,
            "skills_count": inspection.skills_count,
        },
    });
    let rendered = if json_output {
        format!("{value}\n")
    } else {
        let mut text = format!("Conversation {id}\n");
        if let Some(title) = &inspection.title {
            text.push_str(&format!("Title: {title}\n"));
        }
        if let Some(created) = inspection
            .created_at_ms
            .map(tars_core::format_timestamp_utc)
        {
            text.push_str(&format!("Created: {created}\n"));
        }
        text.push_str(&format!(
            "Last activity: {}\n",
            tars_core::format_timestamp_utc(inspection.updated_at_ms)
        ));
        text.push_str(&format!(
            "Saved revision {} · {} block(s) · {} history record(s)\n",
            inspection.revision, inspection.block_count, inspection.history_records,
        ));
        let size = match inspection.context_usage.context_window {
            Some(window) => format!(
                "~{} tokens of {window}",
                inspection.context_usage.estimate_tokens
            ),
            None => format!(
                "~{} tokens (window unknown)",
                inspection.context_usage.estimate_tokens
            ),
        };
        text.push_str(&format!(
            "Context: {size} · auto {} · threshold {}%\n",
            if inspection.context_usage.auto_compact {
                "on"
            } else {
                "off"
            },
            inspection.context_usage.compact_threshold,
        ));
        text.push_str(&format!(
            "Works: {} / {}\n",
            inspection.work_capacity.used, inspection.work_capacity.capacity
        ));
        text.push_str(&format!(
            "Model: {} {}\n",
            provider_name(definition),
            definition.model
        ));
        text.push_str(&format!(
            "Prompt cache: {}\n",
            if definition.prompt_cache { "on" } else { "off" }
        ));
        text.push_str(&format!(
            "Code execution: {}\n",
            match &definition.code_execution {
                Some(ce) => format!(
                    "on (timeout {}s, max_subcalls {}, max_concurrency {})",
                    ce.timeout_secs, ce.max_subcalls, ce.max_concurrency
                ),
                None => "off".to_owned(),
            }
        ));
        let static_label = if prelude.static_custom.is_some() {
            "configured"
        } else {
            "none"
        };
        let dynamic_label = match &prelude.dynamic_custom {
            None => "none".to_owned(),
            Some(DynamicCustomSlot::Inline(_)) => "inline".to_owned(),
            Some(DynamicCustomSlot::File(path)) => {
                let state = match inspection.dynamic_context_file_exists {
                    Some(true) => "present",
                    Some(false) => "missing",
                    None => "unknown",
                };
                format!("file ({}, {state})", path.display())
            }
        };
        text.push_str(&format!(
            "Prelude: static_custom {static_label} · dynamic_custom {dynamic_label}\n"
        ));
        text.push_str(&format!(
            "Skills: {} · {} dir(s) · workspace {} · {} available\n",
            if prelude.skills_enabled {
                "enabled"
            } else {
                "disabled"
            },
            prelude.skill_dirs.len(),
            if prelude.include_workspace_skills {
                "on"
            } else {
                "off"
            },
            inspection.skills_count,
        ));
        for line in tars_core::usage::session_usage_lines(&inspection.usage) {
            text.push_str(&line);
            text.push('\n');
        }
        text
    };
    if output(&rendered) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn provider_name(definition: &tars_core::definition::SessionDefinition) -> &'static str {
    use tars_core::definition::Provider;
    match definition.provider {
        Provider::OpenAiChat => "openai-chat",
        Provider::OpenAiResponses => "openai-responses",
        Provider::Anthropic => "anthropic",
    }
}
