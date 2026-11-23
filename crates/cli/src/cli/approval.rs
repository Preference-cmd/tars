//! Terminal approval interaction and request handling.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tars_core::permission::{
    ApprovalDecision, ApprovalSubject, CommandTarget, PermissionMode, WriteAction, command_body,
    escape_untrusted_text,
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
                "TARS: {} wants to {} {} (permission mode: {})",
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
            target,
            suggested_prefixes,
            can_save_workspace_rule,
            ..
        } => {
            writeln!(
                writer,
                "TARS: {} wants to run a command{} (permission mode: {})",
                escape_untrusted_text(tool_name),
                if matches!(target, CommandTarget::Outside { .. }) {
                    " outside the sandbox"
                } else {
                    ""
                },
                mode,
            )?;
            for line in command_body(subject).expect("command subject") {
                writeln!(writer, "{line}")?;
            }
            if !suggested_prefixes.is_empty() {
                writeln!(writer, "Options:")?;
                writeln!(writer, "  [1/y] Allow once")?;
                let outside_note = if matches!(target, CommandTarget::Outside { .. }) {
                    "; includes outside_sandbox = true"
                } else {
                    ""
                };
                writeln!(
                    writer,
                    "  [2/s] Allow for this session (in-memory only{outside_note})"
                )?;
                if *can_save_workspace_rule {
                    writeln!(
                        writer,
                        "  [3/w] Always allow in this workspace (.tars/permissions.toml{outside_note})"
                    )?;
                    writeln!(writer, "  [4/n] Deny")?;
                } else {
                    writeln!(writer, "  [3/n] Deny")?;
                }
            }
            writer.flush()?;
        }
        ApprovalSubject::McpCall {
            server,
            tool,
            arguments,
        } => {
            writeln!(
                writer,
                "TARS: {} wants to call MCP tool {}/{} (permission mode: {})",
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

    let (has_rules, can_save_workspace) = match subject {
        ApprovalSubject::Command {
            suggested_prefixes,
            can_save_workspace_rule,
            ..
        } => (!suggested_prefixes.is_empty(), *can_save_workspace_rule),
        _ => (false, false),
    };

    let prompt_text = match subject {
        ApprovalSubject::FileWrite { .. } => "Allow this write? [y/n] ",
        ApprovalSubject::Command {
            target: CommandTarget::Outside { .. },
            ..
        } if !has_rules => "Allow this command outside the sandbox? [y/n] ",
        ApprovalSubject::Command {
            target: CommandTarget::Outside { .. },
            ..
        } if can_save_workspace => "Allow this command outside the sandbox? [1/y, 2/s, 3/w, 4/n] ",
        ApprovalSubject::Command {
            target: CommandTarget::Outside { .. },
            ..
        } => "Allow this command outside the sandbox? [1/y, 2/s, 3/n] ",
        ApprovalSubject::Command { .. } if !has_rules => "Allow this command? [y/n] ",
        ApprovalSubject::Command { .. } if can_save_workspace => {
            "Allow this command? [1/y, 2/s, 3/w, 4/n] "
        }
        ApprovalSubject::Command { .. } => "Allow this command? [1/y, 2/s, 3/n] ",
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
        if trimmed.eq_ignore_ascii_case("y")
            || trimmed.eq_ignore_ascii_case("yes")
            || (has_rules && trimmed == "1")
        {
            return Ok(ApprovalDecision::Allow);
        }
        if has_rules
            && (trimmed == "2"
                || trimmed.eq_ignore_ascii_case("s")
                || trimmed.eq_ignore_ascii_case("session"))
        {
            return Ok(ApprovalDecision::AllowForSession);
        }
        if has_rules
            && can_save_workspace
            && (trimmed == "3"
                || trimmed.eq_ignore_ascii_case("w")
                || trimmed.eq_ignore_ascii_case("workspace"))
        {
            return Ok(ApprovalDecision::AllowInWorkspace);
        }
        let is_deny = trimmed.eq_ignore_ascii_case("n")
            || trimmed.eq_ignore_ascii_case("no")
            || (has_rules && !can_save_workspace && trimmed == "3")
            || (has_rules && can_save_workspace && trimmed == "4");
        if is_deny {
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

pub(crate) fn spawn_approver(session: &tars_core::application::ManagedSession) -> ApproverHandle {
    let mut rx = session.connect_approvals();
    let prompt_open = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let prompt_open_thread = prompt_open.clone();
    let cancelled_thread = cancelled.clone();

    std::thread::Builder::new()
        .name("tars-approval".into())
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
    use tars_core::definition::CommandNetwork;
    use tars_core::permission::{ApprovalDecision, PermissionMode, WriteAction};

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
                "TARS: mcp_srv_echo wants to call MCP tool srv/echo (permission mode: ask)"
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
            "TARS: mcp_srv_mutate wants to call MCP tool srv/mutate (permission mode: ask)"
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
            target: CommandTarget::Sandboxed {
                network: CommandNetwork::None,
            },
            suggested_prefixes: Vec::new(),
            can_save_workspace_rule: false,
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
            target: CommandTarget::Outside {
                justification: "the sandbox blocks the user cargo cache".into(),
            },
            suggested_prefixes: Vec::new(),
            can_save_workspace_rule: false,
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

    #[test]
    fn command_with_rules_and_trusted_workspace_offers_four_options() {
        for (input, expected) in [
            ("1\n", ApprovalDecision::Allow),
            ("y\n", ApprovalDecision::Allow),
            ("2\n", ApprovalDecision::AllowForSession),
            ("s\n", ApprovalDecision::AllowForSession),
            ("3\n", ApprovalDecision::AllowInWorkspace),
            ("w\n", ApprovalDecision::AllowInWorkspace),
            (
                "4\nnot safe\n",
                ApprovalDecision::Deny {
                    reason: Some("not safe".into()),
                },
            ),
            ("n\n\n", ApprovalDecision::Deny { reason: None }),
        ] {
            let subject = ApprovalSubject::Command {
                command: "cargo test -p x".into(),
                workdir: ".".into(),
                timeout_secs: 60,
                target: CommandTarget::Sandboxed {
                    network: CommandNetwork::None,
                },
                suggested_prefixes: vec![vec!["cargo".into(), "test".into()]],
                can_save_workspace_rule: true,
            };
            let mut reader = Cursor::new(input.as_bytes());
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
            assert_eq!(decision, expected, "input: {input}");
            let out = String::from_utf8(writer).unwrap();
            assert!(out.contains("Rule prefix: cargo test"), "{out}");
            assert!(out.contains("[1/y] Allow once"), "{out}");
            assert!(out.contains("[2/s] Allow for this session"), "{out}");
            assert!(
                out.contains("[3/w] Always allow in this workspace"),
                "{out}"
            );
            assert!(out.contains("[4/n] Deny"), "{out}");
            assert!(out.contains("[1/y, 2/s, 3/w, 4/n]"), "{out}");
        }
    }

    #[test]
    fn command_with_untrusted_workspace_offers_three_options_and_rejects_w() {
        let subject = ApprovalSubject::Command {
            command: "git push origin main".into(),
            workdir: ".".into(),
            timeout_secs: 60,
            target: CommandTarget::Outside {
                justification: "needs ssh".into(),
            },
            suggested_prefixes: vec![vec!["git".into(), "push".into()]],
            can_save_workspace_rule: false,
        };
        let mut reader = Cursor::new(b"w\n4\n2\n");
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
        assert_eq!(decision, ApprovalDecision::AllowForSession);
        let out = String::from_utf8(writer).unwrap();
        assert!(
            out.contains("Rule prefix: git push (includes outside_sandbox = true)"),
            "{out}"
        );
        assert!(out.contains("[3/n] Deny"), "{out}");
        assert!(!out.contains("[3/w]"));
        assert!(!out.contains("Always allow in this workspace"));
        assert_eq!(
            out.matches("Allow this command outside the sandbox? [1/y, 2/s, 3/n]")
                .count(),
            3
        );
    }

    #[test]
    fn unmatchable_command_offers_two_options_and_ignores_s_and_w() {
        let subject = ApprovalSubject::Command {
            command: "echo $HOME".into(),
            workdir: ".".into(),
            timeout_secs: 60,
            target: CommandTarget::Sandboxed {
                network: CommandNetwork::None,
            },
            suggested_prefixes: Vec::new(),
            can_save_workspace_rule: false,
        };
        let mut reader = Cursor::new(b"s\nw\n2\n3\ny\n");
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
        let out = String::from_utf8(writer).unwrap();
        assert!(!out.contains("Rule prefix:"));
        assert!(!out.contains("Allow for this session"));
        assert_eq!(out.matches("Allow this command? [y/n] ").count(), 5);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cli_approver_workspace_choice_persists_rules_and_gitignore_end_to_end() {
        use tars_core::application::{ClosePolicy, PreparedCreate, SessionHost, text_input};
        use tars_core::definition::{
            CommandSettings, CredentialRef, Provider, SessionDefinition, WorkspaceTool,
            WorkspaceToolLimits,
        };
        use tars_core::session::{SubmitRequest, WorkState};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let sse1 = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "id": "chat1",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "c1",
                                "type": "function",
                                "function": {
                                    "name": "run_command",
                                    "arguments": "{\"command\":\"echo cli-ws\"}"
                                }
                            }]
                        },
                        "finish_reason": null
                    }]
                }),
                serde_json::json!({
                    "id": "chat1",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "tool_calls"
                    }]
                }),
            );
            let sse2 = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({
                    "id": "chat1",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": "cli done" },
                        "finish_reason": null
                    }]
                }),
                serde_json::json!({
                    "id": "chat1",
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "stop"
                    }]
                }),
            );
            for body in [sse1, sse2] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let (header_end, content_len) = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    data.extend_from_slice(&chunk[..count]);
                    if let Some(end) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&data[..end]);
                        let len = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, len);
                    }
                };
                while data.len() < header_end + content_len {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    data.extend_from_slice(&chunk[..count]);
                }
                let payload = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(payload.as_bytes()).await.unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let user_rules = temp.path().join("user-permissions.toml");

        let def = SessionDefinition {
            provider: Provider::OpenAiChat,
            model: "test-model".into(),
            endpoint: Some(endpoint),
            credential: CredentialRef::Environment("PATH".into()),
            work_timeout_secs: Some(30),
            retained_work_capacity: 4,
            max_model_rounds: 4,
            max_tool_calls: 4,
            max_tokens: None,
            reasoning_effort: None,
            extra_body: Default::default(),
            max_model_attempts: None,
            render_options: Default::default(),
            workspace_root: Some(workspace.clone()),
            tools: vec![WorkspaceTool::RunCommand],
            tool_limits: WorkspaceToolLimits::default(),
            write_limits: None,
            permission_mode: Some(PermissionMode::Ask),
            command: Some(CommandSettings::default()),
            mcp_servers: Vec::new(),
            context_window: None,
            auto_compact: true,
            compact_threshold: 70,
            prompt_cache: true,
            prelude: Default::default(),
            code_execution: None,
        };

        let host = SessionHost::new(Some(store)).with_user_rules_path(Some(user_rules.clone()));
        let session = host
            .create(PreparedCreate::persistent(
                def,
                causa::kernel::Context::default(),
            ))
            .await
            .unwrap();
        let mut rx = session.connect_approvals();
        let approver_task = tokio::task::spawn_blocking(move || {
            let req = rx.blocking_recv().unwrap();
            let mut reader = Cursor::new(b"w\n".as_slice());
            let mut writer = Vec::new();
            let prompt_open = AtomicBool::new(false);
            let cancelled = AtomicBool::new(false);
            let decision = ask_approval(
                &req.tool_name,
                req.mode,
                &req.subject,
                &mut reader,
                &mut writer,
                &prompt_open,
                &cancelled,
            )
            .unwrap();
            assert_eq!(decision, ApprovalDecision::AllowInWorkspace);
            req.reply.send(decision).unwrap();
        });

        let receipt = session
            .submit(SubmitRequest {
                request_key: "cli-ws-1".into(),
                blocks: vec![text_input("run echo")],
            })
            .unwrap();
        loop {
            let obs = session
                .wait(&receipt.work, std::time::Duration::from_secs(10))
                .await
                .unwrap();
            if matches!(obs.state, WorkState::Finished(_)) {
                break;
            }
        }
        approver_task.await.unwrap();
        server.await.unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.join(".tars/.gitignore")).unwrap(),
            "*\n"
        );
        let rules_toml = std::fs::read_to_string(workspace.join(".tars/permissions.toml")).unwrap();
        assert!(
            rules_toml.contains("prefix = [\"echo\", \"cli-ws\"]"),
            "{rules_toml}"
        );
        assert!(!user_rules.exists());
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}
