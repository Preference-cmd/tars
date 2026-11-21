//! Conservative shell command parser and prefix suggestion helper.

use super::file::is_forbidden_single_word_allow;

/// Parse a shell command string into a non-empty list of simple commands, each
/// represented as a non-empty list of unquoted/dequoted words.
///
/// This parser intentionally recognizes only a conservative subset of POSIX shell
/// syntax and returns `None` for any construct outside that subset so that
/// command rules never misclassify complex or ambiguous shell invocations.
pub fn parse_command(command: &str) -> Option<Vec<Vec<String>>> {
    if command.contains('\0') {
        return None;
    }

    let bytes = command.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;

    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut current_words: Vec<String> = Vec::new();
    let mut segment_has_redirection = false;
    let mut pending_connector = false;

    loop {
        while i < len && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= len {
            break;
        }

        if bytes[i] == b'\n' {
            if !current_words.is_empty() {
                segments.push(std::mem::take(&mut current_words));
                segment_has_redirection = false;
                pending_connector = false;
            } else if segment_has_redirection || pending_connector {
                return None;
            }
            i += 1;
            continue;
        }

        if bytes[i] == b';' {
            if current_words.is_empty() {
                return None;
            }
            segments.push(std::mem::take(&mut current_words));
            segment_has_redirection = false;
            pending_connector = true;
            i += 1;
            continue;
        }

        if bytes[i] == b'|' {
            if current_words.is_empty() {
                return None;
            }
            segments.push(std::mem::take(&mut current_words));
            segment_has_redirection = false;
            pending_connector = true;
            if i + 1 < len && bytes[i + 1] == b'|' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'&' {
            if i + 1 < len && bytes[i + 1] == b'&' {
                if current_words.is_empty() {
                    return None;
                }
                segments.push(std::mem::take(&mut current_words));
                segment_has_redirection = false;
                pending_connector = true;
                i += 2;
                continue;
            }
            return None;
        }

        if let Some(redir_len) = match_fixed_redirection(&command[i..]) {
            segment_has_redirection = true;
            i += redir_len;
            continue;
        }

        let mut word = String::new();
        let mut quoted_before_or_at_eq = false;
        let mut has_unquoted_eq = false;
        let mut at_word_start = true;

        while i < len {
            let b = bytes[i];
            match b {
                b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' => break,
                b'\'' => {
                    at_word_start = false;
                    if !has_unquoted_eq {
                        quoted_before_or_at_eq = true;
                    }
                    i += 1;
                    let start = i;
                    while i < len && bytes[i] != b'\'' {
                        i += 1;
                    }
                    if i >= len {
                        return None;
                    }
                    word.push_str(&command[start..i]);
                    i += 1;
                }
                b'"' => {
                    at_word_start = false;
                    if !has_unquoted_eq {
                        quoted_before_or_at_eq = true;
                    }
                    i += 1;
                    let start = i;
                    while i < len && bytes[i] != b'"' {
                        if matches!(bytes[i], b'$' | b'`' | b'\\') {
                            return None;
                        }
                        i += 1;
                    }
                    if i >= len {
                        return None;
                    }
                    word.push_str(&command[start..i]);
                    i += 1;
                }
                b'$' | b'`' | b'\\' | b'<' | b'>' | b'(' | b')' | b'{' | b'}' | b'*' | b'?'
                | b'[' | b']' | b'#' => {
                    return None;
                }
                b'~' if at_word_start => {
                    return None;
                }
                b'=' => {
                    at_word_start = false;
                    if !quoted_before_or_at_eq {
                        has_unquoted_eq = true;
                    }
                    word.push('=');
                    i += 1;
                }
                _ => {
                    at_word_start = false;
                    let ch = command[i..].chars().next()?;
                    word.push(ch);
                    i += ch.len_utf8();
                }
            }
        }

        if current_words.is_empty()
            && !quoted_before_or_at_eq
            && let Some((name, _)) = word.split_once('=')
            && is_shell_identifier(name)
        {
            return None;
        }

        current_words.push(word);
        pending_connector = false;
    }

    if !current_words.is_empty() {
        segments.push(current_words);
    } else if segment_has_redirection || pending_connector {
        return None;
    }

    if segments.is_empty() {
        return None;
    }

    Some(segments)
}

/// Suggest candidate `allow` rule prefixes for each simple command in `command`,
/// deduplicated in order of first appearance.
///
/// Returns `None` if `command` cannot be parsed conservatively, if any segment
/// starts with an interpreter or wrapper command from
/// [`FORBIDDEN_SINGLE_WORD_ALLOW`](super::FORBIDDEN_SINGLE_WORD_ALLOW) and has
/// only one word, or if any resulting prefix word would be empty.
pub fn suggest_command_prefixes(command: &str) -> Option<Vec<Vec<String>>> {
    let segments = parse_command(command)?;
    let mut prefixes = Vec::new();
    for segment in segments {
        let first = segment.first()?;
        if first.is_empty() {
            return None;
        }
        let suggested = if is_forbidden_single_word_allow(first) {
            if segment.len() < 2 || segment.iter().any(String::is_empty) {
                return None;
            }
            segment
        } else if let Some(second) = segment.get(1)
            && !second.is_empty()
            && !second.starts_with('-')
        {
            vec![first.clone(), second.clone()]
        } else {
            vec![first.clone()]
        };

        if !prefixes.contains(&suggested) {
            prefixes.push(suggested);
        }
    }
    Some(prefixes)
}

fn match_fixed_redirection(rest: &str) -> Option<usize> {
    for token in ["2>&1", "2>/dev/null", ">/dev/null"] {
        if let Some(after) = rest.strip_prefix(token) {
            let boundary = after
                .bytes()
                .next()
                .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b';' | b'&' | b'|'));
            if boundary {
                return Some(token.len());
            }
        }
    }
    None
}

fn is_shell_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
