use super::options::SessionCommand;
use crate::cli::command::{failure, output};
use crate::cli::execute::{classify, early_failure, model_failure, report_detail};
use crate::cli::managed_execution::{self, RecoveryFile, SaveWait};
use causa::kernel::Context;
use causa::runtime::new_block_id;
use serde_json::json;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tars_core::application::{
    ApplicationError, CloseOutcome, ClosePolicy, CommitDisposition, Lifecycle, PreparedCreate,
    SaveStatus, SessionHost,
};
use tars_core::definition::SessionDefinition;

pub(crate) fn run(command: SessionCommand) -> ExitCode {
    if let SessionCommand::ExportMaterials {
        record,
        output: path,
    } = command
    {
        let context = match tars_core::material::load_session_materials(&record) {
            Ok(context) => context,
            Err(error) => return failure(error),
        };
        return match tars_core::material::save_new(&path, &context) {
            Ok(saved) => {
                if !saved.importable {
                    let _ = writeln!(
                        io::stderr().lock(),
                        "TARS: exported materials: {}{}",
                        path.display(),
                        managed_execution::recovery_limit_warning(false)
                    );
                }
                output(&format!("{}\n", path.display()))
            }
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
    let (code, abandoned) = runtime.block_on(async {
        match command {
            SessionCommand::Create {
                store,
                definition,
                load,
                recovery_dir,
                json,
            } => (
                create(store, *definition, load, recovery_dir, json).await,
                false,
            ),
            SessionCommand::Run {
                store,
                id,
                input,
                permission_mode,
                recovery_dir,
                json,
            } => run_saved(store, id, input, permission_mode, recovery_dir, json).await,
            SessionCommand::List { store, json } => (list(store, json).await, false),
            SessionCommand::Repair {
                store,
                id,
                apply,
                expected_revision,
                recovery_dir,
                json,
            } => (
                super::repair::run(store, id, apply, expected_revision, recovery_dir, json).await,
                false,
            ),
            SessionCommand::ExportMaterials { .. } => {
                unreachable!("offline export handled before runtime")
            }
        }
    });
    if abandoned {
        // A save that was abandoned may still be blocked in blocking I/O.
        runtime.shutdown_background();
    }
    code
}

async fn create(
    store: PathBuf,
    definition: SessionDefinition,
    load: Option<PathBuf>,
    recovery_dir: PathBuf,
    json_output: bool,
) -> ExitCode {
    let context = match load {
        Some(path) => match tars_core::material::load(path) {
            Ok(context) => context,
            Err(error) => {
                return early_failure(
                    json_output,
                    &format!("initial material load failed: {error}"),
                );
            }
        },
        None => Context::new(),
    };
    let prepared = PreparedCreate::persistent(definition, context.clone());
    let id = prepared.conversation_id().clone();
    // Creating saves the definition and closes; it starts no MCP server.
    let host = managed_execution::definition_host(store);
    let session = match host.create(prepared).await {
        Ok(session) => session,
        Err(error) => {
            let status = host.lookup(&id);
            if let Some(status) = status
                && status.lifecycle == Lifecycle::InitializationBlocked
            {
                let recovered = export_initial_recovery(&context, &recovery_dir);
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
                            "recovery_file":recovered.as_ref().ok().map(|file| &file.path),
                            "recovery_importable":recovered.as_ref().ok().map(|file| file.importable),
                            "recovery_error":recovered.as_ref().err(),
                            "close_error":close.as_ref().err().map(|e|e.to_string()),
                            "close_outcome":close.as_ref().ok().map(close_value),
                        })
                    ));
                }
                match recovered {
                    Ok(file) => {
                        let _ = writeln!(
                            io::stderr().lock(),
                            "TARS: initial save failed; recovery materials: {}{}",
                            file.path.display(),
                            managed_execution::recovery_limit_warning(file.importable)
                        );
                    }
                    Err(error) => {
                        let _ = writeln!(io::stderr().lock(), "TARS: initial save failed; {error}");
                    }
                }
                return ExitCode::FAILURE;
            }
            return early_failure(json_output, &error.to_string());
        }
    };
    let revision = revision(&session.status().save);
    managed_execution::report_command_details(&session);
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

fn export_initial_recovery(
    context: &Context,
    directory: &Path,
) -> Result<RecoveryFile, &'static str> {
    let path = directory.join(format!("tars-recovery-{}.json", new_block_id().0));
    let saved = tars_core::material::save_new(&path, context)
        .map_err(|_| "recovery material export failed")?;
    Ok(RecoveryFile {
        path,
        importable: saved.importable,
    })
}

async fn run_saved(
    store: PathBuf,
    id: String,
    input: Option<String>,
    permission_mode: Option<tars_core::permission::PermissionMode>,
    recovery_dir: PathBuf,
    json_output: bool,
) -> (ExitCode, bool) {
    let used_stdin_input = input.is_none();
    let input = match managed_execution::input_text(input) {
        Ok(input) => input,
        Err(error) => return (early_failure(json_output, error), false),
    };
    let host = SessionHost::new(Some(store));
    let session = match host.open(id.into()).await {
        Ok(session) => session,
        Err(error) => return (early_failure(json_output, &error.to_string()), false),
    };
    managed_execution::report_mcp_status(&session);
    managed_execution::report_command_brief(&session);
    if let Some(mode) = permission_mode {
        if let Err(error) = session.set_permission_mode(mode) {
            let _ = host
                .close(&session.target(), ClosePolicy::DiscardLocal)
                .await;
            return (early_failure(json_output, &error.to_string()), false);
        }
        if let Err(error) = session.wait_saved().await {
            let _ = host
                .close(&session.target(), ClosePolicy::DiscardLocal)
                .await;
            return (early_failure(json_output, &error.to_string()), false);
        }
    }
    // An explicit mode supersedes the saved mode for this invocation, so the
    // reopen fact is no longer actionable for the user.
    if permission_mode.is_none() {
        managed_execution::report_open_notices(&session);
    }
    let connect_approvals = crate::cli::approval::should_connect_stdio(used_stdin_input);
    let report = match managed_execution::work(&session, input, connect_approvals).await {
        Ok(report) => report,
        Err(error) => {
            let _ = host.close(&session.target(), ClosePolicy::Save).await;
            return (early_failure(json_output, &error), false);
        }
    };
    let (status, text, execution_error, mut code) = classify(&report.result);
    let model_error = model_failure(&report.result);
    let abandoned = matches!(report.save, SaveWait::Abandoned);
    let save_result = match &report.save {
        SaveWait::Finished(result) => Some(result),
        SaveWait::Abandoned => None,
    };
    let mut saved_revision = save_result
        .and_then(|result| result.as_ref().ok())
        .and_then(revision);
    let save_error = save_result
        .and_then(|result| result.as_ref().err())
        .map(ToString::to_string);
    let mut recovery_file = None;
    let mut recovery_importable = None;
    let mut recovery_error = None;
    if save_error.is_some() || abandoned {
        code = 1;
        match managed_execution::recovery(&session, &recovery_dir) {
            Ok(file) => {
                recovery_importable = Some(file.importable);
                recovery_file = Some(file.path);
            }
            Err(error) => recovery_error = Some(error),
        }
    }
    let policy = if save_error.is_some() || abandoned {
        ClosePolicy::DiscardLocal
    } else {
        ClosePolicy::Save
    };
    // An abandoned wait may leave the supervisor blocked in a save. Give close
    // a short bound, then leave the runtime without waiting for that I/O.
    let close = if abandoned {
        match tokio::time::timeout(
            Duration::from_secs(2),
            host.close(&session.target(), policy),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ApplicationError::Faulted(
                "session close is still blocked after interruption".into(),
            )),
        }
    } else {
        host.close(&session.target(), policy).await
    };
    let close_error = close.as_ref().err().map(|error| error.to_string());
    if let Ok(outcome) = &close {
        saved_revision = revision(&outcome.save).or(saved_revision);
        report_confirmation(outcome);
    }
    if close_error.is_some() || report.signal_failed {
        code = 1;
    }
    let error = if abandoned {
        Some("stopped waiting for saving after a second interrupt")
    } else if save_error.is_some() {
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
            "TARS: recovery materials: {}{}",
            path.display(),
            managed_execution::recovery_limit_warning(recovery_importable.unwrap_or(true))
        );
    }
    if let Some(error) = recovery_error {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: {error}; no recovery file was created"
        );
    }
    if let Some(error) = error {
        let _ = writeln!(io::stderr().lock(), "TARS: {error}");
    }
    report_detail(model_error.as_ref());
    let rendered = if json_output {
        format!(
            "{}\n",
            json!({
                "status":status,"text":text,"conversation_id":report.work.conversation_id.as_str(),
                "work_id":report.work.turn_id.0,"saved":saved_revision.is_some(),"revision":saved_revision,
                "error":error,"execution_error":execution_error,"save_error":save_error,
                "save_wait_abandoned":abandoned,
                "error_kind":model_error.as_ref().map(|failure| failure.kind),
                "error_detail":model_error.as_ref().map(|failure| failure.detail.as_str()),
                "recovery_file":recovery_file,"recovery_importable":recovery_importable,
                "recovery_error":recovery_error,"close_error":close_error,
                "close_outcome":close.as_ref().ok().map(close_value),
            })
        )
    } else if status == "completed" {
        format!("{text}\n")
    } else {
        String::new()
    };
    if output(&rendered) != ExitCode::SUCCESS {
        return (ExitCode::FAILURE, abandoned);
    }
    (ExitCode::from(code), abandoned)
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
        let _ = writeln!(io::stderr().lock(), "TARS: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::export_initial_recovery;
    use causa::kernel::Context;

    #[test]
    fn initial_recovery_uses_the_selected_directory() {
        let directory = tempfile::tempdir().unwrap();
        let file = export_initial_recovery(&Context::new(), directory.path()).unwrap();
        assert_eq!(file.path.parent(), Some(directory.path()));
        assert!(file.importable);
        assert!(tars_core::material::load(&file.path).is_ok());
    }
}
