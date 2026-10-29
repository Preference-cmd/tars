//! Terminal approval interaction and request handling.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tarz_core::permission::{
    ApprovalDecision, ApprovalSubject, PermissionMode, WriteAction, escape_untrusted_text,
};

pub(crate) fn should_connect_approvals(
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    used_stdin_input: bool,
) -> bool {
    stdin_is_terminal && stderr_is_terminal && !used_stdin_input
}

pub(crate) fn should_connect_stdio(used_stdin_input: bool) -> bool {
    use std::io::IsTerminal;
    should_connect_approvals(
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        used_stdin_input,
    )
}

pub(crate) fn ask_approval<R: BufRead, W: Write>(
    tool_name: &str,
    mode: PermissionMode,
    subject: &ApprovalSubject,
    reader: &mut R,
    writer: &mut W,
    prompt_open: &AtomicBool,
    cancelled: &AtomicBool,
) -> std::io::Result<ApprovalDecision> {
    if cancelled.load(Ordering::SeqCst) {
        return Ok(ApprovalDecision::Deny { reason: None });
    }

    match subject {
        ApprovalSubject::FileWrite { path, action, diff } => {
            let action_str = match action {
                WriteAction::Created => "create",
                WriteAction::Updated => "update",
            };
            writeln!(
                writer,
                "Tarz: {} wants to {} {} (permission mode: {})",
                escape_untrusted_text(tool_name),
                action_str,
                escape_untrusted_text(path),
                mode,
            )?;
            let escaped_diff = escape_untrusted_text(diff);
            write!(writer, "{escaped_diff}")?;
            if !escaped_diff.ends_with('\n') {
                writeln!(writer)?;
            }
            writer.flush()?;
        }
        ApprovalSubject::Command {
            command,
            workdir,
            timeout_secs,
            network,
            outside_sandbox,
            justification,
        } => {
            if *outside_sandbox {
                writeln!(
                    writer,
                    "Tarz: {} wants to run a command outside the sandbox (permission mode: {})",
                    escape_untrusted_text(tool_name),
                    mode,
                )?;
                writeln!(
                    writer,
                    "NOT in the sandbox: full disk read/write, network and credentials are available"
                )?;
                writeln!(
                    writer,
                    "Justification: {}",
                    escape_untrusted_text(justification.as_deref().unwrap_or("(none)"))
                )?;
                writeln!(
                    writer,
                    "Working directory: {}",
                    escape_untrusted_text(workdir)
                )?;
                writeln!(writer, "Timeout: {timeout_secs}s")?;
            } else {
                writeln!(
                    writer,
                    "Tarz: {} wants to run a command (permission mode: {})",
                    escape_untrusted_text(tool_name),
                    mode,
                )?;
                writeln!(
                    writer,
                    "Working directory: {}",
                    escape_untrusted_text(workdir)
                )?;
                writeln!(
                    writer,
                    "Timeout: {timeout_secs}s · Network: {network} · workspace writable, .git hooks and config read-only, credentials unreadable"
                )?;
            }
            writeln!(writer, "$ {}", escape_untrusted_text(command))?;
            writer.flush()?;
        }
        ApprovalSubject::McpCall {
            server,
            tool,
            arguments,
        } => {
            writeln!(
                writer,
                "Tarz: {} wants to call MCP tool {}/{} (permission mode: {})",
                escape_untrusted_text(tool_name),
                escape_untrusted_text(server),
                escape_untrusted_text(tool),
                mode,
            )?;
            let escaped_args = escape_untrusted_text(arguments);
            writeln!(writer, "{escaped_args}")?;
            writer.flush()?;
        }
    }

    let prompt_text = match subject {
        ApprovalSubject::FileWrite { .. } => "Allow this write? [y/n] ",
        ApprovalSubject::Command {
            outside_sandbox: true,
            ..
        } => "Allow this command outside the sandbox? [y/n] ",
        ApprovalSubject::Command { .. } => "Allow this command? [y/n] ",
        ApprovalSubject::McpCall { .. } => "Allow this tool call? [y/n] ",
    };

    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(ApprovalDecision::Deny { reason: None });
        }

        write!(writer, "{prompt_text}")?;
        writer.flush()?;
        prompt_open.store(true, Ordering::SeqCst);

        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        prompt_open.store(false, Ordering::SeqCst);

        if cancelled.load(Ordering::SeqCst) {
            return Ok(ApprovalDecision::Deny { reason: None });
        }

        if bytes == 0 {
            let _ = writeln!(writer);
            return Ok(ApprovalDecision::Deny { reason: None });
        }

        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(ApprovalDecision::Allow);
        }
        if trimmed.eq_ignore_ascii_case("n") || trimmed.eq_ignore_ascii_case("no") {
            write!(writer, "Reason for the model (optional, Enter to skip): ")?;
            writer.flush()?;
            prompt_open.store(true, Ordering::SeqCst);

            line.clear();
            let reason_bytes = reader.read_line(&mut line)?;
            prompt_open.store(false, Ordering::SeqCst);

            if cancelled.load(Ordering::SeqCst) {
                return Ok(ApprovalDecision::Deny { reason: None });
            }

            let reason = if reason_bytes == 0 || line.trim().is_empty() {
                None
            } else {
                Some(line.trim().to_owned())
            };
            return Ok(ApprovalDecision::Deny { reason });
        }
    }
}

/// Handle to the background thread that reads approval answers.
pub(crate) struct ApproverHandle {
    prompt_open: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl ApproverHandle {
    /// Signal cancellation and leave a clean line if a prompt is open.
    pub(crate) fn on_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if self.prompt_open.swap(false, Ordering::SeqCst) {
            let _ = writeln!(std::io::stderr().lock());
        }
    }

    pub(crate) fn finish(self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

pub(crate) fn spawn_approver(session: &tarz_core::application::ManagedSession) -> ApproverHandle {
    let mut rx = session.connect_approvals();
    let prompt_open = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let prompt_open_thread = prompt_open.clone();
    let cancelled_thread = cancelled.clone();

    std::thread::Builder::new()
        .name("tarz-approval".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            let mut writer = std::io::stderr();
            while let Some(request) = rx.blocking_recv() {
                if cancelled_thread.load(Ordering::SeqCst) {
                    let _ = request.reply.send(ApprovalDecision::Deny { reason: None });
                    continue;
                }
                match ask_approval(
                    &request.tool_name,
                    request.mode,
                    &request.subject,
                    &mut reader,
                    &mut writer,
                    &prompt_open_thread,
                    &cancelled_thread,
                ) {
                    Ok(decision) => {
                        let _ = request.reply.send(decision);
                    }
                    Err(_) => {
                        let _ = request.reply.send(ApprovalDecision::Deny { reason: None });
                        break;
                    }
                }
            }
        })
        .expect("spawn approval thread");

    ApproverHandle {
        prompt_open,
        cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tarz_core::definition::CommandNetwork;
    use tarz_core::permission::{ApprovalDecision, PermissionMode, WriteAction};

    #[test]
    fn allow_responses() {
        for input in ["y\n", "yes\n", "Y\n", "YES\n", "  y  \n"] {
            let subject = ApprovalSubject::FileWrite {
                path: "test.txt".into(),
                action: WriteAction::Created,
                diff: "+hello\n".into(),
            };
            let mut reader = Cursor::new(input.as_bytes());
            let mut writer = Vec::new();
            let prompt_open = AtomicBool::new(false);
            let cancelled = AtomicBool::new(false);

            let decision = ask_approval(
                "write_file",
                PermissionMode::Ask,
                &subject,
                &mut reader,
                &mut writer,
                &prompt_open,
                &cancelled,
            )
            .unwrap();
            assert_eq!(decision, ApprovalDecision::Allow, "input: {input}");
        }
    }

    #[test]
    fn mcp_call_allow_responses() {
        for input in ["y\n", "yes\n", "Y\n", "YES\n", "  y  \n"] {
            let subject = ApprovalSubject::McpCall {
                server: "srv".into(),
                tool: "echo".into(),
                arguments: "{\"msg\":\"hi\"}".into(),
            };
            let mut reader = Cursor::new(input.as_bytes());
            let mut writer = Vec::new();
            let prompt_open = AtomicBool::new(false);
            let cancelled = AtomicBool::new(false);

            let decision = ask_approval(
                "mcp_srv_echo",
                PermissionMode::Ask,
                &subject,
                &mut reader,
                &mut writer,
                &prompt_open,
                &cancelled,
            )
            .unwrap();
            assert_eq!(decision, ApprovalDecision::Allow, "input: {input}");
            let out_str = String::from_utf8(writer).unwrap();
            assert!(out_str.contains(
                "Tarz: mcp_srv_echo wants to call MCP tool srv/echo (permission mode: ask)"
            ));
            assert!(out_str.contains("{\"msg\":\"hi\"}"));
            assert!(out_str.contains("Allow this tool call? [y/n] "));
        }
    }

    #[test]
    fn mcp_call_deny_with_reason() {
        let subject = ApprovalSubject::McpCall {
            server: "srv".into(),
            tool: "mutate".into(),
            arguments: "{\"count\":1}".into(),
        };
        let mut reader = Cursor::new(b"n\nnot safe\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "mcp_srv_mutate",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny {
                reason: Some("not safe".into())
            }
        );
        let out_str = String::from_utf8(writer).unwrap();
        assert!(out_str.contains(
            "Tarz: mcp_srv_mutate wants to call MCP tool srv/mutate (permission mode: ask)"
        ));
        assert!(out_str.contains("Allow this tool call? [y/n] "));
        assert!(out_str.contains("Reason for the model (optional, Enter to skip): "));
    }

    #[test]
    fn mcp_call_escapes_untrusted_control_characters() {
        let subject = ApprovalSubject::McpCall {
            server: "s\x1b[31mrv".into(),
            tool: "to\x00ol".into(),
            arguments: "{\"text\":\"a\x1b[2Jb\"}".into(),
        };
        let mut reader = Cursor::new(b"y\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let _ = ask_approval(
            "mcp_bad\x07name",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        let out_str = String::from_utf8(writer).unwrap();
        assert!(!out_str.contains('\x1b'));
        assert!(!out_str.contains('\x00'));
        assert!(!out_str.contains('\x07'));
    }

    #[test]
    fn deny_with_reason() {
        let subject = ApprovalSubject::FileWrite {
            path: "test.txt".into(),
            action: WriteAction::Updated,
            diff: "-old\n+new\n".into(),
        };
        let mut reader = Cursor::new(b"n\npath is wrong\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "write_file",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(
            decision,
            ApprovalDecision::Deny {
                reason: Some("path is wrong".into())
            }
        );
        let out_str = String::from_utf8(writer).unwrap();
        assert!(out_str.contains("Reason for the model (optional, Enter to skip): "));
    }

    #[test]
    fn deny_with_empty_reason() {
        for input in [b"n\n\n".as_slice(), b"no\n   \n".as_slice()] {
            let subject = ApprovalSubject::FileWrite {
                path: "test.txt".into(),
                action: WriteAction::Updated,
                diff: "-old\n+new\n".into(),
            };
            let mut reader = Cursor::new(input);
            let mut writer = Vec::new();
            let prompt_open = AtomicBool::new(false);
            let cancelled = AtomicBool::new(false);

            let decision = ask_approval(
                "write_file",
                PermissionMode::Ask,
                &subject,
                &mut reader,
                &mut writer,
                &prompt_open,
                &cancelled,
            )
            .unwrap();
            assert_eq!(decision, ApprovalDecision::Deny { reason: None });
        }
    }

    #[test]
    fn invalid_input_reprompts_until_valid() {
        let subject = ApprovalSubject::FileWrite {
            path: "test.txt".into(),
            action: WriteAction::Created,
            diff: "+hello\n".into(),
        };
        let mut reader = Cursor::new(b"maybe\nwhat\ny\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "write_file",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);

        let out_str = String::from_utf8(writer).unwrap();
        assert_eq!(out_str.matches("Allow this write? [y/n] ").count(), 3);
    }

    #[test]
    fn command_displays_sandbox_details_and_prompts() {
        let subject = ApprovalSubject::Command {
            command: "cargo test".into(),
            workdir: "src".into(),
            timeout_secs: 30,
            network: CommandNetwork::None,
            outside_sandbox: false,
            justification: None,
        };
        let mut reader = Cursor::new(b"y\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let decision = ask_approval(
            "run_command",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
        let output = String::from_utf8(writer).unwrap();
        assert!(output.contains("Working directory: src"));
        assert!(output.contains("Timeout: 30s"));
        assert!(output.contains("Network: none"));
        assert!(output.contains(".git hooks and config read-only"));
        assert!(output.contains("credentials unreadable"));
        assert!(output.contains("$ cargo test"));
        assert!(output.contains("Allow this command? [y/n]"));
    }

    #[test]
    fn outside_command_displays_the_warning_and_justification() {
        let subject = ApprovalSubject::Command {
            command: "cargo test".into(),
            workdir: "src".into(),
            timeout_secs: 30,
            network: CommandNetwork::None,
            outside_sandbox: true,
            justification: Some("the sandbox blocks the user cargo cache".into()),
        };
        let mut reader = Cursor::new(b"y\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);
        let decision = ask_approval(
            "run_command",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
        let output = String::from_utf8(writer).unwrap();
        assert!(output.contains("outside the sandbox"));
        assert!(output.contains("NOT in the sandbox"));
        assert!(output.contains("Justification: the sandbox blocks the user cargo cache"));
        assert!(output.contains("Working directory: src"));
        assert!(output.contains("$ cargo test"));
        assert!(output.contains("Allow this command outside the sandbox? [y/n]"));
        assert!(!output.contains("Network: none"));
    }

    #[test]
    fn mcp_call_displays_truncated_arguments() {
        let subject = ApprovalSubject::McpCall {
            server: "srv".into(),
            tool: "process".into(),
            arguments: "{\"data\":\"long-value\"}... (truncated)".into(),
        };
        let mut reader = Cursor::new(b"y\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "mcp_srv_process",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
        let out_str = String::from_utf8(writer).unwrap();
        assert!(out_str.contains("... (truncated)"));
    }

    #[test]
    fn eof_is_denied_without_reason() {
        let subject = ApprovalSubject::FileWrite {
            path: "test.txt".into(),
            action: WriteAction::Created,
            diff: "+hello\n".into(),
        };
        let mut reader = Cursor::new(b"");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "write_file",
            PermissionMode::Ask,
            &subject,
            &mut reader,
            &mut writer,
            &prompt_open,
            &cancelled,
        )
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny { reason: None });
    }
}
