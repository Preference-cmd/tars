use std::path::{Path, PathBuf};
use tars_core::permission::escape_untrusted_text;

const MAX_DIR_SCAN_ENTRIES: usize = 1024;
const MAX_PATH_CANDIDATES: usize = 64;
const MAX_NOTICE_CANDIDATES: usize = 6;
const IMAGE_SUBCOMMANDS: [&str; 2] = ["clear", "remove"];

/// Result of completing the arguments after `/image `.
pub(crate) enum ImageArgCompletion {
    None,
    Modified {
        replacement: String,
        notice: Option<String>,
    },
    Notice(String),
}

/// Format a byte length in compact binary units (`B`, `KiB`, `MiB`).
pub(crate) fn format_human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        let kib = bytes / 1024;
        let rem = (bytes % 1024) * 10 / 1024;
        if rem == 0 {
            format!("{kib} KiB")
        } else {
            format!("{kib}.{rem} KiB")
        }
    } else {
        let mib = bytes / (1024 * 1024);
        let rem = (bytes % (1024 * 1024)) * 10 / (1024 * 1024);
        if rem == 0 {
            format!("{mib} MiB")
        } else {
            format!("{mib}.{rem} MiB")
        }
    }
}

/// Strip a leading `/image` (or `image` in the command palette) followed by whitespace.
pub(crate) fn strip_image_command_prefix(input: &str) -> Option<&str> {
    let rest = input
        .strip_prefix("/image")
        .or_else(|| input.strip_prefix("image"))?;
    let first = rest.chars().next()?;
    if !first.is_whitespace() {
        return None;
    }
    Some(&rest[first.len_utf8()..])
}

/// Tokenize shell-style path lists (drag-and-drop or pasted paths).
///
/// Supports single quotes (`'...'`), double quotes (`"..."` with `\"` / `\\`),
/// backslash-escaped characters outside quotes (e.g. `\ `), and arbitrary
/// whitespace/newline separators. Returns `None` on empty input, unclosed quotes,
/// or a trailing dangling backslash so callers can fall back losslessly.
pub(crate) fn parse_pasted_path_tokens(raw: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut chars = raw.chars().peekable();

    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }

        let mut token = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_whitespace() {
                break;
            }
            match ch {
                '\'' => {
                    chars.next();
                    let mut closed = false;
                    for inner in chars.by_ref() {
                        if inner == '\'' {
                            closed = true;
                            break;
                        }
                        token.push(inner);
                    }
                    if !closed {
                        return None;
                    }
                }
                '"' => {
                    chars.next();
                    let mut closed = false;
                    while let Some(inner) = chars.next() {
                        if inner == '"' {
                            closed = true;
                            break;
                        }
                        if inner == '\\' {
                            match chars.peek().copied() {
                                Some(esc @ ('"' | '\\' | '$' | '`')) => {
                                    chars.next();
                                    token.push(esc);
                                }
                                Some('\n') => {
                                    chars.next();
                                }
                                Some(_) => token.push('\\'),
                                None => return None,
                            }
                        } else {
                            token.push(inner);
                        }
                    }
                    if !closed {
                        return None;
                    }
                }
                '\\' => {
                    chars.next();
                    let next = chars.next()?;
                    token.push(next);
                }
                other => {
                    chars.next();
                    token.push(other);
                }
            }
        }
        if token.is_empty() {
            return None;
        }
        tokens.push(token);
    }

    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

/// Parse a single path argument for `/image <path>`, accepting quotes and escapes.
pub(crate) fn parse_single_image_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match parse_pasted_path_tokens(trimmed) {
        Some(mut tokens) if tokens.len() == 1 => Some(tokens.remove(0)),
        Some(tokens) if tokens.len() > 1 => Some(trimmed.to_owned()),
        _ => None,
    }
}

/// Resolve a parsed path token (`file://`, `~`, `~/...`, absolute, or relative)
/// against `base_dir` (usually the session workspace root) or the current directory.
pub(crate) fn resolve_local_path(token: &str, base_dir: Option<&Path>) -> Option<PathBuf> {
    if token.is_empty() || token.contains('\0') {
        return None;
    }

    let expanded = if let Some(rest) = token.strip_prefix("file://") {
        decode_file_url_path(rest)?
    } else if token.contains("://") {
        // Reject non-file URLs such as https:// or http://.
        return None;
    } else if token == "~" {
        home_dir()?
    } else if let Some(rest) = token.strip_prefix("~/") {
        home_dir()?.join(rest)
    } else {
        PathBuf::from(token)
    };

    if expanded.is_absolute() {
        Some(expanded)
    } else if let Some(base) = base_dir {
        Some(base.join(expanded))
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(expanded))
    }
}

fn decode_file_url_path(after_scheme: &str) -> Option<PathBuf> {
    let raw_path = if after_scheme.starts_with('/') {
        after_scheme
    } else if let Some(rest) = after_scheme.strip_prefix("localhost/") {
        // Keep the leading `/` when stripping `localhost`.
        &after_scheme["localhost".len()..("localhost".len() + 1 + rest.len())]
    } else {
        return None;
    };
    let decoded = percent_decode(raw_path)?;
    if decoded.contains('\0') {
        return None;
    }
    Some(PathBuf::from(decoded))
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_val(bytes[i + 1])?;
            let lo = hex_val(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// Escape spaces and shell-special characters with backslashes for completed paths.
pub(crate) fn escape_shell_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for ch in path.chars() {
        match ch {
            ' ' | '\\' | '\'' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

/// Complete `/image` subcommands (`remove`, `clear`) or filesystem paths.
pub(crate) fn complete_image_arg(arg_prefix: &str, base_dir: Option<&Path>) -> ImageArgCompletion {
    let lower = arg_prefix.to_lowercase();
    if lower == "remove" {
        return ImageArgCompletion::Modified {
            replacement: "remove ".into(),
            notice: None,
        };
    }
    if lower.starts_with("remove ") || lower.starts_with("remove\t") {
        return ImageArgCompletion::Notice("Usage: /image remove <n> (1-based index)".into());
    }
    if lower == "clear" || lower.starts_with("clear ") || lower.starts_with("clear\t") {
        return ImageArgCompletion::None;
    }

    let allow_subcommands = !arg_prefix.contains('/')
        && !arg_prefix.contains('\\')
        && !arg_prefix.starts_with('.')
        && !arg_prefix.starts_with('~')
        && !arg_prefix.starts_with('\'')
        && !arg_prefix.starts_with('"');

    let sub_matches: Vec<&str> = if allow_subcommands {
        IMAGE_SUBCOMMANDS
            .iter()
            .copied()
            .filter(|cmd| cmd.starts_with(&lower))
            .collect()
    } else {
        Vec::new()
    };

    let (quote, unescaped_partial) = match parse_partial_path(arg_prefix) {
        Some(parsed) => parsed,
        None => return ImageArgCompletion::None,
    };

    if quote.is_none() && unescaped_partial == "~" {
        return ImageArgCompletion::Modified {
            replacement: "~/".into(),
            notice: None,
        };
    }

    let path_candidates = list_path_candidates(&unescaped_partial, base_dir);

    if allow_subcommands && !sub_matches.is_empty() && path_candidates.is_empty() {
        return match sub_matches.as_slice() {
            ["remove"] => ImageArgCompletion::Modified {
                replacement: "remove ".into(),
                notice: None,
            },
            ["clear"] => ImageArgCompletion::Modified {
                replacement: "clear".into(),
                notice: None,
            },
            many => ImageArgCompletion::Notice(format_candidate_notice(
                "Subcommands",
                many.iter().copied().map(str::to_owned),
            )),
        };
    }

    if arg_prefix.is_empty() {
        let mut items: Vec<String> = IMAGE_SUBCOMMANDS
            .iter()
            .map(|cmd| (*cmd).to_owned())
            .collect();
        items.extend(
            path_candidates
                .iter()
                .map(|c| escape_untrusted_text(&c.display_name)),
        );
        return ImageArgCompletion::Notice(format_candidate_notice("Image", items));
    }

    match path_candidates.as_slice() {
        [] => {
            if sub_matches.is_empty() {
                ImageArgCompletion::Notice(format!(
                    "No matching image path '{}'",
                    escape_untrusted_text(arg_prefix)
                ))
            } else {
                ImageArgCompletion::Notice(format_candidate_notice(
                    "Subcommands",
                    sub_matches.into_iter().map(str::to_owned),
                ))
            }
        }
        [single] if sub_matches.is_empty() => {
            let replacement = format_completed_path(&single.full_path, single.is_dir, quote);
            ImageArgCompletion::Modified {
                replacement,
                notice: None,
            }
        }
        many => {
            let mut all_labels: Vec<String> = sub_matches.iter().map(|s| (*s).to_owned()).collect();
            all_labels.extend(many.iter().map(|c| escape_untrusted_text(&c.display_name)));
            let notice = format_candidate_notice("Paths", all_labels);
            if sub_matches.is_empty() {
                let raw_paths: Vec<&str> = many.iter().map(|c| c.full_path.as_str()).collect();
                let lcp = longest_common_prefix(&raw_paths);
                if lcp.len() > unescaped_partial.len() {
                    let replacement = format_completed_path(&lcp, true, quote);
                    return ImageArgCompletion::Modified {
                        replacement,
                        notice: Some(notice),
                    };
                }
            }
            ImageArgCompletion::Notice(notice)
        }
    }
}

struct PathCandidate {
    full_path: String,
    display_name: String,
    is_dir: bool,
}

fn parse_partial_path(raw: &str) -> Option<(Option<char>, String)> {
    let mut chars = raw.chars().peekable();
    let quote = match chars.peek().copied() {
        Some(q @ ('\'' | '"')) => {
            chars.next();
            Some(q)
        }
        _ => None,
    };
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        match quote {
            Some('\'') => {
                if ch == '\'' {
                    return None;
                }
                out.push(ch);
            }
            Some('"') => {
                if ch == '"' {
                    return None;
                }
                if ch == '\\' {
                    match chars.peek().copied() {
                        Some(esc @ ('"' | '\\')) => {
                            chars.next();
                            out.push(esc);
                        }
                        Some(_) => out.push('\\'),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(ch);
                }
            }
            _ => {
                if ch == '\\' {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                } else {
                    out.push(ch);
                }
            }
        }
    }
    if out.contains('\0') {
        return None;
    }
    Some((quote, out))
}

fn format_completed_path(raw_path: &str, is_dir: bool, quote: Option<char>) -> String {
    match quote {
        Some(q) => {
            let escaped = if q == '"' {
                raw_path.replace('\\', "\\\\").replace('"', "\\\"")
            } else {
                raw_path.to_owned()
            };
            if is_dir {
                format!("{q}{escaped}")
            } else {
                format!("{q}{escaped}{q}")
            }
        }
        None => escape_shell_path(raw_path),
    }
}

fn list_path_candidates(partial: &str, base_dir: Option<&Path>) -> Vec<PathCandidate> {
    let (dir_display, file_prefix) = match partial.rfind('/') {
        Some(pos) => (&partial[..=pos], &partial[pos + 1..]),
        None => ("", partial),
    };

    let search_dir = if dir_display.is_empty() {
        match base_dir {
            Some(dir) => dir.to_path_buf(),
            None => match std::env::current_dir() {
                Ok(cwd) => cwd,
                Err(_) => return Vec::new(),
            },
        }
    } else if dir_display == "~/" {
        match home_dir() {
            Some(home) => home,
            None => return Vec::new(),
        }
    } else if let Some(rest) = dir_display.strip_prefix("~/") {
        match home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        }
    } else {
        let dir_path = Path::new(dir_display);
        if dir_path.is_absolute() {
            dir_path.to_path_buf()
        } else if let Some(base) = base_dir {
            base.join(dir_path)
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(dir_path),
                Err(_) => return Vec::new(),
            }
        }
    };

    let Ok(entries) = std::fs::read_dir(&search_dir) else {
        return Vec::new();
    };

    let show_hidden = file_prefix.starts_with('.');
    let mut exact = Vec::new();
    let mut case_insensitive = Vec::new();
    let lower_prefix = file_prefix.to_lowercase();

    for entry in entries.take(MAX_DIR_SCAN_ENTRIES).flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map(|ft| ft.is_dir() || (ft.is_symlink() && entry.path().is_dir()))
            .unwrap_or(false);
        let display_name = if is_dir {
            format!("{name}/")
        } else {
            name.clone()
        };
        let full_path = format!("{dir_display}{display_name}");
        if name.starts_with(file_prefix) {
            exact.push(PathCandidate {
                full_path,
                display_name,
                is_dir,
            });
        } else if exact.is_empty() && name.to_lowercase().starts_with(&lower_prefix) {
            case_insensitive.push(PathCandidate {
                full_path,
                display_name,
                is_dir,
            });
        }
    }

    let mut out = if exact.is_empty() {
        case_insensitive
    } else {
        exact
    };
    out.sort_by(|a, b| a.full_path.cmp(&b.full_path));
    out.truncate(MAX_PATH_CANDIDATES);
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_tokenizer_handles_quotes_escapes_and_fallbacks() {
        assert_eq!(
            parse_pasted_path_tokens("'/tmp/my shot.png' \"/tmp/two\\n.jpg\""),
            Some(vec!["/tmp/my shot.png".into(), "/tmp/two\\n.jpg".into()])
        );
        assert_eq!(
            parse_pasted_path_tokens("/tmp/a\\ b.png  /tmp/c.webp\n"),
            Some(vec!["/tmp/a b.png".into(), "/tmp/c.webp".into()])
        );
        assert_eq!(parse_pasted_path_tokens("'/tmp/unclosed.png"), None);
        assert_eq!(parse_pasted_path_tokens("\"/tmp/unclosed.png"), None);
        assert_eq!(parse_pasted_path_tokens("/tmp/dangling\\"), None);
        assert_eq!(parse_pasted_path_tokens("   \n  "), None);
    }

    #[test]
    fn resolve_local_path_handles_tilde_file_urls_and_rejects_remote_urls() {
        if let Some(home) = home_dir() {
            assert_eq!(resolve_local_path("~", None), Some(home.clone()));
            assert_eq!(
                resolve_local_path("~/pics/a.png", None),
                Some(home.join("pics/a.png"))
            );
        }
        assert_eq!(
            resolve_local_path("file:///tmp/my%20shot.png", None),
            Some(PathBuf::from("/tmp/my shot.png"))
        );
        assert_eq!(
            resolve_local_path("file://localhost/tmp/a.png", None),
            Some(PathBuf::from("/tmp/a.png"))
        );
        assert_eq!(
            resolve_local_path("file://remotehost/tmp/a.png", None),
            None
        );
        assert_eq!(resolve_local_path("https://example.com/a.png", None), None);
        assert_eq!(resolve_local_path("/tmp/\0bad.png", None), None);
    }

    #[test]
    fn human_size_formats_compactly() {
        assert_eq!(format_human_size(0), "0 B");
        assert_eq!(format_human_size(512), "512 B");
        assert_eq!(format_human_size(42 * 1024), "42 KiB");
        assert_eq!(format_human_size(1536), "1.5 KiB");
        assert_eq!(format_human_size(5 * 1024 * 1024), "5 MiB");
    }
}
