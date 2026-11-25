use causa::runtime::new_block_id;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tars_core::McpServerStatus;
use tars_core::application::{
    ApplicationError, HostOptions, ManagedSession, SaveStatus, SessionHost, text_input,
};
use tars_core::session::{SubmitRequest, WorkRef, WorkResult, WorkState};

pub(super) struct WorkReport {
    pub work: WorkRef,
    pub result: Arc<WorkResult>,
    pub save: SaveWait,
    pub signal_failed: bool,
    pub usage: Option<tars_core::usage::WorkUsage>,
}

/// The independent save barrier either settled, was abandoned on a second Ctrl-C,
/// or timed out waiting for cancelled execution to settle.
pub(super) enum SaveWait {
    Finished(Result<SaveStatus, ApplicationError>),
    Abandoned,
    CancelWaitTimedOut,
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
            "TARS: reopened permission mode was downgraded from full-access to ask; the saved definition was not created with full-access"
        );
    }
}

/// Warn on `stderr` when untrusted workspace `allow` rules were ignored on open.
pub(super) fn report_workspace_rules_notice(session: &ManagedSession) {
    let report = session.open_report();
    let count = report.workspace_rules.allow_rules.len();
    if count == 0 {
        return;
    }
    if report.workspace_rules.can_trust {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: workspace rules are untrusted; ignored {count} allow rule(s) (use --trust-workspace-rules to trust)"
        );
    } else {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: workspace rules are untrusted; ignored {count} allow rule(s) (transient tars run cannot trust workspace allow rules; use a persistent session with --trust-workspace-rules)"
        );
    }
}

/// Warn on `stderr` when untrusted workspace `.tars/agents/*.md` presets were ignored on open.
pub(super) fn report_workspace_agents_notice(session: &ManagedSession) {
    let report = session.open_report();
    if report.workspace_agents.trusted || report.workspace_agents.untrusted_agents.is_empty() {
        return;
    }
    let count = report.workspace_agents.untrusted_agents.len();
    if report.workspace_agents.can_trust {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: workspace agents are untrusted; ignored {count} preset(s) (use --trust-workspace-agents to trust)"
        );
    } else {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: workspace agents are untrusted; ignored {count} preset(s) (transient tars run cannot trust workspace agents; use a persistent session with --trust-workspace-agents)"
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
                "TARS: MCP server '{}' is unavailable: {reason}",
                server.id
            ));
        }
        for tool in &server.skipped_tools {
            lines.push(format!(
                "TARS: MCP tool skipped: {} ({})",
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

/// Writes non-fatal prelude and skill discovery warnings to `stderr` only,
/// keeping `--json` output on `stdout` valid.
pub(super) fn report_prelude_diagnostics(diagnostics: &[tars_core::prelude::PreludeDiagnostic]) {
    if diagnostics.is_empty() {
        return;
    }
    let mut stderr = io::stderr().lock();
    for diag in diagnostics {
        let _ = writeln!(
            stderr,
            "TARS: prelude warning ({}): {}",
            diag.path.display(),
            diag.message
        );
    }
}

/// Report the probed command sandbox in full when a session is created.
pub(super) fn report_command_details(session: &ManagedSession) {
    if let Some(status) = session.command_status() {
        let _ = writeln!(io::stderr().lock(), "TARS: command sandbox: {status}");
    }
}

/// Report the probed command sandbox in its short form when a session is
/// opened for work. Sessions without the command tool print nothing.
pub(super) fn report_command_brief(session: &ManagedSession) {
    if let Some(status) = session.command_status() {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: command sandbox: {}",
            status.brief()
        );
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
    let mut progress = session.subscribe_progress();
    let mut events = session.subscribe_events();
    let receipt = session
        .submit(SubmitRequest {
            request_key: new_block_id().0.to_string(),
            blocks: vec![text_input(input)],
        })
        .map_err(|error| error.to_string())?;
    let mut signal_failed = false;
    let mut stopping = false;
    let mut progress_open = true;
    let mut reported_rule_calls = std::collections::HashSet::new();
    let initial_result = loop {
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
            event = progress.recv(), if progress_open => {
                match event {
                    Ok(event) if event.work == receipt.work => {
                        report_progress_update(&event.update, &mut reported_rule_calls);
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        progress_open = false;
                    }
                }
                continue;
            }
        };
        if let WorkState::Finished(result) = observation.state {
            for call in &observation.tools.calls {
                report_tool_rule_provenance(call, &mut reported_rule_calls);
            }
            break result;
        }
    };
    loop {
        match progress.try_recv() {
            Ok(event) if event.work == receipt.work => {
                report_progress_update(&event.update, &mut reported_rule_calls);
            }
            Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }

    // Converge background child tasks and any reactive root wakeups before
    // closing the interactive approver so sub-agent and wakeup approvals still work.
    let mut final_work = receipt.work.clone();
    let mut final_result = initial_result;
    let mut settle_error: Option<ApplicationError> = None;
    let mut settle_abandoned = false;
    let mut cancel_timed_out = false;
    let mut cancelled_during_settle = false;
    let mut save_prompted = false;
    let mut settled_work: Option<WorkRef> = None;
    let mut events_open = true;
    let mut agent_names = std::collections::HashMap::new();
    let root_agent_id = receipt.work.conversation_id.as_str().to_owned();

    if stopping {
        if session.has_active_execution() {
            cancelled_during_settle = true;
        }
        session.cancel_all_active();
        let settle = session.wait_settled_after(&receipt.work);
        tokio::pin!(settle);
        let timeout = tokio::time::sleep(Duration::from_secs(5));
        tokio::pin!(timeout);
        loop {
            tokio::select! {
                biased;
                res = &mut settle => {
                    match res {
                        Ok(w) => settled_work = w,
                        Err(err) => settle_error = Some(err),
                    }
                    break;
                }
                signal = tokio::signal::ctrl_c() => {
                    if signal.is_err() {
                        signal_failed = true;
                    }
                    if !session.has_active_execution() {
                        if save_prompted {
                            settle_abandoned = true;
                            break;
                        }
                        let _ = writeln!(
                            io::stderr().lock(),
                            "TARS: saving; press Ctrl-C again to stop waiting and export recovery materials"
                        );
                        save_prompted = true;
                    } else {
                        settle_abandoned = true;
                        break;
                    }
                }
                _ = &mut timeout => {
                    if session.has_active_execution() {
                        cancel_timed_out = true;
                    }
                    break;
                }
                event = events.recv(), if events_open => {
                    match event {
                        Ok(ev) => report_session_event(&ev, &root_agent_id, &mut agent_names),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            events_open = false;
                        }
                    }
                }
                event = progress.recv(), if progress_open => {
                    match event {
                        Ok(ev) => report_progress_update(&ev.update, &mut reported_rule_calls),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            progress_open = false;
                        }
                    }
                }
            }
        }
    } else {
        let settle = session.wait_settled_after(&receipt.work);
        tokio::pin!(settle);
        let mut cancel_timeout: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        loop {
            tokio::select! {
                biased;
                res = &mut settle => {
                    match res {
                        Ok(w) => settled_work = w,
                        Err(err) => settle_error = Some(err),
                    }
                    break;
                }
                signal = tokio::signal::ctrl_c() => {
                    if signal.is_err() {
                        signal_failed = true;
                    }
                    if !session.has_active_execution() && !cancelled_during_settle {
                        // Execution is already finished and only persistence is waiting:
                        // preserve the two-step Ctrl-C escape hatch for blocked saves.
                        if save_prompted {
                            settle_abandoned = true;
                            break;
                        }
                        let _ = writeln!(
                            io::stderr().lock(),
                            "TARS: saving; press Ctrl-C again to stop waiting and export recovery materials"
                        );
                        save_prompted = true;
                    } else if !cancelled_during_settle {
                        cancelled_during_settle = true;
                        if let Some(approver) = &approver {
                            approver.on_cancel();
                        }
                        session.cancel_all_active();
                        cancel_timeout = Some(Box::pin(tokio::time::sleep(Duration::from_secs(5))));
                    } else {
                        settle_abandoned = true;
                        break;
                    }
                }
                _ = async {
                    if let Some(timer) = cancel_timeout.as_mut() {
                        timer.as_mut().await;
                    }
                }, if cancel_timeout.is_some() => {
                    cancel_timed_out = true;
                    break;
                }
                event = events.recv(), if events_open => {
                    match event {
                        Ok(ev) => report_session_event(&ev, &root_agent_id, &mut agent_names),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            events_open = false;
                        }
                    }
                }
                event = progress.recv(), if progress_open => {
                    match event {
                        Ok(ev) => report_progress_update(&ev.update, &mut reported_rule_calls),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            progress_open = false;
                        }
                    }
                }
            }
        }
    }

    loop {
        match events.try_recv() {
            Ok(ev) => report_session_event(&ev, &root_agent_id, &mut agent_names),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }
    loop {
        match progress.try_recv() {
            Ok(ev) => report_progress_update(&ev.update, &mut reported_rule_calls),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }

    if let Some(approver) = approver {
        approver.finish();
    }

    if let Some(wakeup_work) = settled_work
        && wakeup_work != receipt.work
        && let Ok(obs) = session.observe(&wakeup_work)
    {
        for call in &obs.tools.calls {
            report_tool_rule_provenance(call, &mut reported_rule_calls);
        }
        if let WorkState::Finished(res) = obs.state {
            final_work = wakeup_work;
            final_result = res;
        }
    }
    if cancelled_during_settle
        && tars_core::application::WorkOutcome::of(&final_result).kind != "cancelled"
    {
        final_result = Arc::new(WorkResult::StoppedBeforeRun(
            causa::kernel::ControlError::Cancelled,
        ));
    }

    // A raw Finished observation precedes persistence. Keep the runtime alive
    // through the independent host save barrier, including cancelled work.
    let (save, wait_signal_failed) = if settle_abandoned {
        (SaveWait::Abandoned, false)
    } else if cancel_timed_out {
        (SaveWait::CancelWaitTimedOut, false)
    } else {
        let (mut save, wait_sig) = wait_saved(session, save_prompted).await;
        if let Some(err) = settle_error
            && matches!(save, SaveWait::Finished(Ok(_)))
        {
            save = SaveWait::Finished(Err(err));
        }
        (save, wait_sig)
    };
    let usage = session.work_usage(&final_work);
    Ok(WorkReport {
        work: final_work,
        result: final_result,
        save,
        signal_failed: signal_failed || wait_signal_failed,
        usage,
    })
}

fn report_session_event(
    event: &tars_core::events::SessionEvent,
    root_agent_id: &str,
    agent_names: &mut std::collections::HashMap<String, String>,
) {
    use tars_core::events::{OperationStatus, SessionEvent};
    match event {
        SessionEvent::AgentRegistered { agent_id, name, .. } => {
            agent_names.insert(agent_id.clone(), name.clone());
        }
        SessionEvent::AutoWakeupLimitReached {
            agent_id,
            max_auto_wakeups,
            ..
        } => {
            let label = agent_names
                .get(agent_id)
                .map(String::as_str)
                .unwrap_or(agent_id.as_str());
            let _ = writeln!(
                io::stderr().lock(),
                "TARS: auto-wakeup limit ({max_auto_wakeups}) reached for agent {}",
                tars_core::permission::escape_untrusted_text(label),
            );
        }
        SessionEvent::OperationStateChanged {
            target_agent_id,
            status,
            ..
        } if target_agent_id != root_agent_id && !matches!(status, OperationStatus::Creating) => {
            let label = agent_names
                .get(target_agent_id)
                .map(String::as_str)
                .unwrap_or(target_agent_id.as_str());
            let _ = writeln!(
                io::stderr().lock(),
                "TARS: [agent: {}] {}",
                tars_core::permission::escape_untrusted_text(label),
                status.as_str(),
            );
        }
        _ => {}
    }
}

fn report_progress_update(
    update: &tars_core::session::ProgressUpdate,
    reported_rule_calls: &mut std::collections::HashSet<(
        causa::kernel::InvocationId,
        causa::kernel::BlockId,
    )>,
) {
    match update {
        tars_core::session::ProgressUpdate::Compaction(progress) => {
            report_compaction_progress(progress);
        }
        tars_core::session::ProgressUpdate::Tool(call) => {
            report_tool_rule_provenance(call, reported_rule_calls);
        }
        _ => {}
    }
}

fn report_tool_rule_provenance(
    call: &tars_core::session::ToolProgress,
    reported_rule_calls: &mut std::collections::HashSet<(
        causa::kernel::InvocationId,
        causa::kernel::BlockId,
    )>,
) {
    let Some(summary) = call
        .permission_decision
        .as_ref()
        .and_then(|decision| decision.rule_summary())
    else {
        return;
    };
    if reported_rule_calls.insert((call.invocation_id.clone(), call.call_block_id)) {
        let _ = writeln!(
            io::stderr().lock(),
            "TARS: {}: {}",
            tars_core::permission::escape_untrusted_text(&call.tool_name),
            summary
        );
    }
}

fn report_compaction_progress(progress: &tars_core::compact::CompactionProgress) {
    use tars_core::compact::{CompactionAbandon, CompactionProgress};
    match progress {
        CompactionProgress::Trimmed {
            before_tokens,
            after_tokens,
        } => {
            let _ = writeln!(
                io::stderr().lock(),
                "TARS: request frame temporarily trimmed (~{before_tokens} -> ~{after_tokens} tokens)"
            );
        }
        CompactionProgress::Abandoned {
            reason: CompactionAbandon::PrefixTooLarge,
            ..
        } => {
            let _ = writeln!(
                io::stderr().lock(),
                "TARS: fixed prefix too large; compaction skipped"
            );
        }
        CompactionProgress::Started { .. }
        | CompactionProgress::Finished { .. }
        | CompactionProgress::MicroCompacted { .. }
        | CompactionProgress::Abandoned { .. } => {}
    }
}

/// Wait for the save barrier, but let a second Ctrl-C abandon an unresponsive
/// wait. The first interrupt only explains the escape hatch; the save itself
/// keeps running until closing or process exit.
async fn wait_saved(session: &ManagedSession, mut prompted: bool) -> (SaveWait, bool) {
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
                    "TARS: saving; press Ctrl-C again to stop waiting and export recovery materials"
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
    let path = directory.join(format!("tars-recovery-{}.json", new_block_id().0));
    let context = snapshot.detached_context();
    let saved = tars_core::material::save_new(&path, &context)
        .map_err(|_| "recovery material export failed")?;
    Ok(RecoveryFile {
        path,
        importable: saved.importable,
    })
}

#[cfg(test)]
mod tests {
    use super::mcp_notices;
    use tars_core::{McpServerStatus, McpSkippedTool};

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
                "TARS: MCP server 'git' is unavailable: startup timed out after 1s",
                "TARS: MCP tool skipped: mcp_docs_bad.name (invalid name)",
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
