use super::output_format::OutputFormat;
use causa::kernel::{BlockId, InvocationId, ToolResultStatus};
use causa::runtime::new_block_id;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tars_core::McpServerStatus;
use tars_core::application::{
    ApplicationError, HostOptions, ManagedSession, SaveStatus, SessionHost, text_input,
};
use tars_core::media::{
    MAX_IMAGES_PER_MESSAGE, MediaError, PreparedImage, prepare_image_from_path,
};
use tars_core::permission::escape_untrusted_text;
use tars_core::session::{
    ProgressUpdate, SubmitRequest, ToolPhase, ToolProgress, WorkProgress, WorkRef, WorkResult,
    WorkState,
};

pub(super) struct WorkReport {
    pub work: WorkRef,
    pub result: Arc<WorkResult>,
    pub save: SaveWait,
    pub signal_failed: bool,
    pub usage: Option<tars_core::usage::WorkUsage>,
    pub streamed_text: bool,
    pub streamed_ends_with_newline: bool,
    pub stream_write_failed: bool,
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

pub(super) fn validate_prompt_options(
    input: Option<&str>,
    stdin: bool,
    has_images: bool,
) -> Result<(), &'static str> {
    if stdin && input.is_some() {
        return Err("choose exactly one of --input or --stdin");
    }
    if !has_images {
        if !stdin && input.is_none() {
            return Err("choose exactly one of --input or --stdin");
        }
        if input.is_some_and(|value| value.trim().is_empty()) {
            return Err("input must not be blank");
        }
    }
    Ok(())
}

pub(super) fn prepare_images(paths: &[PathBuf]) -> Result<Vec<PreparedImage>, String> {
    if paths.len() > MAX_IMAGES_PER_MESSAGE {
        return Err(MediaError::TooManyImages {
            count: paths.len(),
            max_images: MAX_IMAGES_PER_MESSAGE,
        }
        .to_string());
    }
    let mut images = Vec::with_capacity(paths.len());
    for path in paths {
        let image = prepare_image_from_path(path).map_err(|error| {
            format!(
                "invalid image '{}': {error}",
                escape_untrusted_text(&path.display().to_string())
            )
        })?;
        images.push(image);
    }
    Ok(images)
}

pub(super) fn prepare_prompt_input(
    input: Option<String>,
    stdin: bool,
    has_images: bool,
) -> Result<Option<String>, &'static str> {
    let text = match (input, stdin) {
        (Some(text), false) => Some(text),
        (None, true) => {
            let mut text = String::new();
            io::stdin()
                .read_to_string(&mut text)
                .map_err(|_| "cannot read UTF-8 stdin")?;
            Some(text)
        }
        (None, false) => None,
        (Some(_), true) => return Err("choose exactly one of --input or --stdin"),
    };
    match text {
        Some(text) if text.trim().is_empty() => {
            if has_images {
                Ok(None)
            } else {
                Err("input must not be blank")
            }
        }
        Some(text) => Ok(Some(text)),
        None => {
            if has_images {
                Ok(None)
            } else {
                Err("choose exactly one of --input or --stdin")
            }
        }
    }
}

pub(super) async fn work(
    session: &ManagedSession,
    input: Option<String>,
    images: &[PreparedImage],
    connect_approvals: bool,
    output_format: OutputFormat,
) -> Result<WorkReport, String> {
    let approver = if connect_approvals {
        Some(crate::cli::approval::spawn_approver(session))
    } else {
        None
    };
    let mut progress = session.subscribe_progress();
    let mut events = session.subscribe_events();
    let request_key = new_block_id().0.to_string();
    let receipt = if images.is_empty() {
        let text = input.ok_or_else(|| "input must not be blank".to_owned())?;
        session
            .submit(SubmitRequest {
                request_key,
                blocks: vec![text_input(text)],
            })
            .map_err(|error| error.to_string())?
    } else {
        session
            .submit_with_images(request_key, input.as_deref(), images)
            .map_err(|error| error.to_string())?
    };
    let mut signal_failed = false;
    let mut stopping = false;
    let mut progress_open = true;
    let mut events_open = true;
    let mut emitter = StreamEmitter::new(output_format, receipt.work.conversation_id.as_str());
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
                        emitter.on_progress_stdio(&event);
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        progress_open = false;
                    }
                }
                continue;
            }
            event = events.recv(), if events_open => {
                match event {
                    Ok(ev) => emitter.on_session_event_stdio(&ev),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        events_open = false;
                    }
                }
                continue;
            }
        };
        if let WorkState::Finished(result) = observation.state {
            emitter.reconcile_tool_provenance_stdio(&observation.tools.calls);
            break result;
        }
    };
    loop {
        match progress.try_recv() {
            Ok(event) if event.work == receipt.work => {
                emitter.on_progress_stdio(&event);
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
                        Ok(ev) => emitter.on_session_event_stdio(&ev),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            events_open = false;
                        }
                    }
                }
                event = progress.recv(), if progress_open => {
                    match event {
                        Ok(ev) => emitter.on_progress_stdio(&ev),
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
                        Ok(ev) => emitter.on_session_event_stdio(&ev),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            events_open = false;
                        }
                    }
                }
                event = progress.recv(), if progress_open => {
                    match event {
                        Ok(ev) => emitter.on_progress_stdio(&ev),
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
            Ok(ev) => emitter.on_session_event_stdio(&ev),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(
                tokio::sync::broadcast::error::TryRecvError::Empty
                | tokio::sync::broadcast::error::TryRecvError::Closed,
            ) => break,
        }
    }
    loop {
        match progress.try_recv() {
            Ok(ev) => emitter.on_progress_stdio(&ev),
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
        emitter.reconcile_tool_provenance_stdio(&obs.tools.calls);
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
        streamed_text: emitter.emitted_any_text,
        streamed_ends_with_newline: emitter.last_char_was_newline,
        stream_write_failed: emitter.stdout_failed,
    })
}

struct StreamEmitter {
    format: OutputFormat,
    root_agent_id: String,
    agent_names: HashMap<String, String>,
    emitted_any_text: bool,
    round_emitted_text: bool,
    round_completed: bool,
    need_round_separator: bool,
    last_char_was_newline: bool,
    stdout_broken_pipe: bool,
    stdout_failed: bool,
    reported_rule_calls: HashSet<(InvocationId, BlockId)>,
    reported_tool_phases: HashSet<(InvocationId, BlockId, &'static str, Option<&'static str>)>,
}

impl StreamEmitter {
    fn new(format: OutputFormat, root_agent_id: &str) -> Self {
        Self {
            format,
            root_agent_id: root_agent_id.to_owned(),
            agent_names: HashMap::new(),
            emitted_any_text: false,
            round_emitted_text: false,
            round_completed: false,
            need_round_separator: false,
            last_char_was_newline: false,
            stdout_broken_pipe: false,
            stdout_failed: false,
            reported_rule_calls: HashSet::new(),
            reported_tool_phases: HashSet::new(),
        }
    }

    fn on_progress_stdio(&mut self, event: &WorkProgress) {
        let mut out = io::stdout().lock();
        let mut err = io::stderr().lock();
        self.on_progress(event, &mut out, &mut err);
    }

    fn on_session_event_stdio(&mut self, event: &tars_core::events::SessionEvent) {
        let mut out = io::stdout().lock();
        let mut err = io::stderr().lock();
        self.on_session_event(event, &mut out, &mut err);
    }

    fn reconcile_tool_provenance_stdio(&mut self, calls: &[ToolProgress]) {
        let mut err = io::stderr().lock();
        for call in calls {
            report_tool_rule_provenance(call, &mut self.reported_rule_calls, &mut err);
        }
    }

    fn write_stdout(&mut self, out: &mut dyn Write, bytes: &[u8]) {
        if self.stdout_broken_pipe || self.stdout_failed {
            return;
        }
        match out.write_all(bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.stdout_broken_pipe = true;
            }
            Err(_) => {
                self.stdout_failed = true;
            }
        }
    }

    fn flush_stdout(&mut self, out: &mut dyn Write) {
        if self.stdout_broken_pipe || self.stdout_failed {
            return;
        }
        match out.flush() {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.stdout_broken_pipe = true;
            }
            Err(_) => {
                self.stdout_failed = true;
            }
        }
    }

    fn write_ndjson(&mut self, out: &mut dyn Write, value: &serde_json::Value) {
        if self.stdout_broken_pipe || self.stdout_failed {
            return;
        }
        if let Ok(line) = serde_json::to_string(value) {
            self.write_stdout(out, line.as_bytes());
            self.write_stdout(out, b"\n");
            self.flush_stdout(out);
        }
    }

    fn on_progress(&mut self, event: &WorkProgress, out: &mut dyn Write, err: &mut dyn Write) {
        match self.format {
            OutputFormat::Text | OutputFormat::Json => match &event.update {
                ProgressUpdate::Compaction(progress) => {
                    report_compaction_progress(progress, err, false);
                }
                ProgressUpdate::Tool(call) => {
                    report_tool_rule_provenance(call, &mut self.reported_rule_calls, err);
                }
                _ => {}
            },
            OutputFormat::Stream => match &event.update {
                ProgressUpdate::ModelStarted => {
                    if self.round_emitted_text && !self.round_completed {
                        let _ = writeln!(
                            err,
                            "TARS: model stream retrying; resetting current round preview"
                        );
                    }
                    if self.emitted_any_text {
                        self.need_round_separator = true;
                    }
                    self.round_emitted_text = false;
                    self.round_completed = false;
                }
                ProgressUpdate::Text { delta, .. } => {
                    if !delta.is_empty() {
                        if self.need_round_separator {
                            self.write_stdout(out, b"\n\n");
                            self.need_round_separator = false;
                        }
                        self.write_stdout(out, delta.as_bytes());
                        self.flush_stdout(out);
                        self.emitted_any_text = true;
                        self.round_emitted_text = true;
                        self.last_char_was_newline = delta.ends_with('\n');
                    }
                }
                ProgressUpdate::ModelOutputReady => {
                    self.round_completed = true;
                }
                ProgressUpdate::Tool(call) => {
                    report_tool_rule_provenance(call, &mut self.reported_rule_calls, err);
                    let phase = tool_phase_name(call.phase);
                    let status = call.status.as_ref().map(tool_status_name);
                    if self.reported_tool_phases.insert((
                        call.invocation_id.clone(),
                        call.call_block_id,
                        phase,
                        status,
                    )) {
                        let label = format_tool_status_label(call.phase, status);
                        let _ = writeln!(
                            err,
                            "TARS: tool {}: {label}",
                            escape_untrusted_text(&call.tool_name),
                        );
                    }
                }
                ProgressUpdate::Compaction(progress) => {
                    report_compaction_progress(progress, err, true);
                }
                ProgressUpdate::Reasoning { .. } | ProgressUpdate::ToolCallDelta { .. } => {}
            },
            OutputFormat::StreamJson => match &event.update {
                ProgressUpdate::ModelStarted => {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "model_started",
                            "conversation_id": event.work.conversation_id.as_str(),
                            "work_id": event.work.turn_id.0,
                            "invocation_id": {
                                "turn_id": event.invocation_id.turn_id.0,
                                "round_id": event.invocation_id.round_id.0,
                            },
                        }),
                    );
                }
                ProgressUpdate::Text { delta, truncated } => {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "text_delta",
                            "delta": delta,
                            "truncated": truncated,
                        }),
                    );
                }
                ProgressUpdate::Reasoning { delta, truncated } => {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "reasoning_delta",
                            "delta": delta,
                            "truncated": truncated,
                        }),
                    );
                }
                ProgressUpdate::ToolCallDelta {
                    call_index,
                    name_delta,
                    arguments_delta,
                    truncated,
                } => {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "tool_call_delta",
                            "call_index": call_index,
                            "name_delta": name_delta,
                            "arguments_delta": arguments_delta,
                            "truncated": truncated,
                        }),
                    );
                }
                ProgressUpdate::ModelOutputReady => {}
                ProgressUpdate::Tool(call) => {
                    report_tool_rule_provenance(call, &mut self.reported_rule_calls, err);
                    let phase = tool_phase_name(call.phase);
                    let status = call.status.as_ref().map(tool_status_name);
                    if self.reported_tool_phases.insert((
                        call.invocation_id.clone(),
                        call.call_block_id,
                        phase,
                        status,
                    )) {
                        self.write_ndjson(
                            out,
                            &serde_json::json!({
                                "type": "tool_progress",
                                "tool_name": call.tool_name,
                                "phase": phase,
                                "status": status,
                                "arguments_summary": call.arguments_summary,
                            }),
                        );
                    }
                }
                ProgressUpdate::Compaction(progress) => {
                    report_compaction_progress(progress, err, false);
                    self.write_ndjson(out, &compaction_progress_json(progress));
                }
            },
        }
    }

    fn on_session_event(
        &mut self,
        event: &tars_core::events::SessionEvent,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) {
        use tars_core::events::{OperationStatus, SessionEvent};
        match event {
            SessionEvent::AgentRegistered { agent_id, name, .. } => {
                self.agent_names.insert(agent_id.clone(), name.clone());
            }
            SessionEvent::AutoWakeupLimitReached {
                agent_id,
                max_auto_wakeups,
                ..
            } => {
                let label = self
                    .agent_names
                    .get(agent_id)
                    .map(String::as_str)
                    .unwrap_or(agent_id.as_str());
                let _ = writeln!(
                    err,
                    "TARS: auto-wakeup limit ({max_auto_wakeups}) reached for agent {}",
                    escape_untrusted_text(label),
                );
                if self.format == OutputFormat::StreamJson {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "agent_event",
                            "event": "auto_wakeup_limit_reached",
                            "agent_id": agent_id,
                            "agent_name": label,
                            "max_auto_wakeups": max_auto_wakeups,
                        }),
                    );
                }
            }
            SessionEvent::OperationStateChanged {
                operation_id,
                target_agent_id,
                parent_operation_id,
                status,
                ..
            } if target_agent_id != &self.root_agent_id
                && !matches!(status, OperationStatus::Creating) =>
            {
                let label = self
                    .agent_names
                    .get(target_agent_id)
                    .map(String::as_str)
                    .unwrap_or(target_agent_id.as_str());
                let _ = writeln!(
                    err,
                    "TARS: [agent: {}] {}",
                    escape_untrusted_text(label),
                    status.as_str(),
                );
                if self.format == OutputFormat::StreamJson {
                    self.write_ndjson(
                        out,
                        &serde_json::json!({
                            "type": "agent_event",
                            "event": "operation_state_changed",
                            "agent_id": target_agent_id,
                            "agent_name": label,
                            "operation_id": operation_id,
                            "parent_operation_id": parent_operation_id,
                            "status": status.as_str(),
                        }),
                    );
                }
            }
            _ => {}
        }
    }
}

fn tool_phase_name(phase: ToolPhase) -> &'static str {
    match phase {
        ToolPhase::Waiting => "waiting",
        ToolPhase::Running => "running",
        ToolPhase::Returned => "returned",
        ToolPhase::Committed => "committed",
        ToolPhase::NotStarted => "not_started",
        ToolPhase::Unknown => "unknown",
    }
}

fn tool_status_name(status: &ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Succeeded => "succeeded",
        ToolResultStatus::Failed => "failed",
        ToolResultStatus::Rejected => "rejected",
        ToolResultStatus::Cancelled => "cancelled",
        ToolResultStatus::TimedOut => "timed_out",
        ToolResultStatus::UnknownOutcome => "unknown_outcome",
    }
}

fn format_tool_status_label(phase: ToolPhase, status: Option<&'static str>) -> String {
    match phase {
        ToolPhase::Waiting => "waiting".to_owned(),
        ToolPhase::Running => "running".to_owned(),
        ToolPhase::Returned => match status {
            Some(s) if s != "succeeded" => format!("returned ({s})"),
            _ => "returned".to_owned(),
        },
        ToolPhase::Committed => match status {
            Some("succeeded") | None => "committed".to_owned(),
            Some(s) => format!("committed ({s})"),
        },
        ToolPhase::NotStarted => "not_started".to_owned(),
        ToolPhase::Unknown => "unknown".to_owned(),
    }
}

fn compaction_progress_json(
    progress: &tars_core::compact::CompactionProgress,
) -> serde_json::Value {
    use tars_core::compact::CompactionProgress;
    match progress {
        CompactionProgress::Started {
            trigger,
            estimate_before,
        } => serde_json::json!({
            "type": "compaction",
            "stage": "started",
            "trigger": trigger.name(),
            "estimate_before": estimate_before,
        }),
        CompactionProgress::Finished {
            trigger,
            estimate_before,
            estimate_after,
            record,
        } => serde_json::json!({
            "type": "compaction",
            "stage": "finished",
            "trigger": trigger.name(),
            "estimate_before": estimate_before,
            "estimate_after": estimate_after,
            "record": record,
        }),
        CompactionProgress::MicroCompacted {
            trigger,
            estimate_before,
            estimate_after,
            cleared_results,
            record,
        } => serde_json::json!({
            "type": "compaction",
            "stage": "micro_compacted",
            "trigger": trigger.name(),
            "estimate_before": estimate_before,
            "estimate_after": estimate_after,
            "cleared_results": cleared_results,
            "record": record,
        }),
        CompactionProgress::Abandoned { trigger, reason } => serde_json::json!({
            "type": "compaction",
            "stage": "abandoned",
            "trigger": trigger.name(),
            "reason": reason.name(),
        }),
        CompactionProgress::Trimmed {
            before_tokens,
            after_tokens,
        } => serde_json::json!({
            "type": "compaction",
            "stage": "trimmed",
            "before_tokens": before_tokens,
            "after_tokens": after_tokens,
        }),
    }
}

fn report_tool_rule_provenance(
    call: &ToolProgress,
    reported_rule_calls: &mut HashSet<(InvocationId, BlockId)>,
    err: &mut dyn Write,
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
            err,
            "TARS: {}: {}",
            escape_untrusted_text(&call.tool_name),
            summary
        );
    }
}

fn report_compaction_progress(
    progress: &tars_core::compact::CompactionProgress,
    err: &mut dyn Write,
    verbose: bool,
) {
    use tars_core::compact::{CompactionAbandon, CompactionProgress};
    match progress {
        CompactionProgress::Trimmed {
            before_tokens,
            after_tokens,
        } => {
            let _ = writeln!(
                err,
                "TARS: request frame temporarily trimmed (~{before_tokens} -> ~{after_tokens} tokens)"
            );
        }
        CompactionProgress::Abandoned {
            reason: CompactionAbandon::PrefixTooLarge,
            ..
        } => {
            let _ = writeln!(err, "TARS: fixed prefix too large; compaction skipped");
        }
        CompactionProgress::Started {
            trigger,
            estimate_before,
        } if verbose => {
            let _ = writeln!(
                err,
                "TARS: compacting ({}) ~{estimate_before} tokens",
                trigger.name()
            );
        }
        CompactionProgress::Finished {
            trigger,
            estimate_before,
            estimate_after,
            ..
        } if verbose => {
            let _ = writeln!(
                err,
                "TARS: compacted ({}) ~{estimate_before} -> ~{estimate_after} tokens",
                trigger.name()
            );
        }
        CompactionProgress::MicroCompacted {
            trigger,
            estimate_before,
            estimate_after,
            cleared_results,
            ..
        } if verbose => {
            let _ = writeln!(
                err,
                "TARS: micro-compacted ({}) {cleared_results} tool result(s), ~{estimate_before} -> ~{estimate_after} tokens",
                trigger.name()
            );
        }
        CompactionProgress::Abandoned { trigger, reason } if verbose => {
            let _ = writeln!(
                err,
                "TARS: compaction abandoned ({}): {}",
                trigger.name(),
                reason.name()
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

    fn work_progress(round: u32, update: super::ProgressUpdate) -> super::WorkProgress {
        let turn_id = causa::kernel::TurnId::new("work-1");
        super::WorkProgress {
            work: super::WorkRef {
                conversation_id: "conv-1".into(),
                turn_id: turn_id.clone(),
            },
            invocation_id: causa::kernel::InvocationId {
                turn_id,
                round_id: causa::kernel::RoundId(round),
            },
            update,
        }
    }

    #[test]
    fn stream_emitter_inserts_paragraph_break_between_rounds_and_reports_retry() {
        use super::{OutputFormat, ProgressUpdate, StreamEmitter, ToolPhase, ToolProgress};
        use causa::kernel::ToolResultStatus;

        let mut emitter = StreamEmitter::new(OutputFormat::Stream, "conv-1");
        let mut out = Vec::new();
        let mut err = Vec::new();

        // Round 0: emits text, completes, then tool runs.
        emitter.on_progress(
            &work_progress(0, ProgressUpdate::ModelStarted),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Text {
                    delta: "Checking".into(),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(0, ProgressUpdate::ModelOutputReady),
            &mut out,
            &mut err,
        );

        let call_id = super::new_block_id();
        let tool_running = ToolProgress {
            invocation_id: causa::kernel::InvocationId {
                turn_id: causa::kernel::TurnId::new("work-1"),
                round_id: causa::kernel::RoundId(0),
            },
            call_block_id: call_id,
            tool_name: "read\x1b[31m_file".into(),
            arguments_summary: "{}".into(),
            phase: ToolPhase::Running,
            status: None,
            operation_not_started: false,
            permission_decision: None,
        };
        emitter.on_progress(
            &work_progress(0, ProgressUpdate::Tool(tool_running.clone())),
            &mut out,
            &mut err,
        );
        // Duplicate phase update should be deduplicated.
        emitter.on_progress(
            &work_progress(0, ProgressUpdate::Tool(tool_running)),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Tool(ToolProgress {
                    invocation_id: causa::kernel::InvocationId {
                        turn_id: causa::kernel::TurnId::new("work-1"),
                        round_id: causa::kernel::RoundId(0),
                    },
                    call_block_id: call_id,
                    tool_name: "read\x1b[31m_file".into(),
                    arguments_summary: "{}".into(),
                    phase: ToolPhase::Committed,
                    status: Some(ToolResultStatus::Failed),
                    operation_not_started: false,
                    permission_decision: None,
                }),
            ),
            &mut out,
            &mut err,
        );

        // Round 1: emits partial text then retries before ModelOutputReady!
        emitter.on_progress(
            &work_progress(1, ProgressUpdate::ModelStarted),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                1,
                ProgressUpdate::Text {
                    delta: "Partial".into(),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(1, ProgressUpdate::ModelStarted),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                1,
                ProgressUpdate::Text {
                    delta: "Final".into(),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(1, ProgressUpdate::ModelOutputReady),
            &mut out,
            &mut err,
        );

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert_eq!(stdout, "Checking\n\nPartial\n\nFinal");
        assert!(emitter.emitted_any_text);
        assert!(!emitter.last_char_was_newline);
        assert!(!stderr.contains('\x1b'));
        assert_eq!(
            stderr
                .matches("TARS: tool read\\x1b[31m_file: running")
                .count(),
            1
        );
        assert!(stderr.contains("TARS: tool read\\x1b[31m_file: committed (failed)"));
        assert!(stderr.contains("TARS: model stream retrying; resetting current round preview"));
    }

    #[test]
    fn stream_json_emitter_writes_valid_ndjson_lines() {
        use super::{OutputFormat, ProgressUpdate, StreamEmitter, ToolPhase, ToolProgress};
        use causa::kernel::ToolResultStatus;
        use tars_core::compact::{CompactionProgress, HistoryTrigger};
        use tars_core::events::{AgentInteraction, OperationStatus, SessionEvent};

        let mut emitter = StreamEmitter::new(OutputFormat::StreamJson, "conv-1");
        let mut out = Vec::new();
        let mut err = Vec::new();

        emitter.on_progress(
            &work_progress(0, ProgressUpdate::ModelStarted),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Text {
                    delta: "hi".into(),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Reasoning {
                    delta: "think".into(),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::ToolCallDelta {
                    call_index: 0,
                    name_delta: Some("read_file".into()),
                    arguments_delta: Some("{\"path\":\".\"}".into()),
                    truncated: false,
                },
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Tool(ToolProgress {
                    invocation_id: causa::kernel::InvocationId {
                        turn_id: causa::kernel::TurnId::new("work-1"),
                        round_id: causa::kernel::RoundId(0),
                    },
                    call_block_id: super::new_block_id(),
                    tool_name: "read_file".into(),
                    arguments_summary: "{\"path\":\".\"}".into(),
                    phase: ToolPhase::Committed,
                    status: Some(ToolResultStatus::Succeeded),
                    operation_not_started: false,
                    permission_decision: None,
                }),
            ),
            &mut out,
            &mut err,
        );
        emitter.on_progress(
            &work_progress(
                0,
                ProgressUpdate::Compaction(CompactionProgress::Started {
                    trigger: HistoryTrigger::Auto,
                    estimate_before: 1200,
                }),
            ),
            &mut out,
            &mut err,
        );
        emitter.on_session_event(
            &SessionEvent::AgentRegistered {
                session_id: "conv-1".into(),
                agent_id: "agent-2".into(),
                name: "helper".into(),
                preset_name: None,
                interaction: AgentInteraction::Reusable,
            },
            &mut out,
            &mut err,
        );
        emitter.on_session_event(
            &SessionEvent::OperationStateChanged {
                session_id: "conv-1".into(),
                operation_id: "op-1".into(),
                target_agent_id: "agent-2".into(),
                parent_operation_id: None,
                status: OperationStatus::Running,
            },
            &mut out,
            &mut err,
        );

        let lines: Vec<serde_json::Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0]["type"], "model_started");
        assert_eq!(lines[1]["type"], "text_delta");
        assert_eq!(lines[2]["type"], "reasoning_delta");
        assert_eq!(lines[3]["type"], "tool_call_delta");
        assert_eq!(lines[4]["type"], "tool_progress");
        assert_eq!(lines[4]["phase"], "committed");
        assert_eq!(lines[4]["status"], "succeeded");
        assert_eq!(lines[5]["type"], "compaction");
        assert_eq!(lines[5]["stage"], "started");
        assert_eq!(lines[6]["type"], "agent_event");
        assert_eq!(lines[6]["agent_name"], "helper");
    }
}
