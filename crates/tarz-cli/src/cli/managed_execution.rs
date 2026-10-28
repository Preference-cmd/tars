use causa::runtime::new_block_id;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tarz_core::McpServerStatus;
use tarz_core::application::{
    ApplicationError, HostOptions, ManagedSession, SaveStatus, SessionHost, text_input,
};
use tarz_core::session::{SubmitRequest, WorkRef, WorkResult, WorkState};

pub(super) struct WorkReport {
    pub work: WorkRef,
    pub result: Arc<WorkResult>,
    pub save: SaveWait,
    pub signal_failed: bool,
}

/// The independent save barrier either settled or was explicitly abandoned.
pub(super) enum SaveWait {
    Finished(Result<SaveStatus, ApplicationError>),
    Abandoned,
}

/// A new recovery material file.
pub(super) struct RecoveryFile {
    pub path: PathBuf,
    pub importable: bool,
}

/// The stderr/notice suffix for a recovery export that exceeds the import limit.
pub(super) fn recovery_limit_warning(importable: bool) -> &'static str {
    if importable {
        ""
    } else {
        " (exceeds the 64 MiB import limit; this version cannot import it directly)"
    }
}

/// Report a saved-mode downgrade that opening applied, once.
pub(super) fn report_open_notices(session: &ManagedSession) {
    if session.open_report().permission_downgraded {
        let _ = writeln!(
            io::stderr().lock(),
            "Tarz: reopened permission mode was downgraded from full-access to ask; the saved definition was not created with full-access"
        );
    }
}

/// A host for commands that save or repair a session and run no work: it
/// opens sessions without starting their MCP servers.
pub(super) fn definition_host(store: PathBuf) -> SessionHost {
    let options = HostOptions {
        connect_mcp_servers: false,
        ..HostOptions::default()
    };
    SessionHost::with_options(Some(store), options).expect("default bounds are positive")
}

/// Stderr lines for MCP servers that did not start and for tools left out of
/// the model's list. The status text is already bounded and escaped.
pub(super) fn mcp_notices(status: &[McpServerStatus]) -> Vec<String> {
    let mut lines = Vec::new();
    for server in status {
        if !server.ready {
            let reason = server.reason.as_deref().unwrap_or("unknown reason");
            lines.push(format!(
                "Tarz: MCP server '{}' is unavailable: {reason}",
                server.id
            ));
        }
        for tool in &server.skipped_tools {
            lines.push(format!(
                "Tarz: MCP tool skipped: {} ({})",
                tool.name, tool.reason
            ));
        }
    }
    lines
}

pub(super) fn report_mcp_status(session: &ManagedSession) {
    let mut stderr = io::stderr().lock();
    for line in mcp_notices(&session.mcp_status()) {
        let _ = writeln!(stderr, "{line}");
    }
}

pub(super) fn input_text(input: Option<String>) -> Result<String, &'static str> {
    let text = match input {
        Some(text) => text,
        None => {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|_| "cannot read UTF-8 stdin")?;
            text
        }
    };
    if text.trim().is_empty() {
        return Err("input must not be blank");
    }
    Ok(text)
}

pub(super) async fn work(
    session: &ManagedSession,
    input: String,
    connect_approvals: bool,
) -> Result<WorkReport, String> {
    let approver = if connect_approvals {
        Some(crate::cli::approval::spawn_approver(session))
    } else {
        None
    };
    let receipt = session
        .submit(SubmitRequest {
            request_key: new_block_id().0.to_string(),
            blocks: vec![text_input(input)],
        })
        .map_err(|error| error.to_string())?;
    let mut signal_failed = false;
    let mut stopping = false;
    let result = loop {
        let observation = tokio::select! {
            biased;
            observation = session.wait(&receipt.work, Duration::from_secs(60)) => {
                observation.map_err(|error| error.to_string())?
            },
            // Cancellation remains cooperative and is bounded by the work
            // deadline; the independent save barrier below has no such bound
            // and gets the explicit second-interrupt escape hatch.
            signal = tokio::signal::ctrl_c(), if !stopping => {
                signal_failed = signal.is_err();
                if let Some(approver) = &approver {
                    approver.on_cancel();
                }
                session.cancel(&receipt.work).map_err(|error| error.to_string())?;
                stopping = true;
                continue;
            }
        };
        if let WorkState::Finished(result) = observation.state {
            break result;
        }
    };
    if let Some(approver) = approver {
        approver.finish();
    }
    // A raw Finished observation precedes persistence. Keep the runtime alive
    // through the independent host save barrier, including cancelled work.
    let (save, wait_signal_failed) = wait_saved(session).await;
    Ok(WorkReport {
        work: receipt.work,
        result,
        save,
        signal_failed: signal_failed || wait_signal_failed,
    })
}

/// Wait for the save barrier, but let a second Ctrl-C abandon an unresponsive
/// wait. The first interrupt only explains the escape hatch; the save itself
/// keeps running until closing or process exit.
async fn wait_saved(session: &ManagedSession) -> (SaveWait, bool) {
    let mut prompted = false;
    loop {
        tokio::select! {
            biased;
            result = session.wait_saved() => return (SaveWait::Finished(result), false),
            signal = tokio::signal::ctrl_c() => {
                if signal.is_err() {
                    return (SaveWait::Finished(session.wait_saved().await), true);
                }
                if prompted {
                    return (SaveWait::Abandoned, false);
                }
                let _ = writeln!(
                    io::stderr().lock(),
                    "Tarz: saving; press Ctrl-C again to stop waiting and export recovery materials"
                );
                prompted = true;
            }
        }
    }
}

pub(super) fn recovery(
    session: &ManagedSession,
    directory: &Path,
) -> Result<RecoveryFile, &'static str> {
    let snapshot = session
        .read_materials()
        .map_err(|_| "returned materials are unavailable for recovery")?;
    let path = directory.join(format!("tarz-recovery-{}.json", new_block_id().0));
    let saved = tarz_core::material::save_new(&path, &snapshot.context)
        .map_err(|_| "recovery material export failed")?;
    Ok(RecoveryFile {
        path,
        importable: saved.importable,
    })
}

#[cfg(test)]
mod tests {
    use super::mcp_notices;
    use tarz_core::{McpServerStatus, McpSkippedTool};

    #[test]
    fn notices_name_unavailable_servers_and_skipped_tools() {
        let status = [
            McpServerStatus {
                id: "git".into(),
                ready: false,
                reason: Some("startup timed out after 1s".into()),
                skipped_tools: Vec::new(),
            },
            McpServerStatus {
                id: "docs".into(),
                ready: true,
                reason: None,
                skipped_tools: vec![McpSkippedTool {
                    name: "mcp_docs_bad.name".into(),
                    reason: "invalid name".into(),
                }],
            },
        ];
        assert_eq!(
            mcp_notices(&status),
            [
                "Tarz: MCP server 'git' is unavailable: startup timed out after 1s",
                "Tarz: MCP tool skipped: mcp_docs_bad.name (invalid name)",
            ]
        );
        assert!(mcp_notices(&[]).is_empty());
    }

    #[test]
    fn recovery_limit_warning_names_the_import_boundary() {
        assert_eq!(super::recovery_limit_warning(true), "");
        let warning = super::recovery_limit_warning(false);
        assert!(warning.contains("64 MiB"));
        assert!(warning.contains("cannot import"));
    }
}
