use crate::app::{COMMANDS, matching_commands};
use ratatui_textarea::{CursorMove, TextArea};
use std::path::Path;
use tars_core::permission::escape_untrusted_text;
use tars_core::workspace::{WorkspacePathCandidate, complete_workspace_paths};

const MAX_WORKSPACE_CANDIDATES: usize = 64;
const MAX_NOTICE_CANDIDATES: usize = 6;

pub(crate) enum CompleteResult {
    None,
    Modified { notice: Option<String> },
    Notice(String),
}

pub(crate) fn set_editor_text(
    editor: &mut TextArea<'static>,
    text: &str,
    cursor_at_first_line_end: bool,
) {
    let mut next = super::editor();
    if !text.is_empty() {
        next.insert_str(text);
    }
    if cursor_at_first_line_end {
        next.move_cursor(CursorMove::Top);
        next.move_cursor(CursorMove::End);
    } else {
        next.move_cursor(CursorMove::Bottom);
        next.move_cursor(CursorMove::End);
    }
    *editor = next;
}

pub(crate) fn complete_editor(
    editor: &mut TextArea<'static>,
    workspace_root: Option<&Path>,
    profile_names: &[String],
) -> CompleteResult {
    let cursor = editor.cursor();
    let (line_count, line) = {
        let lines = editor.lines();
        let Some(line) = lines.get(cursor.0) else {
            return CompleteResult::None;
        };
        (lines.len(), line.clone())
    };
    let cursor_byte = char_col_to_byte(&line, cursor.1);
    let before_cursor = &line[..cursor_byte];

    if line_count == 1
        && before_cursor.starts_with('/')
        && !before_cursor.chars().any(char::is_whitespace)
    {
        return complete_slash_command(editor, &line, cursor_byte);
    }

    if line_count == 1
        && before_cursor.starts_with('/')
        && let Some(arg_prefix) = super::image::strip_image_command_prefix(before_cursor)
    {
        return complete_image_in_editor(editor, &line, cursor_byte, arg_prefix, workspace_root);
    }

    if line_count == 1
        && before_cursor.starts_with('/')
        && let Some(arg_prefix) = strip_model_command_prefix(before_cursor)
    {
        return complete_model_in_editor(editor, &line, cursor_byte, arg_prefix, profile_names);
    }

    if line_count == 1
        && before_cursor.starts_with('/')
        && let Some(arg_prefix) = strip_copy_command_prefix(before_cursor)
    {
        return complete_copy_in_editor(editor, &line, cursor_byte, arg_prefix);
    }

    let token_start = before_cursor
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(idx, c)| idx + c.len_utf8());
    let token = &before_cursor[token_start..];
    if let Some(raw_prefix) = token.strip_prefix('@') {
        return complete_at_path(
            editor,
            cursor.0,
            &line,
            token_start,
            cursor_byte,
            raw_prefix,
            workspace_root,
        );
    }

    CompleteResult::None
}

pub(crate) fn slash_command_takes_args(cmd: &str) -> bool {
    matches!(
        cmd,
        "mode"
            | "resume"
            | "model"
            | "copy"
            | "auto_compact"
            | "agents"
            | "rules"
            | "queue"
            | "rename"
            | "fork"
            | "image"
            | "export-md"
    )
}

pub(crate) enum ModelArgCompletion {
    None,
    Modified {
        replacement: String,
        notice: Option<String>,
    },
    Notice(String),
}

pub(crate) fn strip_model_command_prefix(input: &str) -> Option<&str> {
    let rest = input
        .strip_prefix("/model")
        .or_else(|| input.strip_prefix("model"))?;
    let first = rest.chars().next()?;
    first.is_whitespace().then(|| rest.trim_start())
}

pub(crate) fn complete_model_arg(arg_prefix: &str, candidates: &[String]) -> ModelArgCompletion {
    if arg_prefix.chars().any(char::is_whitespace) {
        return ModelArgCompletion::None;
    }
    if candidates.is_empty() {
        return ModelArgCompletion::Notice("No model profiles configured".into());
    }
    let mut matches: Vec<&str> = candidates
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with(arg_prefix))
        .collect();
    if matches.is_empty() && !arg_prefix.is_empty() {
        let lower = arg_prefix.to_ascii_lowercase();
        matches = candidates
            .iter()
            .map(String::as_str)
            .filter(|name| name.to_ascii_lowercase().starts_with(&lower))
            .collect();
    }
    match matches.as_slice() {
        [] => ModelArgCompletion::Notice(format!(
            "No matching model or profile for '{}'",
            escape_untrusted_text(arg_prefix)
        )),
        [single] => ModelArgCompletion::Modified {
            replacement: (*single).to_owned(),
            notice: None,
        },
        many => {
            let lcp = longest_common_prefix(many);
            let notice = format_candidate_notice(
                "Models",
                many.iter().map(|name| escape_untrusted_text(name)),
            );
            if lcp.len() > arg_prefix.len() {
                ModelArgCompletion::Modified {
                    replacement: lcp,
                    notice: Some(notice),
                }
            } else {
                ModelArgCompletion::Notice(notice)
            }
        }
    }
}

fn apply_arg_completion(
    editor: &mut TextArea<'static>,
    line: &str,
    cursor_byte: usize,
    arg_prefix: &str,
    replacement: String,
    notice: Option<String>,
) -> CompleteResult {
    let command_prefix_len = cursor_byte - arg_prefix.len();
    let command_prefix = &line[..command_prefix_len];
    let completed = format!("{command_prefix}{replacement}");
    let new_line = format!("{completed}{}", &line[cursor_byte..]);
    let target_col = completed.chars().count();
    replace_line_and_move_cursor(editor, 0, new_line, target_col);
    CompleteResult::Modified { notice }
}

fn complete_model_in_editor(
    editor: &mut TextArea<'static>,
    line: &str,
    cursor_byte: usize,
    arg_prefix: &str,
    profile_names: &[String],
) -> CompleteResult {
    match complete_model_arg(arg_prefix, profile_names) {
        ModelArgCompletion::None => CompleteResult::None,
        ModelArgCompletion::Modified {
            replacement,
            notice,
        } => apply_arg_completion(editor, line, cursor_byte, arg_prefix, replacement, notice),
        ModelArgCompletion::Notice(notice) => CompleteResult::Notice(notice),
    }
}

pub(crate) fn strip_copy_command_prefix(input: &str) -> Option<&str> {
    let rest = input
        .strip_prefix("/copy")
        .or_else(|| input.strip_prefix("copy"))?;
    let first = rest.chars().next()?;
    first.is_whitespace().then(|| rest.trim_start())
}

pub(crate) fn complete_copy_arg(arg_prefix: &str) -> ModelArgCompletion {
    const COPY_SUBCOMMANDS: [&str; 2] = ["code", "id"];
    if arg_prefix.chars().any(char::is_whitespace) {
        let lower = arg_prefix.to_ascii_lowercase();
        if lower.starts_with("code ") || lower.starts_with("code\t") {
            return ModelArgCompletion::Notice("Usage: /copy code [N] (1-based index)".into());
        }
        return ModelArgCompletion::None;
    }
    let lower = arg_prefix.to_ascii_lowercase();
    let matches: Vec<&str> = COPY_SUBCOMMANDS
        .iter()
        .copied()
        .filter(|sub| sub.starts_with(&lower))
        .collect();
    match matches.as_slice() {
        [] => ModelArgCompletion::Notice(format!(
            "No matching /copy subcommand for '{}'",
            escape_untrusted_text(arg_prefix)
        )),
        [single] => {
            if *single == lower && *single == "code" {
                ModelArgCompletion::Notice("Usage: /copy code [N] (1-based index)".into())
            } else if *single == lower {
                ModelArgCompletion::None
            } else {
                ModelArgCompletion::Modified {
                    replacement: (*single).to_owned(),
                    notice: None,
                }
            }
        }
        many => ModelArgCompletion::Notice(format_candidate_notice(
            "Copy",
            many.iter().map(|s| (*s).to_owned()),
        )),
    }
}

fn complete_copy_in_editor(
    editor: &mut TextArea<'static>,
    line: &str,
    cursor_byte: usize,
    arg_prefix: &str,
) -> CompleteResult {
    match complete_copy_arg(arg_prefix) {
        ModelArgCompletion::None => CompleteResult::None,
        ModelArgCompletion::Modified {
            replacement,
            notice,
        } => apply_arg_completion(editor, line, cursor_byte, arg_prefix, replacement, notice),
        ModelArgCompletion::Notice(notice) => CompleteResult::Notice(notice),
    }
}

fn complete_slash_command(
    editor: &mut TextArea<'static>,
    line: &str,
    cursor_byte: usize,
) -> CompleteResult {
    let prefix = line[..cursor_byte].strip_prefix('/').unwrap_or("");
    let lower = prefix.to_lowercase();
    let mut matches: Vec<&str> = COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(&lower))
        .map(|(name, _)| *name)
        .collect();
    if matches.is_empty() && !lower.is_empty() {
        matches = matching_commands(&lower)
            .into_iter()
            .map(|idx| COMMANDS[idx].0)
            .collect();
    }

    match matches.as_slice() {
        [] => CompleteResult::Notice(format!(
            "No matching command for '/{}'",
            escape_untrusted_text(prefix)
        )),
        [cmd] => {
            let replacement = if slash_command_takes_args(cmd) {
                format!("/{cmd} ")
            } else {
                format!("/{cmd}")
            };
            let new_line = format!("{replacement}{}", &line[cursor_byte..]);
            let target_col = replacement.chars().count();
            replace_line_and_move_cursor(editor, 0, new_line, target_col);
            CompleteResult::Modified { notice: None }
        }
        many => {
            let lcp = longest_common_prefix(many);
            let notice = format_candidate_notice("Commands", many.iter().map(|s| format!("/{s}")));
            if lcp.len() > lower.len() {
                let replacement = format!("/{lcp}");
                let new_line = format!("{replacement}{}", &line[cursor_byte..]);
                let target_col = replacement.chars().count();
                replace_line_and_move_cursor(editor, 0, new_line, target_col);
                CompleteResult::Modified {
                    notice: Some(notice),
                }
            } else {
                CompleteResult::Notice(notice)
            }
        }
    }
}

fn complete_image_in_editor(
    editor: &mut TextArea<'static>,
    line: &str,
    cursor_byte: usize,
    arg_prefix: &str,
    workspace_root: Option<&Path>,
) -> CompleteResult {
    let command_prefix_len = cursor_byte - arg_prefix.len();
    let command_prefix = &line[..command_prefix_len];
    match super::image::complete_image_arg(arg_prefix, workspace_root) {
        super::image::ImageArgCompletion::None => CompleteResult::None,
        super::image::ImageArgCompletion::Modified {
            replacement,
            notice,
        } => {
            let completed = format!("{command_prefix}{replacement}");
            let new_line = format!("{completed}{}", &line[cursor_byte..]);
            let target_col = completed.chars().count();
            replace_line_and_move_cursor(editor, 0, new_line, target_col);
            CompleteResult::Modified { notice }
        }
        super::image::ImageArgCompletion::Notice(notice) => CompleteResult::Notice(notice),
    }
}

fn complete_at_path(
    editor: &mut TextArea<'static>,
    row: usize,
    line: &str,
    token_start: usize,
    cursor_byte: usize,
    raw_prefix: &str,
    workspace_root: Option<&Path>,
) -> CompleteResult {
    if raw_prefix == ".."
        || raw_prefix.starts_with('/')
        || raw_prefix.contains('\0')
        || raw_prefix.contains('\\')
        || raw_prefix.split('/').any(|part| part == "..")
    {
        return CompleteResult::Notice(format!(
            "Invalid workspace path '@{}'",
            escape_untrusted_text(raw_prefix)
        ));
    }
    let Some(root) = workspace_root else {
        return CompleteResult::Notice("No workspace root configured for this session".into());
    };

    let candidates = complete_workspace_paths(root, raw_prefix, MAX_WORKSPACE_CANDIDATES);
    match candidates.as_slice() {
        [] => CompleteResult::Notice(format!(
            "No workspace path matches '@{}'",
            escape_untrusted_text(raw_prefix)
        )),
        [single] => {
            let suffix = if single.is_dir { "/" } else { " " };
            let replacement = format!("@{}{suffix}", single.relative_path);
            let new_line = format!(
                "{}{replacement}{}",
                &line[..token_start],
                &line[cursor_byte..]
            );
            let target_col = line[..token_start].chars().count() + replacement.chars().count();
            replace_line_and_move_cursor(editor, row, new_line, target_col);
            CompleteResult::Modified { notice: None }
        }
        many => {
            let paths: Vec<&str> = many.iter().map(|c| c.relative_path.as_str()).collect();
            let lcp = longest_common_prefix(&paths);
            let notice = format_candidate_notice("Paths", many.iter().map(format_path_candidate));
            let normalized_prefix = raw_prefix.trim_start_matches("./");
            if lcp.len() > normalized_prefix.len() {
                let replacement = format!("@{lcp}");
                let new_line = format!(
                    "{}{replacement}{}",
                    &line[..token_start],
                    &line[cursor_byte..]
                );
                let target_col = line[..token_start].chars().count() + replacement.chars().count();
                replace_line_and_move_cursor(editor, row, new_line, target_col);
                CompleteResult::Modified {
                    notice: Some(notice),
                }
            } else {
                CompleteResult::Notice(notice)
            }
        }
    }
}

fn format_path_candidate(candidate: &WorkspacePathCandidate) -> String {
    let escaped = escape_untrusted_text(&candidate.relative_path);
    if candidate.is_dir {
        format!("{escaped}/")
    } else {
        escaped
    }
}

fn format_candidate_notice(label: &str, items: impl IntoIterator<Item = String>) -> String {
    let all: Vec<String> = items.into_iter().collect();
    let shown: Vec<&str> = all
        .iter()
        .take(MAX_NOTICE_CANDIDATES)
        .map(String::as_str)
        .collect();
    if all.len() > MAX_NOTICE_CANDIDATES {
        format!(
            "{label}: {} (+{} more)",
            shown.join(", "),
            all.len() - MAX_NOTICE_CANDIDATES
        )
    } else {
        format!("{label}: {}", shown.join(", "))
    }
}

fn longest_common_prefix(items: &[&str]) -> String {
    let Some((first, rest)) = items.split_first() else {
        return String::new();
    };
    let mut prefix_end = first.len();
    for other in rest {
        let mut matched = 0;
        for (a, b) in first.chars().zip(other.chars()) {
            if a != b {
                break;
            }
            matched += a.len_utf8();
        }
        prefix_end = prefix_end.min(matched);
    }
    first[..prefix_end].to_owned()
}

fn char_col_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map_or(line.len(), |(idx, _)| idx)
}

fn replace_line_and_move_cursor(
    editor: &mut TextArea<'static>,
    row: usize,
    new_line: String,
    target_col: usize,
) {
    let mut lines: Vec<String> = editor.lines().to_vec();
    if row < lines.len() {
        lines[row] = new_line;
    }
    let mut next = super::editor();
    next.insert_str(lines.join("\n"));
    next.move_cursor(CursorMove::Top);
    for _ in 0..row {
        next.move_cursor(CursorMove::Down);
    }
    next.move_cursor(CursorMove::Head);
    for _ in 0..target_col {
        next.move_cursor(CursorMove::Forward);
    }
    *editor = next;
}
