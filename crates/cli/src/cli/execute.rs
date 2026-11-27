use super::command::{failure, output};
use super::managed_execution;
use super::options::RunOptions;
use super::output_format::OutputFormat;
use causa::runtime::TurnResult;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tars_core::application::{
    ApplicationError, ClosePolicy, PreparedCreate, SessionHost, WorkOutcome,
};
use tars_core::definition::SessionDefinition;
use tars_core::external_tools::ExternalTools;
use tars_core::session::{ModelFailure, WorkResult};

pub(super) struct TransientRunRequest {
    pub definition: SessionDefinition,
    pub external_tools: ExternalTools,
    pub model_profiles: std::collections::BTreeMap<String, tars_core::agents::ModelProfile>,
    pub user_rules_path: Option<PathBuf>,
    pub credentials_path: Option<PathBuf>,
    pub input: Option<String>,
    pub used_stdin_input: bool,
    pub images: Vec<PathBuf>,
    pub load: Option<PathBuf>,
    pub save: Option<PathBuf>,
    pub output_format: OutputFormat,
}

pub(super) fn run(options: RunOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    let (code, abandoned) = runtime.block_on(async move {
        let input = match managed_execution::prepare_prompt_input(
            options.input,
            options.stdin,
            !options.images.is_empty(),
        ) {
            Ok(input) => input,
            Err(error) => {
                return (
                    early_failure_with_format(options.output_format, error),
                    false,
                );
            }
        };
        execute_transient(TransientRunRequest {
            definition: options.definition,
            external_tools: options.external_tools,
            model_profiles: options.model_profiles,
            user_rules_path: options.user_rules_path,
            credentials_path: options.credentials_path,
            input,
            used_stdin_input: options.stdin,
            images: options.images,
            load: options.load,
            save: options.save,
            output_format: options.output_format,
        })
        .await
    });
    if abandoned {
        // A save wait abandoned on the second interrupt may still hold
        // blocking I/O; do not make runtime teardown wait for it.
        runtime.shutdown_background();
    }
    code
}

pub(super) fn run_transient(request: TransientRunRequest) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    let (code, abandoned) = runtime.block_on(execute_transient(request));
    if abandoned {
        runtime.shutdown_background();
    }
    code
}

async fn execute_transient(request: TransientRunRequest) -> (ExitCode, bool) {
    let output_format = request.output_format;
    let images = match managed_execution::prepare_images(&request.images) {
        Ok(images) => images,
        Err(error) => return (early_failure_with_format(output_format, &error), false),
    };
    let imported_context = match &request.load {
        Some(path) => match tars_core::material::load(path) {
            Ok(context) => Some(context),
            Err(error) => {
                return (
                    early_failure_with_format(
                        output_format,
                        &format!("material load failed: {error}"),
                    ),
                    false,
                );
            }
        },
        None => None,
    };
    let prepared = match PreparedCreate::prepare_transient(request.definition, imported_context) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                early_failure_with_format(output_format, &error.to_string()),
                false,
            );
        }
    };
    let initial_diagnostics = prepared.prelude_diagnostics().to_vec();
    managed_execution::report_prelude_diagnostics(&initial_diagnostics);
    let host = SessionHost::new(None)
        .with_external_tools(request.external_tools)
        .with_user_rules_path(request.user_rules_path)
        .with_credentials_path(request.credentials_path)
        .with_model_profiles(request.model_profiles);
    let session = match host.create(prepared).await {
        Ok(session) => session,
        Err(error) => {
            return (
                early_failure_with_format(output_format, &application_error_message(&error)),
                false,
            );
        }
    };
    managed_execution::report_workspace_rules_notice(&session);
    managed_execution::report_workspace_agents_notice(&session);
    managed_execution::report_mcp_status(&session);
    managed_execution::report_command_brief(&session);
    let connect_approvals = super::approval::should_connect_stdio(request.used_stdin_input);
    let report = match managed_execution::work(
        &session,
        request.input,
        &images,
        connect_approvals,
        output_format,
    )
    .await
    {
        Ok(report) => report,
        Err(error) => {
            let _ = host
                .close(&session.target(), ClosePolicy::DiscardLocal)
                .await;
            return (early_failure_with_format(output_format, &error), false);
        }
    };
    let extra_diagnostics: Vec<_> = session
        .prelude_diagnostics()
        .into_iter()
        .filter(|diag| !initial_diagnostics.contains(diag))
        .collect();
    managed_execution::report_prelude_diagnostics(&extra_diagnostics);
    let presented = present_outcome(&report.result);
    let status = presented.status;
    let kind = presented.kind;
    let text = presented.text;
    let mut error = presented.error;
    let mut code = presented.code;
    let model_error = presented.model_failure;
    let detail = presented.detail;
    let save_wait_abandoned = matches!(
        report.save,
        managed_execution::SaveWait::Abandoned | managed_execution::SaveWait::CancelWaitTimedOut
    );
    match &report.save {
        managed_execution::SaveWait::Abandoned => {
            error = Some("stopped waiting for saving after a second interrupt".to_owned());
            code = 1;
        }
        managed_execution::SaveWait::CancelWaitTimedOut => {
            error = Some("timed out waiting for cancelled execution to settle".to_owned());
            code = 1;
        }
        managed_execution::SaveWait::Finished(Err(save_err)) if error.is_none() => {
            error = Some(save_err.to_string());
            code = 1;
        }
        managed_execution::SaveWait::Finished(_) => {}
    }
    let mut saved = false;
    if let Some(path) = request.save {
        match session.read_materials() {
            Ok(materials) => {
                let context = materials.detached_context();
                match tars_core::material::save(&path, &context) {
                    Ok(()) => saved = true,
                    Err(_) => {
                        error = Some("material save failed".to_owned());
                        code = 1;
                    }
                }
            }
            Err(_) => {
                error = Some("materials unavailable after worker failure".to_owned());
                code = 1;
            }
        }
    }
    if host
        .close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .is_err()
    {
        error = Some("session close failed".to_owned());
        code = 1;
    }
    if report.signal_failed {
        error = Some("signal listener failed".to_owned());
        code = 1;
    }
    let usage = report
        .usage
        .as_ref()
        .map(crate::cli::usage_json::work_usage_value);
    let json_value = serde_json::json!({
        "status": status, "kind": kind, "text": text, "conversation_id": report.work.conversation_id.as_str(),
        "work_id": report.work.turn_id.0, "saved": saved, "error": error,
        "error_kind": model_error.as_ref().map(|failure| failure.kind),
        "error_detail": model_error.as_ref().map(|failure| failure.detail.as_str()),
        "usage": usage,
    });
    let rendered = format_terminal_output(output_format, status, &text, &report, json_value);
    if let Some(error) = error {
        let _ = writeln!(io::stderr().lock(), "TARS: {error}");
    }
    report_detail(detail.as_deref());
    if report.stream_write_failed || output(&rendered) != ExitCode::SUCCESS {
        return (ExitCode::FAILURE, save_wait_abandoned);
    }
    (ExitCode::from(code), save_wait_abandoned)
}

pub(super) fn format_terminal_output(
    format: OutputFormat,
    status: &str,
    text: &str,
    report: &managed_execution::WorkReport,
    mut json_value: serde_json::Value,
) -> String {
    match format {
        OutputFormat::Json => format!("{json_value}\n"),
        OutputFormat::StreamJson => {
            if let Some(obj) = json_value.as_object_mut() {
                obj.insert("type".to_owned(), serde_json::json!("result"));
            }
            format!("{json_value}\n")
        }
        OutputFormat::Stream => {
            if status != "completed" {
                String::new()
            } else if report.streamed_text {
                if report.streamed_ends_with_newline {
                    String::new()
                } else {
                    "\n".to_owned()
                }
            } else if !text.is_empty() {
                format!("{text}\n")
            } else {
                String::new()
            }
        }
        OutputFormat::Text => {
            if status == "completed" {
                format!("{text}\n")
            } else {
                String::new()
            }
        }
    }
}

pub(super) struct CliOutcome {
    pub(super) status: &'static str,
    pub(super) kind: &'static str,
    pub(super) text: String,
    pub(super) error: Option<String>,
    pub(super) detail: Option<String>,
    pub(super) model_failure: Option<ModelFailure>,
    pub(super) code: u8,
}

pub(super) fn present_outcome(result: &WorkResult) -> CliOutcome {
    let outcome = WorkOutcome::of(result);
    let text = match result {
        WorkResult::Run(TurnResult::Completed { final_output }) => final_output.response.text(),
        _ => String::new(),
    };
    let (status, code) = match outcome.kind {
        "completed" => ("completed", 0),
        "cancelled" => ("cancelled", 130),
        "timed_out" => ("timed_out", 124),
        _ => ("failed", 1),
    };
    let error = if outcome.kind == "completed" {
        None
    } else {
        Some(format_cli_error(&outcome))
    };
    CliOutcome {
        status,
        kind: outcome.kind,
        text,
        error,
        detail: outcome.detail,
        model_failure: outcome.model_failure,
        code,
    }
}

pub(super) fn format_cli_error(outcome: &WorkOutcome) -> String {
    let mut line = outcome.summary.clone();
    if let Some(hint) = &outcome.hint {
        line.push_str("; ");
        line.push_str(hint);
    }
    if outcome.kind == "context_window_exceeded" {
        line.push_str(" (run `tars session compact`)");
    }
    line
}

pub(super) fn application_error_message(error: &ApplicationError) -> String {
    match error {
        ApplicationError::Assembly(asm_error) => {
            let outcome = WorkOutcome::from_assembly_error(asm_error);
            let mut message = format_cli_error(&outcome);
            if let Some(detail) = &outcome.detail {
                let escaped = tars_core::permission::escape_untrusted_text(detail);
                message.push_str(&format!(" ({escaped})"));
            }
            message
        }
        other => other.to_string(),
    }
}

/// Prints the raw error detail associated with a failed work outcome.
pub(super) fn report_detail(detail: Option<&str>) {
    if let Some(detail) = detail {
        let escaped = tars_core::permission::escape_untrusted_text(detail);
        let _ = writeln!(io::stderr().lock(), "TARS: detail: {escaped}");
    }
}

pub(super) fn early_failure(json: bool, error: &str) -> ExitCode {
    let format = if json {
        OutputFormat::Json
    } else {
        OutputFormat::Text
    };
    early_failure_with_format(format, error)
}

pub(super) fn early_failure_with_format(format: OutputFormat, error: &str) -> ExitCode {
    match format {
        OutputFormat::Json => {
            let _ = output(&format!(
                "{}\n",
                serde_json::json!({
                    "status": "not_started",
                    "text": "",
                    "conversation_id": null,
                    "work_id": null,
                    "saved": false,
                    "error": error,
                    "usage": null,
                })
            ));
        }
        OutputFormat::StreamJson => {
            let _ = output(&format!(
                "{}\n",
                serde_json::json!({
                    "type": "result",
                    "status": "not_started",
                    "text": "",
                    "conversation_id": null,
                    "work_id": null,
                    "saved": false,
                    "error": error,
                    "usage": null,
                })
            ));
        }
        OutputFormat::Text | OutputFormat::Stream => {}
    }
    failure(error)
}
