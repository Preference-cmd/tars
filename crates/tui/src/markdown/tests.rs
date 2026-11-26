use super::{
    MAX_HIGHLIGHT_BYTES, highlight_code, highlight_diff_line, render_markdown, wrap_styled_line,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const ACCENT: Color = Color::Rgb(215, 147, 114);
const MUTED: Color = Color::DarkGray;

fn plain_lines(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

fn joined_text(lines: &[Line<'_>]) -> String {
    plain_lines(lines).join("\n")
}

fn all_spans<'a>(lines: &'a [Line<'static>]) -> Vec<&'a Span<'static>> {
    lines.iter().flat_map(|l| l.spans.iter()).collect()
}

fn assert_no_raw_controls(lines: &[Line<'_>]) {
    let forbidden = [
        '\x1b', '\r', '\0', '\x07', '\x08', '\x0c', '\x7f', '\t', '\n', '\u{061c}', '\u{200e}',
        '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}',
        '\u{2067}', '\u{2068}', '\u{2069}', '\u{2028}', '\u{2029}',
    ];
    for (line_idx, line) in lines.iter().enumerate() {
        for span in &line.spans {
            for &ch in &forbidden {
                assert!(
                    !span.content.contains(ch),
                    "line {line_idx} span {:?} leaked forbidden char {ch:?}",
                    span.content
                );
            }
            // Also verify no solid RGB background colors are set anywhere.
            assert!(
                !matches!(span.style.bg, Some(Color::Rgb(_, _, _))),
                "line {line_idx} span {:?} set solid RGB background {:?}",
                span.content,
                span.style.bg
            );
        }
    }
}

#[test]
fn empty_and_whitespace_inputs_never_panic() {
    for width in [0, 1, 2, 10, 80] {
        assert!(render_markdown("", width).is_empty());
        assert!(render_markdown("   \n\t\n  ", width).is_empty());
    }
}

#[test]
fn headings_h1_to_h6_have_distinct_hierarchy_and_spacing() {
    let md = "# H1\n\n## H2\n\n### H3\n\n#### H4\n\n##### H5\n\n###### H6";
    let lines = render_markdown(md, 80);
    assert_no_raw_controls(&lines);

    let find_span = |needle: &str| {
        all_spans(&lines)
            .into_iter()
            .find(|s| s.content.as_ref() == needle)
            .unwrap_or_else(|| panic!("missing span {needle}"))
    };

    let h1 = find_span("H1");
    assert_eq!(h1.style.fg, Some(ACCENT));
    assert!(h1.style.add_modifier.contains(Modifier::BOLD));
    assert!(h1.style.add_modifier.contains(Modifier::UNDERLINED));

    let h2 = find_span("H2");
    assert_eq!(h2.style.fg, Some(ACCENT));
    assert!(h2.style.add_modifier.contains(Modifier::BOLD));

    let h3 = find_span("H3");
    assert_eq!(h3.style.fg, Some(Color::Cyan));
    assert!(h3.style.add_modifier.contains(Modifier::BOLD));

    let h4 = find_span("H4");
    assert!(h4.style.add_modifier.contains(Modifier::BOLD));

    let h5 = find_span("H5");
    let h6 = find_span("H6");
    assert_eq!(h5.style.fg, Some(MUTED));
    assert_eq!(h6.style.fg, Some(MUTED));
    assert!(h5.style.add_modifier.contains(Modifier::BOLD));
    assert!(h6.style.add_modifier.contains(Modifier::BOLD));

    let text = plain_lines(&lines);
    assert!(!text.last().unwrap().is_empty(), "no trailing blank lines");
}

#[test]
fn inline_formatting_and_nesting_preserve_style_stack() {
    let md = "plain **bold** *italic* ~~strike~~ `code` ***both*** **bold `inner` after**";
    let lines = render_markdown(md, 120);
    assert_no_raw_controls(&lines);
    let spans = all_spans(&lines);

    let span_for = |needle: &str| {
        spans
            .iter()
            .find(|s| s.content.as_ref() == needle)
            .unwrap_or_else(|| panic!("missing span {needle:?} in {spans:?}"))
    };

    assert!(span_for("bold").style.add_modifier.contains(Modifier::BOLD));
    assert!(
        span_for("italic")
            .style
            .add_modifier
            .contains(Modifier::ITALIC)
    );
    assert!(
        span_for("strike")
            .style
            .add_modifier
            .contains(Modifier::CROSSED_OUT)
    );
    assert_eq!(span_for("code").style.fg, Some(Color::Cyan));

    let both = span_for("both").style;
    assert!(both.add_modifier.contains(Modifier::BOLD));
    assert!(both.add_modifier.contains(Modifier::ITALIC));

    let inner = span_for("inner").style;
    assert!(inner.add_modifier.contains(Modifier::BOLD));
    assert_eq!(inner.fg, Some(Color::Cyan));

    let after = span_for(" after").style;
    assert!(after.add_modifier.contains(Modifier::BOLD));
    assert_ne!(after.fg, Some(Color::Cyan));
}

#[test]
fn links_dedup_matching_urls_and_render_images() {
    let md = "See [https://example.com](https://example.com) and [docs](https://example.com/docs) and ![logo](https://example.com/logo.png).";
    let lines = render_markdown(md, 120);
    assert_no_raw_controls(&lines);
    let text = joined_text(&lines);

    assert_eq!(text.matches("https://example.com)").count(), 0);
    assert!(text.contains("https://example.com"));
    assert!(text.contains("docs (https://example.com/docs)"));
    assert!(text.contains("[img: logo (https://example.com/logo.png)]"));
}

#[test]
fn lists_support_ordered_unordered_task_nested_and_hanging_indent() {
    let md = "\
- first bullet item wraps cleanly across lines
  - nested item also wraps cleanly
5. ordered fifth item wraps cleanly
6. ordered sixth
- [x] completed task wraps here
- [ ] pending task";
    let lines = render_markdown(md, 24);
    assert_no_raw_controls(&lines);
    let text = plain_lines(&lines);

    for l in &lines {
        assert!(l.width() <= 24, "line exceeded width 24: {l:?}");
    }

    assert!(text[0].starts_with("• first"));
    assert!(text[1].starts_with("  "));
    assert!(text.iter().any(|l| l.starts_with("  ◦ nested")));
    assert!(text.iter().any(|l| l.starts_with("    ")));
    assert!(text.iter().any(|l| l.starts_with("5. ordered")));
    assert!(
        text.iter()
            .any(|l| l.starts_with("   ") && l.contains("wraps"))
    );
    assert!(text.iter().any(|l| l.starts_with("6. ordered sixth")));

    let done_span = all_spans(&lines)
        .into_iter()
        .find(|s| s.content.as_ref() == "[x] ")
        .expect("missing [x] span");
    assert_eq!(done_span.style.fg, Some(Color::Green));

    let todo_span = all_spans(&lines)
        .into_iter()
        .find(|s| s.content.as_ref() == "[ ] ")
        .expect("missing [ ] span");
    assert_eq!(todo_span.style.fg, Some(MUTED));

    assert!(text.iter().any(|l| l.starts_with("[x] completed")));
    assert!(
        text.iter()
            .any(|l| l.starts_with("    ") && l.contains("here"))
    );
}

#[test]
fn blockquotes_nested_and_wrapped_keep_prefix() {
    let md = "> Outer quote wraps over multiple lines cleanly\n>\n> > Inner quote wraps too";
    let lines = render_markdown(md, 22);
    assert_no_raw_controls(&lines);
    let text = plain_lines(&lines);

    for (l, t) in lines.iter().zip(text.iter()) {
        assert!(l.width() <= 22, "blockquote exceeded width: {l:?}");
        assert!(t.starts_with("│ "), "missing quote prefix on {t:?}");
    }
    assert!(
        text.iter().filter(|l| l.starts_with("│ │ ")).count() >= 2,
        "nested quote should preserve │ │  on wrapped continuation: {text:?}"
    );
}

#[test]
fn horizontal_rules_adapt_to_viewport_width() {
    let wide = plain_lines(&render_markdown("---", 40));
    assert_eq!(wide, vec!["─".repeat(40)]);

    let narrow = plain_lines(&render_markdown("---", 10));
    assert_eq!(narrow, vec!["─".repeat(10)]);

    let tiny = plain_lines(&render_markdown("---", 2));
    assert_eq!(tiny, vec!["──"]);
}

#[test]
fn tables_align_columns_wrap_cells_and_degrade_on_very_narrow_viewports() {
    let md = "\
| Left | Center | Right |
| :--- | :---: | ---: |
| alpha beta gamma | mid | 42 |
| 中文内容 | ok | 1000 |
";

    // Wide viewport: boxed table with header & alignments.
    let wide = render_markdown(md, 60);
    assert_no_raw_controls(&wide);
    let wide_text = plain_lines(&wide);
    assert!(wide_text[0].starts_with('┌'));
    assert!(wide_text.last().unwrap().starts_with('└'));
    assert!(wide_text.iter().any(|l| l.contains('┼')));
    for l in &wide {
        assert!(l.width() <= 60);
    }

    // Check bold header span and explicit Center / Right alignment padding
    let left_hdr = all_spans(&wide)
        .into_iter()
        .find(|s| s.content.as_ref().contains("Left"))
        .unwrap();
    assert!(left_hdr.style.add_modifier.contains(Modifier::BOLD));
    assert!(
        wide_text.iter().any(|l| l.contains("│  mid   │")
            || l.contains("│   mid  │")
            || l.contains("│  ok  │")),
        "expected centered padding in middle column: {wide_text:?}"
    );
    assert!(
        wide_text.iter().any(|l| l.contains("│    42 │")),
        "expected right-aligned padding for 42: {wide_text:?}"
    );

    // Medium narrow (>= 20): still boxed, wraps multi-line cells without exceeding width.
    let medium = render_markdown(md, 26);
    assert_no_raw_controls(&medium);
    for l in &medium {
        assert!(l.width() <= 26, "medium table overflow: {l:?}");
    }
    let medium_compact: String = medium
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect::<String>()
        .chars()
        .filter(|c| {
            !matches!(
                c,
                '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | '│' | '─' | ' '
            )
        })
        .collect();
    for expected in ["alpha", "beta", "gamma", "中文内容", "42", "1000"] {
        assert!(
            medium_compact.contains(expected),
            "lost {expected} in {medium_compact:?}"
        );
    }

    // Very narrow (< 20, width = 12, 1, 0): stacked degradation, zero panic, zero lost text.
    for narrow_w in [19u16, 12, 1, 0] {
        let narrow = render_markdown(md, narrow_w);
        assert_no_raw_controls(&narrow);
        let max_allowed = usize::from(narrow_w.max(1));
        for l in &narrow {
            if narrow_w > 1 {
                assert!(
                    l.width() <= max_allowed,
                    "narrow table width {narrow_w} overflowed: {l:?}"
                );
            }
        }
        let compact = joined_text(&narrow).replace('\n', "");
        for expected in [
            "Left", "Center", "Right", "alpha", "beta", "gamma", "中文", "42", "1000",
        ] {
            assert!(
                compact.contains(expected),
                "narrow width {narrow_w} dropped {expected:?} in {compact:?}"
            );
        }
    }
}

#[test]
fn code_blocks_render_header_guide_wrap_without_dropping_lines() {
    let md = "```rust,no_run\nfn main() {\n    let very_long_identifier_name = \"hello world from rust\";\n}\n```";
    let lines = render_markdown(md, 25);
    assert_no_raw_controls(&lines);
    let text = plain_lines(&lines);

    assert_eq!(text.first().unwrap(), "╭─ rust,no_run");
    assert_eq!(text.last().unwrap(), "╰──");
    for l in &lines {
        assert!(l.width() <= 25, "code block line exceeded 25: {l:?}");
    }
    for body_line in &text[1..text.len() - 1] {
        assert!(
            body_line.starts_with("│ "),
            "missing code guide: {body_line:?}"
        );
    }
    let body_joined: String = text[1..text.len() - 1]
        .iter()
        .map(|l| l.strip_prefix("│ ").unwrap_or(l))
        .collect();
    assert!(body_joined.contains("very_long_identifier_name"));
    assert!(body_joined.contains("hello world from rust"));
}

#[test]
fn fenced_diff_and_indented_code_blocks_render_cleanly() {
    let md = "```diff\n@@ -1 +1 @@\n-old\n+new\n```\n\n    indented line";
    let lines = render_markdown(md, 40);
    assert_no_raw_controls(&lines);
    let spans = all_spans(&lines);
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("@@ -1 +1 @@") && s.style.fg == Some(Color::Cyan))
    );
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("-old") && s.style.fg == Some(Color::Red))
    );
    assert!(
        spans
            .iter()
            .any(|s| s.content.contains("+new") && s.style.fg == Some(Color::Green))
    );
    assert!(plain_lines(&lines).iter().any(|l| l == "│ indented line"));
}

#[test]
fn syntax_highlighter_covers_all_eleven_language_groups_and_lifetimes() {
    let has_fg = |lines: &[Line<'_>], needle: &str, color: Color| {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains(needle) && s.style.fg == Some(color))
    };

    // 1. Rust / rs (including lifetime `'a` not swallowing string `"ok"`, raw string, macro)
    let rust = highlight_code(
        "rs",
        "/* block\ncomment */ fn f<'a>(x: &'a str) -> Vec<u64> { println!(\"ok\"); r#\"raw\"#; 42 }",
    );
    assert!(has_fg(&rust, "/* block", MUTED));
    assert!(has_fg(&rust, "comment */", MUTED));
    assert!(has_fg(&rust, "fn", Color::Magenta));
    assert!(has_fg(&rust, "Vec", Color::Cyan));
    assert!(has_fg(&rust, "println!", Color::Cyan));
    assert!(has_fg(&rust, "\"ok\"", Color::Green));
    assert!(has_fg(&rust, "r#\"raw\"#", Color::Green));
    assert!(has_fg(&rust, "42", Color::Yellow));
    // Ensure lifetime 'a did not color `str` or `Vec` green
    assert!(!has_fg(&rust, "Vec", Color::Green));

    // 2. TypeScript / JS / tsx / jsx
    for alias in ["typescript", "ts", "tsx", "javascript", "js", "jsx"] {
        let ts = highlight_code(
            alias,
            "export const x: string = `hi`; // note\nreturn true;",
        );
        assert!(has_fg(&ts, "export", Color::Magenta), "{alias}");
        assert!(has_fg(&ts, "`hi`", Color::Green), "{alias}");
        assert!(has_fg(&ts, "// note", MUTED), "{alias}");
        assert!(has_fg(&ts, "true", Color::Yellow), "{alias}");
    }

    // 3. Python / py
    for alias in ["python", "py"] {
        let py = highlight_code(alias, "def run(n: int) -> None:\n    # cmt\n    return 'x'");
        assert!(has_fg(&py, "def", Color::Magenta));
        assert!(has_fg(&py, "None", Color::Yellow));
        assert!(has_fg(&py, "# cmt", MUTED));
        assert!(has_fg(&py, "'x'", Color::Green));
    }

    // 4. Go
    let go = highlight_code(
        "go",
        "func main() { s := `raw`; if nil == nil { return 7 } }",
    );
    assert!(has_fg(&go, "func", Color::Magenta));
    assert!(has_fg(&go, "`raw`", Color::Green));
    assert!(has_fg(&go, "nil", Color::Yellow));
    assert!(has_fg(&go, "7", Color::Yellow));

    // 5. Shell / sh / bash / zsh
    for alias in ["sh", "bash", "zsh", "shell"] {
        let sh = highlight_code(alias, "# run\nif [ -n \"$HOME\" ]; then export A='1'; fi");
        assert!(has_fg(&sh, "# run", MUTED), "{alias}");
        assert!(has_fg(&sh, "if", Color::Magenta), "{alias}");
        assert!(has_fg(&sh, "\"$HOME\"", Color::Green), "{alias}");
        assert!(has_fg(&sh, "'1'", Color::Green), "{alias}");
    }

    // 6. JSON / JSONC
    for alias in ["json", "jsonc"] {
        let js = highlight_code(alias, "{\"key\": \"val\", \"num\": 12, \"ok\": false}");
        assert!(has_fg(&js, "\"key\"", Color::Cyan));
        assert!(has_fg(&js, "\"val\"", Color::Green));
        assert!(has_fg(&js, "12", Color::Yellow));
        assert!(has_fg(&js, "false", Color::Yellow));
    }

    // 7. TOML
    let toml = highlight_code("toml", "[package]\nname = \"tars\" # c\nenabled = true");
    assert!(has_fg(&toml, "[package]", Color::Cyan));
    assert!(has_fg(&toml, "name", Color::Cyan));
    assert!(has_fg(&toml, "\"tars\"", Color::Green));
    assert!(has_fg(&toml, "# c", MUTED));
    assert!(has_fg(&toml, "true", Color::Yellow));

    // 8. YAML / yml
    for alias in ["yaml", "yml"] {
        let yml = highlight_code(alias, "- name: \"tars\"\n  count: 3 # c\n  ok: true");
        assert!(has_fg(&yml, "name", Color::Cyan));
        assert!(has_fg(&yml, "\"tars\"", Color::Green));
        assert!(has_fg(&yml, "3", Color::Yellow));
        assert!(has_fg(&yml, "# c", MUTED));
        assert!(has_fg(&yml, "true", Color::Yellow));
    }

    // 9. SQL (case-insensitive)
    let sql = highlight_code(
        "sql",
        "select id, name from users where active = true; -- cmt",
    );
    assert!(has_fg(&sql, "select", Color::Magenta));
    assert!(has_fg(&sql, "from", Color::Magenta));
    assert!(has_fg(&sql, "true", Color::Yellow));
    assert!(has_fg(&sql, "-- cmt", MUTED));

    // 10. C / C++
    for alias in ["c", "cpp", "c++", "h", "hpp"] {
        let c = highlight_code(alias, "#include <stdio.h>\nint main() { return 0; }");
        assert!(has_fg(&c, "#include", Color::Magenta), "{alias}");
        assert!(has_fg(&c, "int", Color::Magenta), "{alias}");
        assert!(has_fg(&c, "0", Color::Yellow), "{alias}");
    }

    // 11. Diff / patch
    for alias in ["diff", "patch"] {
        let diff = highlight_code(
            alias,
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n ctx",
        );
        assert!(has_fg(&diff, "diff --git", MUTED), "{alias}");
        assert!(has_fg(&diff, "--- a/x", MUTED), "{alias}");
        assert!(has_fg(&diff, "+++ b/x", MUTED), "{alias}");
        assert!(has_fg(&diff, "@@ -1,2 +1,2 @@", Color::Cyan), "{alias}");
        assert!(has_fg(&diff, "-old", Color::Red), "{alias}");
        assert!(has_fg(&diff, "+new", Color::Green), "{alias}");
    }

    let single_plus = highlight_diff_line("+added");
    assert_eq!(single_plus.spans[0].style.fg, Some(Color::Green));
    let single_minus = highlight_diff_line("-removed");
    assert_eq!(single_minus.spans[0].style.fg, Some(Color::Red));
    let single_hunk = highlight_diff_line("@@ -1 +1 @@");
    assert_eq!(single_hunk.spans[0].style.fg, Some(Color::Cyan));
}

#[test]
fn highlighter_falls_back_cleanly_for_unknown_lang_and_oversized_blocks() {
    let unknown = highlight_code("brainfuck", "+++\n---");
    assert_eq!(unknown.len(), 2);
    for line in &unknown {
        for span in &line.spans {
            assert_eq!(span.style, Style::default());
        }
    }

    // > 64 KiB input
    let line = "let x = 123;\n";
    let count = (MAX_HIGHLIGHT_BYTES / line.len()) + 100;
    let big = line.repeat(count);
    assert!(big.len() > MAX_HIGHLIGHT_BYTES);
    let highlighted = highlight_code("rust", &big);
    assert_eq!(highlighted.len(), count);
    for l in &highlighted {
        for span in &l.spans {
            assert_eq!(span.style, Style::default());
        }
    }
}

#[test]
fn untrusted_controls_bidi_and_separators_are_escaped_everywhere() {
    let evil = "\x1b[31mred\x1b[0m\u{202e}bidi\u{2066}sep\u{2028}para\u{2029}\t";
    let md = format!(
        "# Title {evil}\n\n\
         Prose {evil} and &#x1b; entity.\n\n\
         [{evil}](https://example.com/{evil})\n\n\
         [&#x1b;&#x202e;](https://example.com/&#x1b;&#9;&#x2028;)\n\n\
         ![{evil}](https://example.com/{evil}.png)\n\n\
         ![&#x1b;](https://example.com/&#x202e;.png)\n\n\
         ```{evil}\nlet s = \"{evil}\";\n```\n\n\
         ```rust&#x1b;&#x202e;\nlet entity = 1;\n```\n\n\
         | H{evil} |\n| --- |\n| C{evil} |\n\n\
         | H&#x1b; |\n| --- |\n| C&#x202e;&#9; |\n"
    );

    let lines = render_markdown(&md, 60);
    assert_no_raw_controls(&lines);

    let joined = joined_text(&lines);
    assert!(joined.contains("\\x1b"));
    assert!(joined.contains("\\u{202e}"));
    assert!(joined.contains("\\u{2066}"));
    assert!(joined.contains("\\u{2028}"));
    assert!(joined.contains("\\u{2029}"));

    // Direct highlighter entry points must also sanitize
    let direct_code = highlight_code("rust", &format!("let s = \"{evil}\";"));
    assert_no_raw_controls(&direct_code);
    let direct_diff = highlight_diff_line(&format!("+{evil}"));
    assert_no_raw_controls(&[direct_diff]);
}

#[test]
fn cjk_and_emoji_wrap_accurately_by_display_width() {
    let md = "你好世界测试中文折行与表情符号👨‍👩‍👧‍👦😀👍混合abcde";
    for width in [20u16, 10, 5, 2, 1] {
        let lines = render_markdown(md, width);
        assert_no_raw_controls(&lines);
        for l in &lines {
            if width > 1 {
                assert!(
                    l.width() <= usize::from(width),
                    "width {width} exceeded by line {l:?} (actual {})",
                    l.width()
                );
            } else {
                // At width=1, a single wide CJK/emoji grapheme (width 2) occupies its own line.
                assert!(
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                        .chars()
                        .count()
                        <= 8,
                    "unexpected multi-grapheme line at width 1: {l:?}"
                );
            }
        }
        let reconstructed = joined_text(&lines).replace('\n', "");
        assert_eq!(reconstructed, md);
    }
}

#[test]
fn wrap_styled_line_prepends_continuation_prefix_and_preserves_styles() {
    let line = Line::from(vec![
        Span::styled("hello ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled("world from tars", Style::default().fg(Color::Cyan)),
    ]);
    let prefix = [Span::styled("> ", Style::default().fg(MUTED))];
    let wrapped = wrap_styled_line(line, 12, &prefix);
    assert!(wrapped.len() >= 2);
    for l in &wrapped {
        assert!(l.width() <= 12, "wrapped line exceeded 12: {l:?}");
    }
    let text = plain_lines(&wrapped);
    assert!(text[1].starts_with("> "));

    // Empty line still produces one empty line
    let empty_wrapped = wrap_styled_line(Line::default(), 10, &prefix);
    assert_eq!(empty_wrapped.len(), 1);
}

#[test]
fn trailing_whitespace_at_wrap_width_does_not_emit_spurious_prefix_line() {
    let quoted = plain_lines(&render_markdown("> hello ", 7));
    assert_eq!(quoted, vec!["│ hello"]);

    let prefix = [Span::styled("│ ", Style::default().fg(MUTED))];
    let line = Line::from(vec![
        Span::styled("│ ", Style::default().fg(MUTED)),
        Span::raw("hello "),
    ]);
    let wrapped = plain_lines(&wrap_styled_line(line, 7, &prefix));
    assert_eq!(wrapped, vec!["│ hello"]);
}

#[test]
fn multiline_strings_and_deep_nesting_at_narrow_widths_stay_safe() {
    let rust = highlight_code("rust", "let s = r#\"line1\nline2\"#;\nlet x = 1;");
    assert_eq!(rust.len(), 3);
    assert_eq!(rust[0].spans.last().unwrap().style.fg, Some(Color::Green));
    assert_eq!(rust[1].spans[0].style.fg, Some(Color::Green));
    assert_eq!(rust[2].spans[0].style.fg, Some(Color::Magenta));

    let py = highlight_code("python", "doc = \"\"\"first\nsecond\"\"\"\nreturn True");
    assert_eq!(py.len(), 3);
    assert_eq!(py[0].spans.last().unwrap().style.fg, Some(Color::Green));
    assert_eq!(py[1].spans[0].style.fg, Some(Color::Green));
    assert_eq!(py[2].spans[0].style.fg, Some(Color::Magenta));

    let deep_md = "> > - item one wraps\n> >   - nested two wraps";
    for width in [2u16, 3, 4] {
        let rendered = render_markdown(deep_md, width);
        assert_no_raw_controls(&rendered);
        for l in &rendered {
            assert!(
                l.width() <= usize::from(width),
                "width {width} overflowed: {l:?}"
            );
        }
        let compact = joined_text(&rendered)
            .chars()
            .filter(|c| !matches!(c, '│' | '•' | '◦' | ' ' | '\n'))
            .collect::<String>();
        assert!(compact.contains("itemonewraps"));
        assert!(compact.contains("nestedtwowraps"));
    }
}
