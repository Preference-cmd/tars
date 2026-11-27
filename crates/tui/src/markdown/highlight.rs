//! Pure-Rust, zero-unsafe, single-pass `O(n)` lexical syntax highlighter and
//! unified diff line highlighter.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Maximum code-block byte size eligible for lexical highlighting (`64 KiB`).
///
/// Larger inputs fall back immediately to plain unhighlighted lines in `O(n)`
/// time so oversized blocks never stall the background cache or UI.
pub(crate) const MAX_HIGHLIGHT_BYTES: usize = 64 * 1024;

use crate::render::theme::MUTED;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Shell,
    Json,
    Toml,
    Yaml,
    Sql,
    CFamily,
    Diff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MultiLineQuote {
    Backtick,
    TripleDouble,
    TripleSingle,
    RustRaw(usize),
}

#[derive(Default, Clone, Copy)]
struct LexerState {
    block_comment_depth: usize,
    multiline_quote: Option<MultiLineQuote>,
}

/// Sanitize untrusted text before placing it into Ratatui `Span`s.
pub(super) fn sanitize_text(input: &str) -> String {
    let escaped = tars_core::permission::escape_untrusted_text(input);
    if escaped.contains('\t') {
        escaped.replace('\t', "    ")
    } else {
        escaped
    }
}

/// Sanitize single-line metadata (language tags, URLs, titles) so no `\n` or
/// control/BiDi characters survive.
pub(super) fn sanitize_single_line(input: &str) -> String {
    let sanitized = sanitize_text(input);
    if sanitized.contains('\n') {
        sanitized.replace('\n', " ")
    } else {
        sanitized
    }
}

/// Parse the first whitespace/comma-delimited token of a fence info string.
fn parse_language(lang: &str) -> Option<Language> {
    let token = lang
        .trim()
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match token.as_str() {
        "rust" | "rs" => Some(Language::Rust),
        "typescript" | "ts" | "tsx" | "javascript" | "js" | "jsx" => Some(Language::TypeScript),
        "python" | "py" => Some(Language::Python),
        "go" => Some(Language::Go),
        "sh" | "bash" | "zsh" | "shell" => Some(Language::Shell),
        "json" | "jsonc" => Some(Language::Json),
        "toml" => Some(Language::Toml),
        "yaml" | "yml" => Some(Language::Yaml),
        "sql" => Some(Language::Sql),
        "c" | "cpp" | "c++" | "h" | "hpp" => Some(Language::CFamily),
        "diff" | "patch" => Some(Language::Diff),
        _ => None,
    }
}

/// Split sanitized code into logical lines without dropping internal blank lines
/// and without adding a phantom trailing blank line for a single terminating `\n`.
fn split_code_lines(code: &str) -> Vec<&str> {
    if code.is_empty() {
        return Vec::new();
    }
    let body = code.strip_suffix('\n').unwrap_or(code);
    body.split('\n').collect()
}

/// Highlight a code block using a single-pass `O(n)` lexical scanner.
///
/// Unknown/empty languages and inputs larger than [`MAX_HIGHLIGHT_BYTES`] fall
/// back immediately to plain `Style::default()` lines with zero lines dropped.
pub(crate) fn highlight_code(lang: &str, code: &str) -> Vec<Line<'static>> {
    let sanitized = sanitize_text(code);
    let lines = split_code_lines(&sanitized);
    if code.len() > MAX_HIGHLIGHT_BYTES || sanitized.len() > MAX_HIGHLIGHT_BYTES {
        return lines
            .into_iter()
            .map(|line| Line::raw(line.to_owned()))
            .collect();
    }
    let Some(language) = parse_language(lang) else {
        return lines
            .into_iter()
            .map(|line| Line::raw(line.to_owned()))
            .collect();
    };
    if language == Language::Diff {
        return lines
            .into_iter()
            .map(highlight_diff_line_sanitized)
            .collect();
    }
    let mut state = LexerState::default();
    lines
        .into_iter()
        .map(|line| Line::from(highlight_line(language, line, &mut state)))
        .collect()
}

/// Highlight a single unified diff line (`+` green, `-` red, `@@` cyan, headers muted/bold).
pub(crate) fn highlight_diff_line(line: &str) -> Line<'static> {
    let clean = sanitize_single_line(line);
    highlight_diff_line_sanitized(&clean)
}

fn highlight_diff_line_sanitized(line: &str) -> Line<'static> {
    let style = if line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("+++")
        || line.starts_with("---")
        || line.starts_with("new file mode ")
        || line.starts_with("deleted file mode ")
        || line.starts_with("similarity index ")
        || line.starts_with("rename from ")
        || line.starts_with("rename to ")
    {
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    Line::from(vec![Span::styled(line.to_owned(), style)])
}

fn comment_style() -> Style {
    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)
}

fn string_style() -> Style {
    Style::default().fg(Color::Green)
}

fn keyword_style() -> Style {
    Style::default().fg(Color::Magenta)
}

fn type_style() -> Style {
    Style::default().fg(Color::Cyan)
}

fn number_style() -> Style {
    Style::default().fg(Color::Yellow)
}

fn push_span(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_owned(), style));
}

fn supports_slash_comments(lang: Language) -> bool {
    matches!(
        lang,
        Language::Rust | Language::TypeScript | Language::Go | Language::Json | Language::CFamily
    )
}

fn supports_block_comments(lang: Language) -> bool {
    matches!(
        lang,
        Language::Rust
            | Language::TypeScript
            | Language::Go
            | Language::Json
            | Language::Sql
            | Language::CFamily
    )
}

fn supports_hash_comments(lang: Language) -> bool {
    matches!(
        lang,
        Language::Python | Language::Shell | Language::Toml | Language::Yaml
    )
}

fn highlight_line(lang: Language, line: &str, state: &mut LexerState) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    // TOML section header `[section]` / `[[array]]`
    if lang == Language::Toml && state.block_comment_depth == 0 && state.multiline_quote.is_none() {
        let trimmed_start = line.len() - line.trim_start_matches(' ').len();
        if bytes.get(trimmed_start) == Some(&b'[') {
            push_span(&mut spans, &line[..trimmed_start], Style::default());
            if let Some(comment_pos) = find_toml_comment(&line[trimmed_start..]) {
                let abs = trimmed_start + comment_pos;
                push_span(
                    &mut spans,
                    &line[trimmed_start..abs],
                    type_style().add_modifier(Modifier::BOLD),
                );
                push_span(&mut spans, &line[abs..], comment_style());
            } else {
                push_span(
                    &mut spans,
                    &line[trimmed_start..],
                    type_style().add_modifier(Modifier::BOLD),
                );
            }
            return spans;
        }
    }

    // Track whether we're at the start of a key position for TOML/YAML.
    let mut line_key_eligible = matches!(lang, Language::Toml | Language::Yaml);

    while i < len {
        // 1. Continuation of multi-line block comment
        if state.block_comment_depth > 0 {
            let start = i;
            while i < len {
                if lang == Language::Rust && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*'
                {
                    state.block_comment_depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    state.block_comment_depth -= 1;
                    i += 2;
                    if state.block_comment_depth == 0 {
                        break;
                    }
                } else {
                    i += char_len_at(line, i);
                }
            }
            push_span(&mut spans, &line[start..i], comment_style());
            continue;
        }

        // 2. Continuation of multi-line string / raw string / backtick
        if let Some(mq) = state.multiline_quote {
            let start = i;
            let closed = scan_multiline_quote(lang, line, &mut i, mq);
            if closed {
                state.multiline_quote = None;
            }
            push_span(&mut spans, &line[start..i], string_style());
            continue;
        }

        let b = bytes[i];

        // Whitespace
        if b == b' ' {
            let start = i;
            while i < len && bytes[i] == b' ' {
                i += 1;
            }
            push_span(&mut spans, &line[start..i], Style::default());
            continue;
        }

        // Line / block comments
        if supports_slash_comments(lang) && i + 1 < len && b == b'/' && bytes[i + 1] == b'/' {
            push_span(&mut spans, &line[i..], comment_style());
            break;
        }
        if supports_block_comments(lang) && i + 1 < len && b == b'/' && bytes[i + 1] == b'*' {
            state.block_comment_depth = 1;
            let start = i;
            i += 2;
            while i < len {
                if lang == Language::Rust && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*'
                {
                    state.block_comment_depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    state.block_comment_depth -= 1;
                    i += 2;
                    if state.block_comment_depth == 0 {
                        break;
                    }
                } else {
                    i += char_len_at(line, i);
                }
            }
            push_span(&mut spans, &line[start..i], comment_style());
            continue;
        }
        if supports_hash_comments(lang)
            && b == b'#'
            && (lang != Language::Yaml || i == 0 || bytes[i - 1] == b' ')
        {
            push_span(&mut spans, &line[i..], comment_style());
            break;
        }
        if lang == Language::Sql && i + 1 < len && b == b'-' && bytes[i + 1] == b'-' {
            push_span(&mut spans, &line[i..], comment_style());
            break;
        }

        // C/C++ preprocessor directive (`#include`, `#define`, etc.)
        if lang == Language::CFamily && b == b'#' && line[..i].trim().is_empty() {
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            push_span(&mut spans, &line[start..i], keyword_style());
            continue;
        }

        // YAML list prefix `- ` keeps key eligibility for `- key: val`
        if lang == Language::Yaml
            && line_key_eligible
            && b == b'-'
            && bytes.get(i + 1) == Some(&b' ')
        {
            push_span(&mut spans, "- ", Style::default());
            i += 2;
            continue;
        }

        // YAML unquoted key at key position: `key:` (followed by space or EOL)
        if lang == Language::Yaml
            && line_key_eligible
            && b != b'"'
            && b != b'\''
            && let Some(colon_end) = scan_yaml_key(line, i)
        {
            push_span(&mut spans, &line[i..colon_end], type_style());
            push_span(&mut spans, ":", Style::default());
            i = colon_end + 1;
            line_key_eligible = false;
            continue;
        }

        // TOML unquoted key before `=` (including dotted keys like `a.b =`)
        if lang == Language::Toml
            && line_key_eligible
            && (b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            && let Some(key_end) = scan_toml_key(line, i)
        {
            push_span(&mut spans, &line[i..key_end], type_style());
            i = key_end;
            line_key_eligible = false;
            continue;
        }

        // Once we encounter non-whitespace/non-key token on TOML/YAML line, disable key-only shortcut
        if matches!(lang, Language::Toml | Language::Yaml) && b != b'.' && b != b'=' && b != b':' {
            line_key_eligible = false;
        }

        // Rust raw strings: r"...", r#"..."#, br#"..."#, cr#"..."#
        if lang == Language::Rust
            && let Some((prefix_len, hashes)) = detect_rust_raw_string(bytes, i)
        {
            let start = i;
            i += prefix_len;
            let mq = MultiLineQuote::RustRaw(hashes);
            if !scan_multiline_quote(lang, line, &mut i, mq) {
                state.multiline_quote = Some(mq);
            }
            push_span(&mut spans, &line[start..i], string_style());
            continue;
        }

        // Python / TOML triple-quoted strings `"""..."""` or `'''...'''`
        if matches!(lang, Language::Python | Language::Toml)
            && i + 2 < len
            && ((b == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"')
                || (b == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\''))
        {
            let start = i;
            let mq = if b == b'"' {
                MultiLineQuote::TripleDouble
            } else {
                MultiLineQuote::TripleSingle
            };
            i += 3;
            if !scan_multiline_quote(lang, line, &mut i, mq) {
                state.multiline_quote = Some(mq);
            }
            push_span(&mut spans, &line[start..i], string_style());
            continue;
        }

        // Backtick strings (Go raw strings & JS/TS template literals)
        if matches!(lang, Language::Go | Language::TypeScript) && b == b'`' {
            let start = i;
            i += 1;
            if !scan_multiline_quote(lang, line, &mut i, MultiLineQuote::Backtick) {
                state.multiline_quote = Some(MultiLineQuote::Backtick);
            }
            push_span(&mut spans, &line[start..i], string_style());
            continue;
        }

        // Double-quoted strings
        if b == b'"' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 1 + char_len_at(line, i + 1);
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += char_len_at(line, i);
                }
            }
            let style = if is_quoted_key(lang, line, i) {
                type_style()
            } else {
                string_style()
            };
            push_span(&mut spans, &line[start..i], style);
            continue;
        }

        // Single-quoted strings / Rust char literals & lifetimes
        if b == b'\'' {
            if lang == Language::Rust {
                if let Some(end) = scan_rust_char_literal(line, i) {
                    push_span(&mut spans, &line[i..end], string_style());
                    i = end;
                } else if let Some(end) = scan_rust_lifetime(line, i) {
                    push_span(&mut spans, &line[i..end], type_style());
                    i = end;
                } else {
                    push_span(&mut spans, "'", Style::default());
                    i += 1;
                }
                continue;
            }
            if matches!(
                lang,
                Language::TypeScript
                    | Language::Python
                    | Language::Shell
                    | Language::Toml
                    | Language::Yaml
                    | Language::Sql
                    | Language::CFamily
                    | Language::Go
            ) {
                let start = i;
                i += 1;
                let supports_backslash =
                    !matches!(lang, Language::Toml | Language::Sql | Language::Shell);
                while i < len {
                    if supports_backslash && bytes[i] == b'\\' && i + 1 < len {
                        i += 1 + char_len_at(line, i + 1);
                    } else if lang == Language::Sql
                        && bytes[i] == b'\''
                        && bytes.get(i + 1) == Some(&b'\'')
                    {
                        i += 2;
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        break;
                    } else {
                        i += char_len_at(line, i);
                    }
                }
                let style = if is_quoted_key(lang, line, i) {
                    type_style()
                } else {
                    string_style()
                };
                push_span(&mut spans, &line[start..i], style);
                continue;
            }
        }

        // Shell variables `$VAR`, `${VAR}`
        if lang == Language::Shell && b == b'$' {
            let start = i;
            i += 1;
            if bytes.get(i) == Some(&b'{') {
                i += 1;
                while i < len && bytes[i] != b'}' {
                    i += char_len_at(line, i);
                }
                if i < len && bytes[i] == b'}' {
                    i += 1;
                }
            } else {
                while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
            }
            let style = if i > start + 1 {
                type_style()
            } else {
                Style::default()
            };
            push_span(&mut spans, &line[start..i], style);
            continue;
        }

        // Numbers
        if b.is_ascii_digit() {
            let start = i;
            i = scan_number(bytes, i);
            push_span(&mut spans, &line[start..i], number_style());
            continue;
        }

        // Identifiers / keywords / types / macros
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if lang == Language::Rust && bytes.get(i) == Some(&b'!') {
                i += 1;
                push_span(&mut spans, &line[start..i], type_style());
            } else if let Some(style) = classify_word(lang, word) {
                push_span(&mut spans, word, style);
            } else {
                push_span(&mut spans, word, Style::default());
            }
            continue;
        }

        // Any other character (punctuation, operators, UTF-8 text)
        let ch_len = char_len_at(line, i);
        push_span(&mut spans, &line[i..i + ch_len], Style::default());
        i += ch_len;
    }

    spans
}

fn char_len_at(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map_or(1, char::len_utf8)
}

fn find_toml_comment(slice: &str) -> Option<usize> {
    let bytes = slice.as_bytes();
    let mut in_bracket = true;
    for (idx, &b) in bytes.iter().enumerate() {
        if b == b']' {
            in_bracket = false;
        } else if !in_bracket && b == b'#' {
            return Some(idx);
        }
    }
    None
}

fn scan_yaml_key(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut j = start;
    if !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        return None;
    }
    while j < bytes.len() {
        let b = bytes[j];
        if b == b':' {
            if j > start && (j + 1 == bytes.len() || bytes[j + 1] == b' ') {
                return Some(j);
            }
            return None;
        }
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/') {
            j += 1;
        } else {
            return None;
        }
    }
    None
}

fn scan_toml_key(line: &str, start: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut j = start;
    while j < bytes.len() {
        let b = bytes[j];
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') {
            j += 1;
        } else {
            break;
        }
    }
    if j == start {
        return None;
    }
    let mut k = j;
    while k < bytes.len() && bytes[k] == b' ' {
        k += 1;
    }
    (bytes.get(k) == Some(&b'=')).then_some(j)
}

fn is_quoted_key(lang: Language, line: &str, after_quote: usize) -> bool {
    let rest = line[after_quote..].trim_start_matches(' ');
    match lang {
        Language::Json => rest.starts_with(':'),
        Language::Yaml => rest.starts_with(':') && (rest.len() == 1 || rest.as_bytes()[1] == b' '),
        Language::Toml => rest.starts_with('=') || rest.starts_with('.'),
        _ => false,
    }
}

fn detect_rust_raw_string(bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let prefix = if bytes.get(i) == Some(&b'r') {
        1
    } else if matches!(bytes.get(i), Some(b'b' | b'c')) && bytes.get(i + 1) == Some(&b'r') {
        2
    } else {
        return None;
    };
    let mut j = i + prefix;
    let mut hashes = 0usize;
    while j < bytes.len() && bytes[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if bytes.get(j) == Some(&b'"') {
        Some(((j + 1) - i, hashes))
    } else {
        None
    }
}

fn scan_multiline_quote(lang: Language, line: &str, i: &mut usize, mq: MultiLineQuote) -> bool {
    let bytes = line.as_bytes();
    let len = bytes.len();
    match mq {
        MultiLineQuote::Backtick => {
            while *i < len {
                if lang == Language::TypeScript && bytes[*i] == b'\\' && *i + 1 < len {
                    *i += 1 + char_len_at(line, *i + 1);
                } else if bytes[*i] == b'`' {
                    *i += 1;
                    return true;
                } else {
                    *i += char_len_at(line, *i);
                }
            }
            false
        }
        MultiLineQuote::TripleDouble => {
            while *i < len {
                if lang == Language::Python && bytes[*i] == b'\\' && *i + 1 < len {
                    *i += 1 + char_len_at(line, *i + 1);
                } else if *i + 2 < len
                    && bytes[*i] == b'"'
                    && bytes[*i + 1] == b'"'
                    && bytes[*i + 2] == b'"'
                {
                    *i += 3;
                    return true;
                } else {
                    *i += char_len_at(line, *i);
                }
            }
            false
        }
        MultiLineQuote::TripleSingle => {
            while *i < len {
                if lang == Language::Python && bytes[*i] == b'\\' && *i + 1 < len {
                    *i += 1 + char_len_at(line, *i + 1);
                } else if *i + 2 < len
                    && bytes[*i] == b'\''
                    && bytes[*i + 1] == b'\''
                    && bytes[*i + 2] == b'\''
                {
                    *i += 3;
                    return true;
                } else {
                    *i += char_len_at(line, *i);
                }
            }
            false
        }
        MultiLineQuote::RustRaw(hashes) => {
            while *i < len {
                if bytes[*i] == b'"' {
                    let mut h = 0usize;
                    while h < hashes && *i + 1 + h < len && bytes[*i + 1 + h] == b'#' {
                        h += 1;
                    }
                    if h == hashes {
                        *i += 1 + hashes;
                        return true;
                    }
                }
                *i += char_len_at(line, *i);
            }
            false
        }
    }
}

fn scan_rust_char_literal(line: &str, i: usize) -> Option<usize> {
    let rest = &line[i + 1..];
    let bytes = rest.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    if bytes[0] == b'\\' {
        // Escaped char literal like '\n', '\'', '\x7f', '\u{1f600}'
        let mut j = 1;
        if j < bytes.len() {
            j += char_len_at(rest, j);
        }
        let limit = bytes.len().min(18);
        while j < limit {
            if bytes[j] == b'\'' {
                return Some(i + 1 + j + 1);
            }
            if bytes[j].is_ascii_whitespace() {
                return None;
            }
            j += char_len_at(rest, j);
        }
        return None;
    }
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if ch != '\'' && ch != '\\' && chars.next() == Some('\'') {
        Some(i + 1 + ch.len_utf8() + 1)
    } else {
        None
    }
}

fn scan_rust_lifetime(line: &str, i: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let next = *bytes.get(i + 1)?;
    if !(next.is_ascii_alphabetic() || next == b'_') {
        return None;
    }
    let mut j = i + 2;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    Some(j)
}

fn scan_number(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let len = bytes.len();
    if i + 1 < len
        && bytes[i] == b'0'
        && matches!(bytes[i + 1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O')
    {
        i += 2;
        while i < len && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_') {
            i += 1;
        }
    } else {
        while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
        if i + 1 < len && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
            i += 1;
            while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
        }
        if i < len && matches!(bytes[i], b'e' | b'E') {
            let mut k = i + 1;
            if k < len && matches!(bytes[k], b'+' | b'-') {
                k += 1;
            }
            if k < len && bytes[k].is_ascii_digit() {
                i = k;
                while i < len && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
            }
        }
    }
    // Optional numeric suffix (e.g. u64, f32, usize, L, u)
    while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    i
}

fn classify_word(lang: Language, word: &str) -> Option<Style> {
    if is_literal_word(lang, word) {
        return Some(number_style());
    }
    if is_keyword(lang, word) {
        return Some(keyword_style());
    }
    if is_builtin_type(lang, word) {
        return Some(type_style());
    }
    if lang != Language::Sql && word.as_bytes().first().is_some_and(u8::is_ascii_uppercase) {
        return Some(type_style());
    }
    None
}

fn is_literal_word(lang: Language, word: &str) -> bool {
    match lang {
        Language::Rust | Language::Shell | Language::Toml => matches!(word, "true" | "false"),
        Language::TypeScript => {
            matches!(
                word,
                "true" | "false" | "null" | "undefined" | "NaN" | "Infinity"
            )
        }
        Language::Python => matches!(word, "True" | "False" | "None"),
        Language::Go => matches!(word, "true" | "false" | "nil" | "iota"),
        Language::Json => matches!(word, "true" | "false" | "null"),
        Language::Yaml => matches!(word, "true" | "false" | "null" | "True" | "False" | "Null"),
        Language::Sql => {
            word.eq_ignore_ascii_case("true")
                || word.eq_ignore_ascii_case("false")
                || word.eq_ignore_ascii_case("null")
        }
        Language::CFamily => matches!(word, "true" | "false" | "NULL" | "nullptr"),
        Language::Diff => false,
    }
}

fn is_keyword(lang: Language, word: &str) -> bool {
    match lang {
        Language::Rust => RUST_KEYWORDS.contains(&word),
        Language::TypeScript => TS_KEYWORDS.contains(&word),
        Language::Python => PYTHON_KEYWORDS.contains(&word),
        Language::Go => GO_KEYWORDS.contains(&word),
        Language::Shell => SHELL_KEYWORDS.contains(&word),
        Language::CFamily => C_KEYWORDS.contains(&word),
        Language::Sql => SQL_KEYWORDS.iter().any(|kw| word.eq_ignore_ascii_case(kw)),
        Language::Json | Language::Toml | Language::Yaml | Language::Diff => false,
    }
}

fn is_builtin_type(lang: Language, word: &str) -> bool {
    match lang {
        Language::Rust => matches!(
            word,
            "u8" | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "f32"
                | "f64"
                | "bool"
                | "char"
                | "str"
        ),
        Language::TypeScript => matches!(
            word,
            "string"
                | "number"
                | "boolean"
                | "any"
                | "unknown"
                | "never"
                | "object"
                | "symbol"
                | "bigint"
        ),
        Language::Python => matches!(
            word,
            "int"
                | "str"
                | "float"
                | "bool"
                | "list"
                | "dict"
                | "set"
                | "tuple"
                | "bytes"
                | "object"
        ),
        Language::Go => matches!(
            word,
            "bool"
                | "byte"
                | "complex64"
                | "complex128"
                | "error"
                | "float32"
                | "float64"
                | "int"
                | "int8"
                | "int16"
                | "int32"
                | "int64"
                | "rune"
                | "string"
                | "uint"
                | "uint8"
                | "uint16"
                | "uint32"
                | "uint64"
                | "uintptr"
        ),
        Language::CFamily => matches!(
            word,
            "size_t"
                | "ssize_t"
                | "uint8_t"
                | "uint16_t"
                | "uint32_t"
                | "uint64_t"
                | "int8_t"
                | "int16_t"
                | "int32_t"
                | "int64_t"
                | "std"
                | "string"
                | "vector"
        ),
        Language::Sql => SQL_TYPES.iter().any(|ty| word.eq_ignore_ascii_case(ty)),
        Language::Shell | Language::Json | Language::Toml | Language::Yaml | Language::Diff => {
            false
        }
    }
}

const RUST_KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "const",
    "continue",
    "crate",
    "dyn",
    "else",
    "enum",
    "extern",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "trait",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "yield",
    "try",
    "macro_rules",
];

const TS_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "finally",
    "for",
    "from",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "namespace",
    "new",
    "of",
    "package",
    "private",
    "protected",
    "public",
    "readonly",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "type",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda",
    "match", "case", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while", "with",
    "yield",
];

const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "coproc", "local", "export", "readonly", "declare",
    "typeset", "unset", "return", "exit", "break", "continue", "source", "eval", "exec", "set",
    "shift",
];

const C_KEYWORDS: &[&str] = &[
    "alignas",
    "alignof",
    "and",
    "asm",
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "char8_t",
    "char16_t",
    "char32_t",
    "class",
    "co_await",
    "co_return",
    "co_yield",
    "concept",
    "const",
    "consteval",
    "constexpr",
    "constinit",
    "const_cast",
    "continue",
    "decltype",
    "default",
    "delete",
    "do",
    "double",
    "dynamic_cast",
    "else",
    "enum",
    "explicit",
    "export",
    "extern",
    "float",
    "for",
    "friend",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "operator",
    "or",
    "private",
    "protected",
    "public",
    "register",
    "reinterpret_cast",
    "requires",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "static_assert",
    "static_cast",
    "struct",
    "switch",
    "template",
    "this",
    "thread_local",
    "throw",
    "try",
    "typedef",
    "typeid",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "wchar_t",
    "while",
];

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "ALTER",
    "DROP",
    "TABLE",
    "INDEX",
    "VIEW",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "ON",
    "AS",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "ASC",
    "DESC",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "CONSTRAINT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "TRANSACTION",
    "WITH",
    "RECURSIVE",
    "RETURNING",
    "EXPLAIN",
    "ANALYZE",
    "PRAGMA",
    "VACUUM",
    "IF",
];

const SQL_TYPES: &[&str] = &[
    "INT",
    "INTEGER",
    "BIGINT",
    "SMALLINT",
    "TEXT",
    "VARCHAR",
    "CHAR",
    "BOOLEAN",
    "BOOL",
    "REAL",
    "FLOAT",
    "DOUBLE",
    "DECIMAL",
    "NUMERIC",
    "BLOB",
    "DATE",
    "TIME",
    "TIMESTAMP",
    "JSON",
    "JSONB",
    "UUID",
];
