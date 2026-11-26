use std::io::{Read, Write};

/// Maximum size accepted when importing a prompt back from an external editor (1 MiB).
pub(crate) const MAX_EDITOR_IMPORT_BYTES: usize = 1024 * 1024;

/// Parsed external editor command and fixed leading arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EditorCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

/// Resolve the configured external editor from `$VISUAL` then `$EDITOR`.
pub(crate) fn resolve_editor_command() -> Result<EditorCommand, String> {
    let visual = read_env_utf8("VISUAL")?;
    let editor = read_env_utf8("EDITOR")?;
    resolve_editor_command_from(visual.as_deref(), editor.as_deref())
}

fn read_env_utf8(name: &str) -> Result<Option<String>, String> {
    match std::env::var_os(name) {
        None => Ok(None),
        Some(value) => value
            .into_string()
            .map(Some)
            .map_err(|_| format!("${name} contains invalid UTF-8")),
    }
}

/// Pure helper resolving `$VISUAL` (preferred) or `$EDITOR` from supplied strings.
pub(crate) fn resolve_editor_command_from(
    visual: Option<&str>,
    editor: Option<&str>,
) -> Result<EditorCommand, String> {
    if let Some(raw) = visual.map(str::trim).filter(|s| !s.is_empty()) {
        return parse_editor_command(raw);
    }
    if let Some(raw) = editor.map(str::trim).filter(|s| !s.is_empty()) {
        return parse_editor_command(raw);
    }
    Err("Set $VISUAL or $EDITOR to edit prompts externally".into())
}

/// Tokenize an editor command line without invoking a shell.
///
/// Supports whitespace splitting, single quotes (`'...'` literal), double quotes
/// (`"..."` with `\"` and `\\` escapes), and backslash escapes outside quotes.
pub(crate) fn parse_editor_command(raw: &str) -> Result<EditorCommand, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote = Quote::None;
    let mut chars = raw.chars();

    while let Some(c) = chars.next() {
        match quote {
            Quote::None => {
                if c.is_whitespace() {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else if c == '\'' {
                    in_token = true;
                    quote = Quote::Single;
                } else if c == '"' {
                    in_token = true;
                    quote = Quote::Double;
                } else if c == '\\' {
                    in_token = true;
                    let next = chars
                        .next()
                        .ok_or_else(|| "Trailing backslash in editor command".to_owned())?;
                    current.push(next);
                } else {
                    in_token = true;
                    current.push(c);
                }
            }
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                } else {
                    current.push(c);
                }
            }
            Quote::Double => {
                if c == '"' {
                    quote = Quote::None;
                } else if c == '\\' {
                    let next = chars
                        .next()
                        .ok_or_else(|| "Unterminated double quote in editor command".to_owned())?;
                    if matches!(next, '"' | '\\') {
                        current.push(next);
                    } else {
                        current.push('\\');
                        current.push(next);
                    }
                } else {
                    current.push(c);
                }
            }
        }
    }

    match quote {
        Quote::Single => return Err("Unterminated single quote in editor command".into()),
        Quote::Double => return Err("Unterminated double quote in editor command".into()),
        Quote::None => {}
    }
    if in_token {
        tokens.push(current);
    }
    if tokens.is_empty() || tokens[0].is_empty() {
        return Err("Editor command is empty".into());
    }
    let program = tokens.remove(0);
    Ok(EditorCommand {
        program,
        args: tokens,
    })
}

/// Write `initial` to an exclusive `.md` temp file, run `cmd` directly, and
/// return the validated, newline-normalized result when the editor exits 0.
pub(crate) fn edit_prompt_with_command(
    cmd: &EditorCommand,
    initial: &str,
) -> Result<String, String> {
    let mut temp = tempfile::Builder::new()
        .prefix("tars-prompt-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| format!("Failed to create temporary prompt file: {e}"))?;
    temp.write_all(initial.as_bytes())
        .map_err(|e| format!("Failed to write temporary prompt file: {e}"))?;
    temp.flush()
        .map_err(|e| format!("Failed to flush temporary prompt file: {e}"))?;
    // Close the open descriptor before spawning the editor while retaining
    // TempPath ownership so atomic-rename editors (e.g. vim) work cleanly and
    // the path is still deleted when `temp_path` drops.
    let temp_path = temp.into_temp_path();

    let status = std::process::Command::new(&cmd.program)
        .args(&cmd.args)
        .arg(temp_path.as_os_str())
        .status()
        .map_err(|e| format!("Failed to launch editor '{}': {e}", cmd.program))?;
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("Editor exited with status {code}; draft preserved"),
            None => "Editor terminated by signal; draft preserved".into(),
        });
    }

    let file = std::fs::File::open(&temp_path)
        .map_err(|e| format!("Failed to read edited prompt file: {e}"))?;
    let mut buf = Vec::new();
    file.take((MAX_EDITOR_IMPORT_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Failed to read edited prompt file: {e}"))?;
    if buf.len() > MAX_EDITOR_IMPORT_BYTES {
        return Err(format!(
            "Edited prompt exceeds {} MiB limit; draft preserved",
            MAX_EDITOR_IMPORT_BYTES / (1024 * 1024)
        ));
    }
    let text = String::from_utf8(buf)
        .map_err(|_| "Edited prompt is not valid UTF-8; draft preserved".to_owned())?;
    Ok(normalize_editor_text(&text))
}

fn normalize_editor_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let stripped = normalized.trim_end_matches('\r').replace('\r', "\n");
    stripped
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_visual_and_rejects_missing_or_blank() {
        let cmd = resolve_editor_command_from(Some("nvim -c 'set ft=markdown'"), Some("nano"))
            .expect("VISUAL selected");
        assert_eq!(cmd.program, "nvim");
        assert_eq!(cmd.args, vec!["-c", "set ft=markdown"]);

        let fallback = resolve_editor_command_from(Some("   "), Some("code --wait"))
            .expect("blank VISUAL falls back to EDITOR");
        assert_eq!(fallback.program, "code");
        assert_eq!(fallback.args, vec!["--wait"]);

        let err = resolve_editor_command_from(None, Some("  \t ")).unwrap_err();
        assert!(err.contains("$VISUAL") && err.contains("$EDITOR"), "{err}");
        assert!(resolve_editor_command_from(None, None).is_err());
    }

    #[test]
    fn parse_editor_command_handles_quotes_escapes_and_rejects_malformed() {
        let parsed = parse_editor_command(
            r#""/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl" -w --msg="a \"b\" \\c" 'lit $1'"#,
        )
        .unwrap();
        assert_eq!(
            parsed.program,
            "/Applications/Sublime Text.app/Contents/SharedSupport/bin/subl"
        );
        assert_eq!(parsed.args, vec!["-w", "--msg=a \"b\" \\c", "lit $1"]);

        let escaped = parse_editor_command(r"my\ editor --flag").unwrap();
        assert_eq!(escaped.program, "my editor");
        assert_eq!(escaped.args, vec!["--flag"]);

        assert!(parse_editor_command("").is_err());
        assert!(parse_editor_command("   ").is_err());
        assert!(parse_editor_command("vim 'unterminated").is_err());
        assert!(parse_editor_command("vim \"unterminated").is_err());
        assert!(parse_editor_command("vim \\").is_err());
    }

    #[cfg(unix)]
    fn write_test_editor_script(dir: &std::path::Path, name: &str, body: &str) -> EditorCommand {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        EditorCommand {
            program: path.to_string_lossy().into_owned(),
            args: Vec::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_editor_supports_atomic_rename_crlf_and_rejects_failures() {
        let dir = tempfile::tempdir().unwrap();

        // 1. Vim-style atomic rename replacement + CRLF + trailing CR normalization.
        let rename_cmd = write_test_editor_script(
            dir.path(),
            "rename_editor.sh",
            r#"
target="$1"
initial="$(cat "$target")"
tmp="${target}.swap"
printf '%s\r\nsecond line\r' "$initial" > "$tmp"
mv "$tmp" "$target"
"#,
        );
        let edited = edit_prompt_with_command(&rename_cmd, "first line").unwrap();
        assert_eq!(edited, "first line\nsecond line");

        // 2. Non-zero exit code preserves caller draft by returning Err.
        let fail_cmd = write_test_editor_script(dir.path(), "fail_editor.sh", "exit 7");
        let err = edit_prompt_with_command(&fail_cmd, "keep me").unwrap_err();
        assert!(err.contains("status 7"), "{err}");

        // 3. Non-UTF-8 file contents are rejected.
        let bad_utf8_cmd =
            write_test_editor_script(dir.path(), "bad_utf8.sh", r#"printf '\377\376\000' > "$1""#);
        let err = edit_prompt_with_command(&bad_utf8_cmd, "keep me").unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");

        // 4. Over 1 MiB output is rejected.
        let big_cmd = write_test_editor_script(
            dir.path(),
            "big_editor.sh",
            r#"dd if=/dev/zero bs=1024 count=1025 2>/dev/null | tr '\0' 'a' > "$1""#,
        );
        let err = edit_prompt_with_command(&big_cmd, "keep me").unwrap_err();
        assert!(err.contains("1 MiB"), "{err}");
    }
}
