use super::command::{failure, output};
use super::options::RunOptions;
use causa::kernel::{Context, ControlError};
use causa::runtime::{TurnInterruption, TurnResult};
use std::io::{self, Read, Write};
use std::process::ExitCode;
use tarz::application::{ModelConfig, run_once, text_input};
use tarz::session::{SessionConfig, WorkResult};
use tokio_util::sync::CancellationToken;

pub(super) fn run(options: RunOptions) -> ExitCode {
    match prepare(&options) {
        Ok((model, context, input)) => {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => return failure(error),
            };
            runtime.block_on(execute(options, model, context, input))
        }
        Err(error) => early_failure(options.json, &error),
    }
}

fn prepare(options: &RunOptions) -> Result<(ModelConfig, Context, String), String> {
    let api_key = std::env::var(&options.key_env)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "set the credential environment variable {}",
                options.key_env
            )
        })?;
    let context = match &options.load {
        Some(path) => {
            tarz::material::load(path).map_err(|e| format!("material load failed: {e}"))?
        }
        None => Context::default(),
    };
    let input = match &options.input {
        Some(input) => input.clone(),
        None => {
            let mut input = String::new();
            io::stdin()
                .read_to_string(&mut input)
                .map_err(|_| "cannot read UTF-8 stdin")?;
            if input.trim().is_empty() {
                return Err("stdin must not be blank".into());
            }
            input
        }
    };
    Ok((
        ModelConfig {
            provider: options.provider,
            model: options.model.clone(),
            api_key,
            endpoint: options.endpoint.clone(),
        },
        context,
        input,
    ))
}

async fn execute(
    options: RunOptions,
    model: ModelConfig,
    context: Context,
    input: String,
) -> ExitCode {
    let session = model.session(
        context,
        SessionConfig {
            retained_work_capacity: 1,
            work_timeout: Some(options.timeout),
        },
    );
    let cancellation = CancellationToken::new();
    let execution = run_once(session, vec![text_input(input)], cancellation.clone());
    tokio::pin!(execution);
    let mut signal_error = false;
    let report = tokio::select! {
        biased;
        report = &mut execution => report,
        signal = tokio::signal::ctrl_c() => {
            signal_error = signal.is_err();
            cancellation.cancel();
            execution.await
        },
    };
    let report = match report {
        Ok(report) => report,
        Err(error) => return early_failure(options.json, &error.to_string()),
    };
    let (status, text, mut error, mut code) = classify(&report.result);
    let mut saved = false;
    if let Some(path) = options.save {
        match report.materials {
            Ok(context) => match tarz::material::save(&path, &context) {
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
    if signal_error {
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

fn classify(result: &WorkResult) -> (&'static str, &str, Option<&'static str>, u8) {
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

fn early_failure(json: bool, error: &str) -> ExitCode {
    if json {
        let _ = output(&format!(
            "{}\n",
            serde_json::json!({ "status": "not_started",
            "text": "", "conversation_id": null, "work_id": null, "saved": false, "error": error })
        ));
    }
    failure(error)
}
