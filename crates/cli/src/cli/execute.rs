use super::command::{failure, output};
use super::managed_execution;
use super::options::RunOptions;
use causa::kernel::{Context, ControlError};
use causa::runtime::{TurnInterruption, TurnResult};
use std::io::{self, Write};
use std::process::ExitCode;
use tars_core::application::{ClosePolicy, PreparedCreate, SessionHost};
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
    let context = match &options.load {
        Some(path) => match tars_core::material::load(path) {
            Ok(context) => context,
            Err(error) => {
                return (
                    early_failure(options.json, &format!("material load failed: {error}")),
                    false,
                );
            }
        },
        None => Context::new(),
    };
    let used_stdin_input = options.input.is_none();
    let input = match managed_execution::input_text(options.input) {
        Ok(input) => input,
        Err(error) => return (early_failure(options.json, error), false),
    };
    let host = SessionHost::new(None).with_external_tools(options.external_tools);
    let session = match host
        .create(PreparedCreate::transient(options.definition, context))
        .await
    {
        Ok(session) => session,
        Err(error) => return (early_failure(options.json, &error.to_string()), false),
    };
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
    let (status, text, mut error, mut code) = classify(&report.result);
    let model_error = model_failure(&report.result);
    let save_wait_abandoned = matches!(report.save, managed_execution::SaveWait::Abandoned);
    if save_wait_abandoned {
        error = Some("stopped waiting for saving after a second interrupt");
        code = 1;
    }
    let mut saved = false;
    if let Some(path) = options.save {
        match session.read_materials() {
            Ok(materials) => match tars_core::material::save(&path, &materials.context) {
                Ok(()) => saved = true,
                Err(_) => {
                    error = Some("material save failed");
                    code = 1;
                }
            },
            Err(_) => {
                error = Some("materials unavailable after worker failure");
                code = 1;
            }
        }
    }
    if host
        .close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .is_err()
    {
        error = Some("session close failed");
        code = 1;
    }
    if report.signal_failed {
        error = Some("signal listener failed");
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
                "status": status, "text": text, "conversation_id": report.work.conversation_id.as_str(),
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
    report_detail(model_error.as_ref());
    if output(&rendered) != ExitCode::SUCCESS {
        return (ExitCode::FAILURE, save_wait_abandoned);
    }
    (ExitCode::from(code), save_wait_abandoned)
}

pub(super) fn classify(result: &WorkResult) -> (&'static str, String, Option<&'static str>, u8) {
    match result {
        WorkResult::Run(TurnResult::Completed { final_output }) => {
            ("completed", final_output.response.text(), None, 0)
        }
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::Cancelled { .. },
        }) => ("cancelled", String::new(), Some("work cancelled"), 130),
        WorkResult::StoppedBeforeRun(ControlError::TimedOut)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::DeadlineExceeded { .. },
        }) => (
            "timed_out",
            String::new(),
            Some("work deadline exceeded"),
            124,
        ),
        WorkResult::Run(TurnResult::Interrupted { cause }) if stop_reason(cause).is_some() => {
            ("failed", String::new(), stop_reason(cause), 1)
        }
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelCallFailed { error, .. },
        }) => (
            "failed",
            String::new(),
            Some(ModelFailure::of(error).hint),
            1,
        ),
        _ => ("failed", String::new(), Some("work failed"), 1),
    }
}

/// The failed model call or unrecognized stop behind a result, if any.
pub(super) fn model_failure(result: &WorkResult) -> Option<ModelFailure> {
    match result {
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelCallFailed { error, .. },
        }) => Some(ModelFailure::of(error)),
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelStopUnrecognized { output, .. },
        }) => Some(ModelFailure::unrecognized_stop(output)),
        _ => None,
    }
}

/// Prints the gateway's own description of a failed model call.
pub(super) fn report_detail(failure: Option<&ModelFailure>) {
    if let Some(failure) = failure {
        let _ = writeln!(io::stderr().lock(), "TARS: detail: {}", failure.detail);
    }
}

/// A stop the user can act on by changing a limit or submitting new work.
fn stop_reason(cause: &TurnInterruption) -> Option<&'static str> {
    Some(match cause {
        TurnInterruption::ModelMaxTokens { .. } => {
            "model output reached max_tokens; raise the limit and submit new work"
        }
        TurnInterruption::ModelContextWindowExceeded { .. } => {
            "the conversation no longer fits the model's context window"
        }
        TurnInterruption::ModelRefusal { .. } => "the model refused the request",
        TurnInterruption::ModelStopUnrecognized { .. } => {
            "the provider stopped for a reason this version does not recognize"
        }
        TurnInterruption::MaxModelRounds { .. } => {
            "model round limit reached; submit new work to continue"
        }
        TurnInterruption::MaxToolCalls { .. } => {
            "tool call limit reached; submit new work to continue"
        }
        _ => return None,
    })
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
