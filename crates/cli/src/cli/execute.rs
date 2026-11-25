use super::command::{failure, output};
use super::managed_execution;
use super::options::RunOptions;
use causa::runtime::TurnResult;
use std::io::{self, Write};
use std::process::ExitCode;
use tars_core::application::{
    ApplicationError, ClosePolicy, PreparedCreate, SessionHost, WorkOutcome,
};
use tars_core::session::{ModelFailure, WorkResult};

pub(super) fn run(options: RunOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    let (code, abandoned) = runtime.block_on(execute(options));
    if abandoned {
        // A save wait abandoned on the second interrupt may still hold
        // blocking I/O; do not make runtime teardown wait for it.
        runtime.shutdown_background();
    }
    code
}

async fn execute(options: RunOptions) -> (ExitCode, bool) {
    let imported_context = match &options.load {
        Some(path) => match tars_core::material::load(path) {
            Ok(context) => Some(context),
            Err(error) => {
                return (
                    early_failure(options.json, &format!("material load failed: {error}")),
                    false,
                );
            }
        },
        None => None,
    };
    let used_stdin_input = options.input.is_none();
    let input = match managed_execution::input_text(options.input) {
        Ok(input) => input,
        Err(error) => return (early_failure(options.json, error), false),
    };
    let prepared = match PreparedCreate::prepare_transient(options.definition, imported_context) {
        Ok(prepared) => prepared,
        Err(error) => return (early_failure(options.json, &error.to_string()), false),
    };
    let initial_diagnostics = prepared.prelude_diagnostics().to_vec();
    managed_execution::report_prelude_diagnostics(&initial_diagnostics);
    let host = SessionHost::new(None)
        .with_external_tools(options.external_tools)
        .with_user_rules_path(options.user_rules_path)
        .with_model_profiles(options.model_profiles);
    let session = match host.create(prepared).await {
        Ok(session) => session,
        Err(error) => {
            return (
                early_failure(options.json, &application_error_message(&error)),
                false,
            );
        }
    };
    managed_execution::report_workspace_rules_notice(&session);
    managed_execution::report_workspace_agents_notice(&session);
    managed_execution::report_mcp_status(&session);
    managed_execution::report_command_brief(&session);
    let connect_approvals = super::approval::should_connect_stdio(used_stdin_input);
    let report = match managed_execution::work(&session, input, connect_approvals).await {
        Ok(report) => report,
        Err(error) => {
            let _ = host
                .close(&session.target(), ClosePolicy::DiscardLocal)
                .await;
            return (early_failure(options.json, &error), false);
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
    if let Some(path) = options.save {
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
    let rendered = if options.json {
        format!(
            "{}\n",
            serde_json::json!({
                "status": status, "kind": kind, "text": text, "conversation_id": report.work.conversation_id.as_str(),
                "work_id": report.work.turn_id.0, "saved": saved, "error": error,
                "error_kind": model_error.as_ref().map(|failure| failure.kind),
                "error_detail": model_error.as_ref().map(|failure| failure.detail.as_str()),
                "usage": usage,
            })
        )
    } else if status == "completed" {
        format!("{text}\n")
    } else {
        String::new()
    };
    if let Some(error) = error {
        let _ = writeln!(io::stderr().lock(), "TARS: {error}");
    }
    report_detail(detail.as_deref());
    if output(&rendered) != ExitCode::SUCCESS {
        return (ExitCode::FAILURE, save_wait_abandoned);
    }
    (ExitCode::from(code), save_wait_abandoned)
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
    if json {
        let _ = output(&format!(
            "{}\n",
            serde_json::json!({ "status": "not_started",
            "text": "", "conversation_id": null, "work_id": null, "saved": false, "error": error,
            "usage": null })
        ));
    }
    failure(error)
}
