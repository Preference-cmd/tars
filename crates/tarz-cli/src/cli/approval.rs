//! Terminal approval interaction and request handling.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tarz_core::permission::{
    ApprovalDecision, ApprovalSubject, PermissionMode, WriteAction, escape_untrusted_text,
};

/// Determine whether the CLI approval channel should be connected.
pub(crate) fn should_connect_approvals(
    stdin_is_terminal: bool,
    stderr_is_terminal: bool,
    used_stdin_input: bool,
) -> bool {
    stdin_is_terminal && stderr_is_terminal && !used_stdin_input
}

/// Determine whether the CLI approval channel should be connected using process stdio.
pub(crate) fn should_connect_stdio(used_stdin_input: bool) -> bool {
    use std::io::IsTerminal;
    should_connect_approvals(
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        used_stdin_input,
    )
}

/// Present an approval request to the user and read their decision.
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
    }

    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(ApprovalDecision::Deny { reason: None });
        }

        write!(writer, "Allow this write? [y/n] ")?;
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

/// Handle to an active background approval responder thread.
pub(crate) struct ApproverHandle {
    prompt_open: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

impl ApproverHandle {
    /// Signal cancellation to the approval thread and emit a newline if a prompt is open.
    pub(crate) fn on_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if self.prompt_open.swap(false, Ordering::SeqCst) {
            let _ = writeln!(std::io::stderr().lock());
        }
    }

    /// Mark the approver as finished when execution completes.
    pub(crate) fn finish(self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Spawn the approval responder thread for the given session.
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
    fn output_format_contains_required_fields() {
        let subject = ApprovalSubject::FileWrite {
            path: "notes/a.txt".into(),
            action: WriteAction::Updated,
            diff: "--- a/notes/a.txt\n+++ b/notes/a.txt\n@@ -1 +1 @@\n-old\n+new\n".into(),
        };
        let mut reader = Cursor::new(b"y\n");
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

        assert!(
            out_str
                .contains("Tarz: write_file wants to update notes/a.txt (permission mode: ask)\n")
        );
        assert!(
            out_str.contains("--- a/notes/a.txt\n+++ b/notes/a.txt\n@@ -1 +1 @@\n-old\n+new\n")
        );
        assert!(out_str.contains("Allow this write? [y/n] "));
    }

    #[test]
    fn ask_approval_escapes_diff_and_path_and_tool() {
        let subject = ApprovalSubject::FileWrite {
            path: "dangerous\x1b[31m\u{061C}.txt".into(),
            action: WriteAction::Created,
            diff: "line1\x1b[32m\u{2028}\r\0\x08line2\n".into(),
        };
        let mut reader = std::io::Cursor::new(b"y\n");
        let mut writer = Vec::new();
        let prompt_open = AtomicBool::new(false);
        let cancelled = AtomicBool::new(false);

        let decision = ask_approval(
            "write_file\x07",
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

        assert!(out_str.contains("write_file\\x07"));
        assert!(out_str.contains("dangerous\\x1b[31m\\u{61c}.txt"));
        assert!(out_str.contains("line1\\x1b[32m\\u{2028}\\r\\0\\bline2\n"));
        assert!(!out_str.contains('\x1b'));
        assert!(!out_str.contains('\u{061C}'));
        assert!(!out_str.contains('\u{2028}'));
        assert!(!out_str.contains('\r'));
        assert!(!out_str.contains('\0'));
        assert!(!out_str.contains('\x08'));
        assert!(!out_str.contains('\x07'));
    }

    #[test]
    fn connection_condition_combinations() {
        for stdin_term in [false, true] {
            for stderr_term in [false, true] {
                for used_stdin in [false, true] {
                    let expected = stdin_term && stderr_term && !used_stdin;
                    assert_eq!(
                        should_connect_approvals(stdin_term, stderr_term, used_stdin),
                        expected,
                        "failed for ({stdin_term}, {stderr_term}, {used_stdin})"
                    );
                }
            }
        }
    }
}
