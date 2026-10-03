use super::command::{failure, output};
use super::managed_execution;
use super::options::RunOptions;
use causa::kernel::{Context, ControlError};
use causa::runtime::{TurnInterruption, TurnResult};
use std::io::{self, Write};
use std::process::ExitCode;
use tarz_core::application::{ClosePolicy, PreparedCreate, SessionHost};
use tarz_core::session::WorkResult;

pub(super) fn run(options: RunOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    runtime.block_on(execute(options))
}

async fn execute(options: RunOptions) -> ExitCode {
    let context = match &options.load {
        Some(path) => match tarz_core::material::load(path) {
            Ok(context) => context,
            Err(error) => {
                return early_failure(options.json, &format!("material load failed: {error}"));
            }
        },
        None => Context::new(),
    };
    let input = match managed_execution::input_text(options.input) {
        Ok(input) => input,
        Err(error) => return early_failure(options.json, error),
    };
    let host = SessionHost::new(None);
    let session = match host
        .create(PreparedCreate::transient(options.definition, context))
        .await
    {
        Ok(session) => session,
        Err(error) => return early_failure(options.json, &error.to_string()),
    };
    let report = match managed_execution::work(&session, input).await {
        Ok(report) => report,
        Err(error) => {
            let _ = host
                .close(&session.target(), ClosePolicy::DiscardLocal)
                .await;
            return early_failure(options.json, &error);
        }
    };
    let (status, text, mut error, mut code) = classify(&report.result);
    let mut saved = false;
    if let Some(path) = options.save {
        match session.read_materials() {
            Ok(materials) => match tarz_core::material::save(&path, &materials.context) {
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
    let rendered = if options.json {
        format!(
            "{}\n",
            serde_json::json!({
                "status": status, "text": text, "conversation_id": report.work.conversation_id.as_str(),
                "work_id": report.work.turn_id.0, "saved": saved, "error": error,
            })
        )
    } else if status == "completed" {
        format!("{text}\n")
    } else {
        String::new()
    };
    if let Some(error) = error {
        let _ = writeln!(io::stderr().lock(), "Tarz: {error}");
    }
    if output(&rendered) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}

pub(super) fn classify(result: &WorkResult) -> (&'static str, &str, Option<&'static str>, u8) {
    match result {
        WorkResult::Run(TurnResult::Completed { final_output }) => {
            ("completed", &final_output.response.text.0, None, 0)
        }
        WorkResult::StoppedBeforeRun(ControlError::Cancelled)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::Cancelled { .. },
        }) => ("cancelled", "", Some("work cancelled"), 130),
        WorkResult::StoppedBeforeRun(ControlError::TimedOut)
        | WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::DeadlineExceeded { .. },
        }) => ("timed_out", "", Some("work deadline exceeded"), 124),
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::ModelCallFailed { .. },
        }) => (
            "failed",
            "",
            Some("model request failed; check provider, endpoint, model and credential"),
            1,
        ),
        _ => ("failed", "", Some("work failed"), 1),
    }
}

pub(super) fn early_failure(json: bool, error: &str) -> ExitCode {
    if json {
        let _ = output(&format!(
            "{}\n",
            serde_json::json!({ "status": "not_started",
            "text": "", "conversation_id": null, "work_id": null, "saved": false, "error": error })
        ));
    }
    failure(error)
}
