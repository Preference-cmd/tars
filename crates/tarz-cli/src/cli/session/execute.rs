use super::options::SessionCommand;
use crate::cli::command::{failure, output};
use crate::cli::execute::{classify, early_failure, model_failure, report_detail};
use crate::cli::managed_execution;
use causa::kernel::Context;
use causa::runtime::new_block_id;
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use tarz_core::application::{
    CloseOutcome, ClosePolicy, CommitDisposition, Lifecycle, PreparedCreate, SaveStatus,
    SessionHost,
};
use tarz_core::definition::SessionDefinition;

pub(crate) fn run(command: SessionCommand) -> ExitCode {
    if let SessionCommand::ExportMaterials {
        record,
        output: path,
    } = command
    {
        let context = match tarz_core::material::load_session_materials(&record) {
            Ok(context) => context,
            Err(error) => return failure(error),
        };
        return match tarz_core::material::save_new(&path, &context) {
            Ok(()) => output(&format!("{}\n", path.display())),
            Err(error) => failure(error),
        };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    runtime.block_on(async {
        match command {
            SessionCommand::Create {
                store,
                definition,
                load,
                json,
            } => create(store, *definition, load, json).await,
            SessionCommand::Run {
                store,
                id,
                input,
                recovery_dir,
                json,
            } => run_saved(store, id, input, recovery_dir, json).await,
            SessionCommand::List { store, json } => list(store, json).await,
            SessionCommand::Repair {
                store,
                id,
                apply,
                expected_revision,
                recovery_dir,
                json,
            } => super::repair::run(store, id, apply, expected_revision, recovery_dir, json).await,
            SessionCommand::ExportMaterials { .. } => {
                unreachable!("offline export handled before runtime")
            }
        }
    })
}

async fn create(
    store: PathBuf,
    definition: SessionDefinition,
    load: Option<PathBuf>,
    json_output: bool,
) -> ExitCode {
    let context = match load {
        Some(path) => match tarz_core::material::load(path) {
            Ok(context) => context,
            Err(_) => return early_failure(json_output, "initial material load failed"),
        },
        None => Context::new(),
    };
    let prepared = PreparedCreate::persistent(definition, context.clone());
    let id = prepared.conversation_id().clone();
    let host = SessionHost::new(Some(store));
    let session = match host.create(prepared).await {
        Ok(session) => session,
        Err(error) => {
            let status = host.lookup(&id);
            if let Some(status) = status
                && status.lifecycle == Lifecycle::InitializationBlocked
            {
                let recovered = export_initial_recovery(&context);
                let close = host.close(&status.target, ClosePolicy::DiscardLocal).await;
                let saved_revision = close
                    .as_ref()
                    .ok()
                    .and_then(|outcome| revision(&outcome.save));
                if let Ok(outcome) = &close {
                    report_confirmation(outcome);
                }
                if json_output {
                    let _ = output(&format!(
                        "{}\n",
                        json!({
                            "status":"not_started", "conversation_id":id.as_str(),
                            "work_id":null,"text":"","saved":saved_revision.is_some(),"revision":saved_revision,"error":"initial session save failed",
                            "save_error":error.to_string(),
                            "recovery_file":recovered.as_ref().ok(),
                            "recovery_error":recovered.as_ref().err(),
                            "close_error":close.as_ref().err().map(|e|e.to_string()),
                            "close_outcome":close.as_ref().ok().map(close_value),
                        })
                    ));
                }
                match recovered {
                    Ok(path) => {
                        let _ = writeln!(
                            io::stderr().lock(),
                            "Tarz: initial save failed; recovery materials: {}",
                            path.display()
                        );
                    }
                    Err(error) => {
                        let _ = writeln!(io::stderr().lock(), "Tarz: initial save failed; {error}");
                    }
                }
                return ExitCode::FAILURE;
            }
            return early_failure(json_output, &error.to_string());
        }
    };
    let revision = revision(&session.status().save);
    if let Err(error) = host.close(&session.target(), ClosePolicy::Save).await {
        return early_failure(json_output, &error.to_string());
    }
    if json_output {
        output(&format!(
            "{}\n",
            json!({"status":"created","conversation_id":id.as_str(),"saved":true,"revision":revision})
        ))
    } else {
        output(&format!("{}\n", id.as_str()))
    }
}

fn export_initial_recovery(context: &Context) -> Result<PathBuf, &'static str> {
    let path = std::env::current_dir()
        .map_err(|_| "recovery directory is unavailable")?
        .join(format!("tarz-recovery-{}.json", new_block_id().0));
    tarz_core::material::save_new(&path, context).map_err(|_| "recovery material export failed")?;
    Ok(path)
}

async fn run_saved(
    store: PathBuf,
    id: String,
    input: Option<String>,
    recovery_dir: PathBuf,
    json_output: bool,
) -> ExitCode {
    let input = match managed_execution::input_text(input) {
        Ok(input) => input,
        Err(error) => return early_failure(json_output, error),
    };
    let host = SessionHost::new(Some(store));
    let session = match host.open(id.into()).await {
        Ok(session) => session,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let report = match managed_execution::work(&session, input).await {
        Ok(report) => report,
        Err(error) => {
            let _ = host.close(&session.target(), ClosePolicy::Save).await;
            return early_failure(json_output, &error);
        }
    };
    let (status, text, execution_error, mut code) = classify(&report.result);
    let model_error = model_failure(&report.result);
    let mut saved_revision = report.save.as_ref().ok().and_then(revision);
    let save_error = report.save.as_ref().err().map(ToString::to_string);
    let mut recovery_file = None;
    let mut recovery_error = None;
    if save_error.is_some() {
        code = 1;
        match managed_execution::recovery(&session, &recovery_dir) {
            Ok(path) => recovery_file = Some(path),
            Err(error) => recovery_error = Some(error),
        }
    }
    let policy = if save_error.is_some() {
        ClosePolicy::DiscardLocal
    } else {
        ClosePolicy::Save
    };
    let close = host.close(&session.target(), policy).await;
    let close_error = close.as_ref().err().map(|error| error.to_string());
    if let Ok(outcome) = &close {
        saved_revision = revision(&outcome.save).or(saved_revision);
        report_confirmation(outcome);
    }
    if close_error.is_some() || report.signal_failed {
        code = 1;
    }
    let error = if save_error.is_some() {
        Some("session save failed")
    } else if close_error.is_some() {
        Some("session close failed")
    } else if report.signal_failed {
        Some("signal listener failed")
    } else {
        execution_error
    };
    if let Some(path) = &recovery_file {
        let _ = writeln!(
            io::stderr().lock(),
            "Tarz: recovery materials: {}",
            path.display()
        );
    }
    if let Some(error) = recovery_error {
        let _ = writeln!(
            io::stderr().lock(),
            "Tarz: {error}; no recovery file was created"
        );
    }
    if let Some(error) = error {
        let _ = writeln!(io::stderr().lock(), "Tarz: {error}");
    }
    report_detail(model_error.as_ref());
    let rendered = if json_output {
        format!(
            "{}\n",
            json!({
                "status":status,"text":text,"conversation_id":report.work.conversation_id.as_str(),
                "work_id":report.work.turn_id.0,"saved":saved_revision.is_some(),"revision":saved_revision,
                "error":error,"execution_error":execution_error,"save_error":save_error,
                "error_kind":model_error.as_ref().map(|failure| failure.kind),
                "error_detail":model_error.as_ref().map(|failure| failure.detail.as_str()),
                "recovery_file":recovery_file,"recovery_error":recovery_error,"close_error":close_error,
                "close_outcome":close.as_ref().ok().map(close_value),
            })
        )
    } else if status == "completed" {
        format!("{text}\n")
    } else {
        String::new()
    };
    if output(&rendered) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}

async fn list(store: PathBuf, json_output: bool) -> ExitCode {
    let host = SessionHost::new(Some(store));
    let entries = match host.list().await {
        Ok(entries) => entries,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let mut errors = false;
    let values: Vec<_> = entries
        .into_iter()
        .map(|entry| {
            let record_error = entry.record.as_ref().err().map(ToString::to_string);
            let active_error = entry.active.as_ref().err().map(ToString::to_string);
            errors |= record_error.is_some() || active_error.is_some();
            json!({
                "conversation_id":entry.conversation_id.as_str(),
                "revision":entry.record.as_ref().ok().map(|r|r.revision),
                "definition":entry.record.as_ref().ok().map(|r|&r.definition),
                "block_count":entry.record.as_ref().ok().map(|r|r.block_count),
                "active":entry.active.ok(),"record_error":record_error,"active_error":active_error,
            })
        })
        .collect();
    let rendered = if json_output {
        format!("{}\n", json!({"sessions":values}))
    } else {
        values
            .iter()
            .map(|entry| {
                format!(
                    "{}\t{}\t{}\n",
                    entry["conversation_id"].as_str().unwrap_or("?"),
                    entry["revision"],
                    if !entry["record_error"].is_null() {
                        "invalid"
                    } else if entry["active"] == true {
                        "active"
                    } else {
                        "saved"
                    }
                )
            })
            .collect::<String>()
    };
    if output(&rendered) != ExitCode::SUCCESS || errors {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

pub(super) fn revision(save: &SaveStatus) -> Option<u64> {
    match save {
        SaveStatus::Saved { revision, .. } => Some(*revision),
        _ => None,
    }
}

pub(super) fn close_value(outcome: &CloseOutcome) -> serde_json::Value {
    let (pending, pending_revision, confirmation_error) = match &outcome.pending_commit {
        None => (None, None, None),
        Some(CommitDisposition::Applied { revision }) => (Some("applied"), Some(*revision), None),
        Some(CommitDisposition::NotApplied) => (Some("not_applied"), None, None),
        Some(CommitDisposition::NotSubmitted) => (Some("not_submitted"), None, None),
        Some(CommitDisposition::Unresolved { error }) => {
            (Some("unresolved"), None, Some(error.to_string()))
        }
    };
    json!({
        "policy": match outcome.policy { ClosePolicy::Save => "save", ClosePolicy::DiscardLocal => "discard_local" },
        "saved_revision": revision(&outcome.save), "pending_commit": pending,
        "pending_revision": pending_revision, "confirmation_error": confirmation_error,
        "discarded_unconfirmed": outcome.discarded_unconfirmed,
    })
}

pub(super) fn report_confirmation(outcome: &CloseOutcome) {
    if let Some(pending) = &outcome.pending_commit {
        let message = match pending {
            CommitDisposition::Applied { .. } => {
                "the original pending save was confirmed while closing"
            }
            CommitDisposition::NotApplied => "the original pending save was confirmed not applied",
            CommitDisposition::NotSubmitted => "the pending save had not been submitted",
            CommitDisposition::Unresolved { .. } => {
                "session closed, but the original save outcome remains unconfirmed"
            }
        };
        let _ = writeln!(io::stderr().lock(), "Tarz: {message}");
    }
}
