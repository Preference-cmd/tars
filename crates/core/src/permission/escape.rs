//! Escaping of untrusted text for safe terminal display.

use std::fmt::Write;

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn is_line_or_paragraph_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// Escapes untrusted text for safe display in confirmation prompts or UI panels.
///
/// Preserves newlines (`\n`) and tabs (`\t`), while converting all other Unicode
/// Cc control characters, bidirectional control characters (including U+061C, U+200E,
/// U+200F, U+202A..=U+202E, U+2066..=U+2069), and Unicode line/paragraph separators
/// (U+2028 and U+2029) into visible escaped forms (`\r`, `\x1b`, `\b`, `\0`, `\f`,
/// `\x..`, or `\u{...}`).
///
/// Ordinary formatting characters such as zero-width joiners are not escaped,
/// preserving multi-codepoint emoji rendering.
pub fn escape_untrusted_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\x1b"),
            '\x08' => out.push_str("\\b"),
            '\0' => out.push_str("\\0"),
            '\x0c' => out.push_str("\\f"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\x{:02x}", c as u8);
            }
            c if is_bidi_control(c) || is_line_or_paragraph_separator(c) || c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_unchanged() {
        let input = "hello world! 123";
        assert_eq!(escape_untrusted_text(input), input);
    }

    #[test]
    fn preserves_newlines_and_tabs() {
        let input = "line1\nline2\twith tab\n";
        assert_eq!(escape_untrusted_text(input), input);
    }

    #[test]
    fn common_ascii_controls_escaped() {
        let raw = "cr:\r esc:\x1b bs:\x08 null:\0 ff:\x0c bell:\x07 del:\x7f";
        let escaped = escape_untrusted_text(raw);

        assert!(escaped.contains("\\r"));
        assert!(escaped.contains("\\x1b"));
        assert!(escaped.contains("\\b"));
        assert!(escaped.contains("\\0"));
        assert!(escaped.contains("\\f"));
        assert!(escaped.contains("\\x07"));
        assert!(escaped.contains("\\x7f"));

        assert!(!escaped.contains('\r'));
        assert!(!escaped.contains('\x1b'));
        assert!(!escaped.contains('\x08'));
        assert!(!escaped.contains('\0'));
        assert!(!escaped.contains('\x0c'));
        assert!(!escaped.contains('\x07'));
        assert!(!escaped.contains('\x7f'));
    }

    #[test]
    fn unicode_c1_controls_escaped() {
        let raw = "c1:\u{0080} and \u{009f}";
        let escaped = escape_untrusted_text(raw);
        assert!(escaped.contains("\\u{80}"));
        assert!(escaped.contains("\\u{9f}"));
        assert!(!escaped.contains('\u{0080}'));
        assert!(!escaped.contains('\u{009f}'));
    }

    #[test]
    fn bidi_control_characters_are_escaped() {
        let bidi_chars = [
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
            '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        for &bc in &bidi_chars {
            let raw = format!("start {bc} middle {bc} end");
            let escaped = escape_untrusted_text(&raw);
            assert!(
                !escaped.contains(bc),
                "escaped output must not contain raw bidi control char {bc:?}"
            );
            let expected = format!("\\u{{{:x}}}", bc as u32);
            assert!(
                escaped.contains(&expected),
                "expected {expected} in {escaped}"
            );
        }
    }

    #[test]
    fn line_and_paragraph_separators_are_escaped() {
        let raw = "before\u{2028}middle\u{2029}after";
        let escaped = escape_untrusted_text(raw);
        assert!(!escaped.contains('\u{2028}'));
        assert!(!escaped.contains('\u{2029}'));
        assert!(escaped.contains("\\u{2028}"));
        assert!(escaped.contains("\\u{2029}"));
        assert_eq!(escaped, "before\\u{2028}middle\\u{2029}after");
    }

    #[test]
    fn emojis_and_zero_width_joiners_preserved() {
        let family = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        let escaped = escape_untrusted_text(family);
        assert_eq!(escaped, family);
    }
}
