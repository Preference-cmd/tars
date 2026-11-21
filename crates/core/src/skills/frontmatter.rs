use super::types::SkillDiagnosticKind;

/// Maximum `name` length defined by `agentskills.io`.
const MAX_NAME_LEN: usize = 64;
/// Maximum `description` length in Unicode scalar values defined by `agentskills.io`.
const MAX_DESCRIPTION_CHARS: usize = 1024;

/// Validated YAML frontmatter fields extracted from `SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillFrontmatter {
    /// Skill name (1–64 chars, `[a-z0-9-]`, matching parent directory).
    pub name: String,
    /// Non-empty skill description (1–1024 Unicode scalar values).
    pub description: String,
}

/// Parsed `SKILL.md` document containing validated frontmatter and stripped body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedSkillDocument<'a> {
    /// Validated frontmatter metadata.
    pub meta: SkillFrontmatter,
    /// Markdown body following the closing `---` delimiter.
    pub body: &'a str,
}

/// Parses and validates a `SKILL.md` document against `agentskills.io` rules.
///
/// `expected_dir_name` is the parent directory name that `name` must match.
pub fn parse_skill_document<'a>(
    content: &'a str,
    expected_dir_name: &str,
) -> Result<ParsedSkillDocument<'a>, (SkillDiagnosticKind, String)> {
    let trimmed_bom = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (frontmatter_text, body) = split_frontmatter(trimmed_bom)?;
    let (name, description) = parse_frontmatter_fields(frontmatter_text)?;

    validate_name(&name)?;
    if name != expected_dir_name {
        return Err((
            SkillDiagnosticKind::NameMismatch,
            format!("skill name {name:?} does not match parent directory {expected_dir_name:?}"),
        ));
    }

    let description = validate_description(&description)?;

    Ok(ParsedSkillDocument {
        meta: SkillFrontmatter { name, description },
        body,
    })
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), (SkillDiagnosticKind, String)> {
    let mut offset = 0usize;
    let mut first_end = None;
    let mut lines = content.split_inclusive('\n');
    let Some(first_line) = lines.next() else {
        return Err((
            SkillDiagnosticKind::InvalidFrontmatter,
            "SKILL.md is empty; expected YAML frontmatter starting with ---".into(),
        ));
    };
    let first_trimmed = first_line.trim_end_matches(['\r', '\n', ' ', '\t']);
    if first_trimmed != "---" {
        return Err((
            SkillDiagnosticKind::InvalidFrontmatter,
            "SKILL.md must start with YAML frontmatter delimiter ---".into(),
        ));
    }
    offset += first_line.len();
    let fm_start = offset;

    for line in lines {
        let line_clean = line.trim_end_matches(['\r', '\n', ' ', '\t']);
        if line_clean == "---" {
            let fm_end = offset;
            let body_start = offset + line.len();
            first_end = Some((&content[fm_start..fm_end], &content[body_start..]));
            break;
        }
        offset += line.len();
    }

    first_end.ok_or_else(|| {
        (
            SkillDiagnosticKind::InvalidFrontmatter,
            "unterminated YAML frontmatter; missing closing ---".into(),
        )
    })
}

fn parse_frontmatter_fields(fm: &str) -> Result<(String, String), (SkillDiagnosticKind, String)> {
    let lines: Vec<&str> = fm
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut idx = 0usize;

    while idx < lines.len() {
        let line = lines[idx];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-') {
            return Err((
                SkillDiagnosticKind::InvalidFrontmatter,
                format!(
                    "unexpected indented or list line in frontmatter: line {}",
                    idx + 2
                ),
            ));
        }
        let Some((raw_key, raw_rest)) = line.split_once(':') else {
            return Err((
                SkillDiagnosticKind::InvalidFrontmatter,
                format!("expected key: value in frontmatter: line {}", idx + 2),
            ));
        };
        let key = raw_key.trim();
        if key.is_empty() {
            return Err((
                SkillDiagnosticKind::InvalidFrontmatter,
                format!("empty frontmatter key at line {}", idx + 2),
            ));
        }

        idx += 1;
        let (value, next_idx) = parse_yaml_value(raw_rest, &lines, idx, key == "description")?;
        idx = next_idx;

        match key {
            "name" => {
                if name.is_some() {
                    return Err((
                        SkillDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'name'".into(),
                    ));
                }
                name = Some(value.ok_or_else(|| {
                    (
                        SkillDiagnosticKind::InvalidName,
                        "frontmatter 'name' must be a scalar string".into(),
                    )
                })?);
            }
            "description" => {
                if description.is_some() {
                    return Err((
                        SkillDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'description'".into(),
                    ));
                }
                description = Some(value.ok_or_else(|| {
                    (
                        SkillDiagnosticKind::InvalidDescription,
                        "frontmatter 'description' must be a scalar string".into(),
                    )
                })?);
            }
            _ => {
                // Unknown optional fields (license, compatibility, metadata, allowed-tools, etc.)
                // are tolerated per agentskills.io.
            }
        }
    }

    let name = name.ok_or_else(|| {
        (
            SkillDiagnosticKind::InvalidName,
            "missing required frontmatter field 'name'".into(),
        )
    })?;
    let description = description.ok_or_else(|| {
        (
            SkillDiagnosticKind::InvalidDescription,
            "missing required frontmatter field 'description'".into(),
        )
    })?;
    Ok((name, description))
}

fn parse_yaml_value(
    raw_rest: &str,
    lines: &[&str],
    mut idx: usize,
    allow_unquoted_colon_fallback: bool,
) -> Result<(Option<String>, usize), (SkillDiagnosticKind, String)> {
    let trimmed = raw_rest.trim();

    if trimmed.is_empty() || trimmed.starts_with('#') {
        // Consume any indented nested map/list or sequence items belonging to this key.
        while idx < lines.len() {
            let next = lines[idx];
            if next.trim().is_empty() || next.trim_start().starts_with('#') {
                idx += 1;
                continue;
            }
            if next.starts_with(' ')
                || next.starts_with('\t')
                || next.starts_with("- ")
                || next == "-"
            {
                idx += 1;
                continue;
            }
            break;
        }
        return Ok((None, idx));
    }

    if let Some((folded, chomp_strip)) = parse_block_indicator(trimmed) {
        let mut block_lines = Vec::new();
        while idx < lines.len() {
            let next = lines[idx];
            if next.trim().is_empty() {
                block_lines.push("");
                idx += 1;
                continue;
            }
            if next.starts_with(' ') || next.starts_with('\t') {
                block_lines.push(next);
                idx += 1;
            } else {
                break;
            }
        }
        while block_lines.last().is_some_and(|l| l.is_empty()) {
            block_lines.pop();
        }
        let min_indent = block_lines
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count())
            .min()
            .unwrap_or(0);
        let stripped: Vec<&str> = block_lines
            .iter()
            .map(|l| {
                if l.len() >= min_indent {
                    &l[min_indent..]
                } else {
                    ""
                }
            })
            .collect();
        let mut joined = if folded {
            fold_lines(&stripped)
        } else {
            stripped.join("\n")
        };
        if chomp_strip {
            joined = joined.trim_end_matches('\n').to_owned();
        }
        return Ok((Some(joined), idx));
    }

    if trimmed.starts_with('"') {
        let parsed = parse_double_quoted(trimmed).map_err(|msg| {
            (
                SkillDiagnosticKind::InvalidFrontmatter,
                format!("invalid double-quoted frontmatter value: {msg}"),
            )
        })?;
        return Ok((Some(parsed), idx));
    }

    if trimmed.starts_with('\'') {
        let parsed = parse_single_quoted(trimmed).map_err(|msg| {
            (
                SkillDiagnosticKind::InvalidFrontmatter,
                format!("invalid single-quoted frontmatter value: {msg}"),
            )
        })?;
        return Ok((Some(parsed), idx));
    }

    // Plain (unquoted) scalar, with relaxed support for unquoted colons in `description`
    // and indented continuation lines.
    let mut parts = vec![trimmed.to_owned()];
    while idx < lines.len() {
        let next = lines[idx];
        if next.trim().is_empty() {
            break;
        }
        if next.starts_with(' ') || next.starts_with('\t') {
            parts.push(next.trim().to_owned());
            idx += 1;
        } else {
            break;
        }
    }
    let combined = parts.join(" ");
    if !allow_unquoted_colon_fallback {
        // For non-description plain scalars, strip trailing `# comment` if preceded by space.
        if let Some((before, _)) = combined.split_once(" #") {
            return Ok((Some(before.trim_end().to_owned()), idx));
        }
    }
    Ok((Some(combined), idx))
}

fn parse_block_indicator(value: &str) -> Option<(bool, bool)> {
    let head = value.split('#').next()?.trim();
    match head {
        "|" | "|+" => Some((false, false)),
        "|-" => Some((false, true)),
        ">" | ">+" => Some((true, false)),
        ">-" => Some((true, true)),
        _ => None,
    }
}

fn fold_lines(lines: &[&str]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            if line.is_empty() || lines[i - 1].is_empty() {
                out.push('\n');
            } else {
                out.push(' ');
            }
        }
        out.push_str(line);
    }
    out
}

fn parse_double_quoted(input: &str) -> Result<String, &'static str> {
    let mut chars = input.chars();
    if chars.next() != Some('"') {
        return Err("missing opening quote");
    }
    let mut out = String::new();
    let mut closed = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                closed = true;
                break;
            }
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => return Err("unterminated escape"),
            },
            other => out.push(other),
        }
    }
    if !closed {
        return Err("unterminated double-quoted string");
    }
    let rest: String = chars.collect();
    let rest_trimmed = rest.trim();
    if !rest_trimmed.is_empty() && !rest_trimmed.starts_with('#') {
        return Err("unexpected trailing characters after closing quote");
    }
    Ok(out)
}

fn parse_single_quoted(input: &str) -> Result<String, &'static str> {
    let mut chars = input.chars().peekable();
    if chars.next() != Some('\'') {
        return Err("missing opening quote");
    }
    let mut out = String::new();
    let mut closed = false;
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
            } else {
                closed = true;
                break;
            }
        } else {
            out.push(c);
        }
    }
    if !closed {
        return Err("unterminated single-quoted string");
    }
    let rest: String = chars.collect();
    let rest_trimmed = rest.trim();
    if !rest_trimmed.is_empty() && !rest_trimmed.starts_with('#') {
        return Err("unexpected trailing characters after closing quote");
    }
    Ok(out)
}

fn validate_name(name: &str) -> Result<(), (SkillDiagnosticKind, String)> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err((
            SkillDiagnosticKind::InvalidName,
            format!("skill name must be 1..={MAX_NAME_LEN} characters"),
        ));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err((
            SkillDiagnosticKind::InvalidName,
            format!("skill name {name:?} must not start/end with '-' or contain '--'"),
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err((
            SkillDiagnosticKind::InvalidName,
            format!("skill name {name:?} may only contain [a-z0-9-]"),
        ));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<String, (SkillDiagnosticKind, String)> {
    let trimmed = description.trim();
    let char_count = trimmed.chars().count();
    if char_count == 0 {
        return Err((
            SkillDiagnosticKind::InvalidDescription,
            "skill description must not be empty".into(),
        ));
    }
    if char_count > MAX_DESCRIPTION_CHARS {
        return Err((
            SkillDiagnosticKind::InvalidDescription,
            format!("skill description exceeds {MAX_DESCRIPTION_CHARS} characters ({char_count})"),
        ));
    }
    Ok(trimmed.to_owned())
}
