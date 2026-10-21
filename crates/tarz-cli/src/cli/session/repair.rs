use super::execute::{close_value, report_confirmation, revision};
use crate::cli::{command::output, execute::early_failure, managed_execution};
use serde_json::json;
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};
use tarz_core::application::{ClosePolicy, RepairError, RepairMode, SaveStatus, SessionHost};

pub(super) async fn run(
    store: PathBuf,
    id: String,
    apply: bool,
    expected_revision: Option<u64>,
    recovery_dir: PathBuf,
    json_output: bool,
) -> ExitCode {
    let host = SessionHost::new(Some(store));
    let session = match host.open(id.clone().into()).await {
        Ok(session) => session,
        Err(error) => return early_failure(json_output, &error.to_string()),
    };
    let current_revision = revision(&session.status().save).expect("opened persistent revision");
    let mut value = json!({
        "status": "preview", "conversation_id": id, "mode": "remove_unpaired",
        "revision": current_revision, "expected_revision": expected_revision,
        "uncommitted_results_available": false, "diagnosis": null, "plan": null,
        "applied": false, "saved": true, "error": null, "save_error": null,
        "recovery_file": null, "recovery_error": null, "close_error": null,
    });
    let planned = (|| {
        if let Some(expected) = expected_revision
            && expected != current_revision
        {
            return Err(RepairError::RevisionConflict {
                expected,
                actual: current_revision,
            }
            .into());
        }
        let inspection = session.inspect_repair(None)?;
        value["diagnosis"] = json!(inspection.diagnosis);
        if inspection.diagnosis.is_ready() {
            return Ok(None);
        }
        session.plan_repair(RepairMode::RemoveUnpaired).map(Some)
    })();
    let mut code = 0;
    let mut policy = ClosePolicy::Save;
    match planned {
        Err(error) => {
            code = 1;
            value["status"] = json!("rejected");
            value["error"] = json!(error.to_string());
        }
        Ok(None) => {
            value["status"] = json!("not_needed");
        }
        Ok(Some(plan)) => {
            value["plan"] = json!(plan.summary());
            if apply {
                match session.apply_repair(&plan) {
                    Err(error) => {
                        code = 1;
                        value["status"] = json!("rejected");
                        value["error"] = json!(error.to_string());
                    }
                    Ok(_) => {
                        value["applied"] = json!(true);
                        value["status"] = json!("applied");
                        match session.wait_saved().await {
                            Ok(save) => {
                                value["revision"] = json!(revision(&save));
                            }
                            Err(error) => {
                                code = 1;
                                policy = ClosePolicy::DiscardLocal;
                                value["saved"] = json!(false);
                                value["error"] = json!("session save failed");
                                value["save_error"] = json!(error.to_string());
                                match managed_execution::recovery(&session, &recovery_dir) {
                                    Ok(path) => value["recovery_file"] = json!(path),
                                    Err(error) => value["recovery_error"] = json!(error),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    match host.close(&session.target(), policy).await {
        Ok(outcome) => {
            report_confirmation(&outcome);
            if matches!(outcome.save, SaveStatus::Saved { .. }) {
                value["saved"] = json!(true);
            }
            if let Some(revision) = revision(&outcome.save) {
                value["revision"] = json!(revision);
            }
            value["close_outcome"] = close_value(&outcome);
        }
        Err(error) => {
            code = 1;
            value["close_error"] = json!(error.to_string());
        }
    }
    if let Some(error) = value["error"].as_str() {
        let _ = writeln!(io::stderr().lock(), "Tarz: {error}");
    }
    let rendered = if json_output {
        format!("{value}\n")
    } else {
        let mut text = format!(
            "{} · current materials saved: {} · record revision {}\n",
            value["status"].as_str().unwrap(),
            value["saved"],
            value["revision"]
        );
        if let Some(count) = value["plan"]["removed_count"].as_u64() {
            text.push_str(&format!("Remove {count} unpaired declarations from future requests. This does not establish non-execution.\n"));
            if let Some(ids) = value["plan"]["removed"].as_array() {
                for id in ids {
                    text.push_str(&format!("  Declaration {id}\n"));
                }
                if count > ids.len() as u64 {
                    text.push_str("  Identity display limited to 128 declarations.\n");
                }
            }
        }
        if let Some(count) = value["plan"]["write_warnings_count"].as_u64()
            && count > 0
        {
            text.push_str(&format!(
                "{count} write call(s) may have changed files before interruption; read them again before continuing.\n"
            ));
            if let Some(warnings) = value["plan"]["write_warnings"].as_array() {
                for warning in warnings {
                    let tool = warning["tool_name"].as_str().unwrap_or("write");
                    let path = warning["path"].as_str().unwrap_or("");
                    if path.is_empty() {
                        text.push_str(&format!(
                            "  {tool} may have changed the file; read it again before continuing\n"
                        ));
                    } else {
                        text.push_str(&format!("  {tool} {path} may have changed the file; read it again before continuing\n"));
                    }
                }
                if count > warnings.len() as u64 {
                    text.push_str("  Write warning display limited to 128 items.\n");
                }
            }
        }
        if !apply && value["status"] == "preview" {
            text.push_str(&format!("Apply explicitly with --apply --expected-revision {current_revision}; no new work will be sent.\n"));
        }
        if let Some(path) = value["recovery_file"].as_str() {
            text.push_str(&format!("Recovery materials: {path}\n"));
        }
        for field in ["save_error", "recovery_error", "close_error"] {
            if let Some(error) = value[field].as_str() {
                text.push_str(&format!("{field}: {error}\n"));
            }
        }
        text
    };
    if output(&rendered) != ExitCode::SUCCESS {
        return ExitCode::FAILURE;
    }
    ExitCode::from(code)
}
