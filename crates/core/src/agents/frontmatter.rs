use super::types::{AgentDiagnosticKind, AgentModelConfig};
use crate::definition::{BuiltinTool, Temperature};
use crate::events::AgentInteraction;
use std::collections::HashSet;

const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1024;
const MAX_TIMEOUT_SECS: u64 = 86_400;

/// Validated YAML frontmatter fields extracted from an agent Markdown file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFrontmatter {
    /// Validated preset name (`1..=64` `[a-z0-9-]`).
    pub name: String,
    /// Non-empty description (`1..=1024` Unicode scalar values).
    pub description: String,
    /// Optional explicit built-in tool allowlist.
    pub tools: Option<Vec<BuiltinTool>>,
    /// Optional nested `model` overrides.
    pub model: Option<AgentModelConfig>,
    /// Optional positive model round ceiling.
    pub max_model_rounds: Option<u32>,
    /// Optional timeout ceiling in `1..=86400`.
    pub timeout_secs: Option<u64>,
    /// Optional default interaction mode.
    pub default_interaction: Option<AgentInteraction>,
}

/// Parsed agent Markdown document containing validated frontmatter and optional role prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAgentDocument {
    /// Validated frontmatter metadata.
    pub meta: AgentFrontmatter,
    /// Trimmed Markdown body after closing `---`, or `None` when blank.
    pub role_prompt: Option<String>,
}

/// Parses and validates an agent `.md` document with strict YAML frontmatter.
///
/// Unknown top-level or nested keys, invalid tool names, duplicate keys, and
/// out-of-range numeric settings are rejected with a structured diagnostic kind.
pub fn parse_agent_document(
    content: &str,
) -> Result<ParsedAgentDocument, (AgentDiagnosticKind, String)> {
    let trimmed_bom = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (frontmatter_text, body) = split_frontmatter(trimmed_bom)?;
    let meta = parse_frontmatter_fields(frontmatter_text)?;
    let trimmed_body = body.trim();
    let role_prompt = (!trimmed_body.is_empty()).then(|| trimmed_body.to_owned());
    Ok(ParsedAgentDocument { meta, role_prompt })
}

fn split_frontmatter(content: &str) -> Result<(&str, &str), (AgentDiagnosticKind, String)> {
    let mut offset = 0usize;
    let mut lines = content.split_inclusive('\n');
    let Some(first_line) = lines.next() else {
        return Err((
            AgentDiagnosticKind::InvalidFrontmatter,
            "agent file is empty; expected YAML frontmatter starting with ---".into(),
        ));
    };
    if first_line.trim_end_matches(['\r', '\n', ' ', '\t']) != "---" {
        return Err((
            AgentDiagnosticKind::InvalidFrontmatter,
            "agent file must start with YAML frontmatter delimiter ---".into(),
        ));
    }
    offset += first_line.len();
    let fm_start = offset;

    for line in lines {
        if line.trim_end_matches(['\r', '\n', ' ', '\t']) == "---" {
            let fm_end = offset;
            let body_start = offset + line.len();
            return Ok((&content[fm_start..fm_end], &content[body_start..]));
        }
        offset += line.len();
    }

    Err((
        AgentDiagnosticKind::InvalidFrontmatter,
        "unterminated YAML frontmatter; missing closing ---".into(),
    ))
}

fn parse_frontmatter_fields(fm: &str) -> Result<AgentFrontmatter, (AgentDiagnosticKind, String)> {
    let lines: Vec<&str> = fm
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<BuiltinTool>> = None;
    let mut model: Option<AgentModelConfig> = None;
    let mut max_model_rounds: Option<u32> = None;
    let mut timeout_secs: Option<u64> = None;
    let mut default_interaction: Option<AgentInteraction> = None;

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
                AgentDiagnosticKind::InvalidFrontmatter,
                format!(
                    "unexpected indented or list line in frontmatter: line {}",
                    idx + 2
                ),
            ));
        }
        let Some((raw_key, raw_rest)) = line.split_once(':') else {
            return Err((
                AgentDiagnosticKind::InvalidFrontmatter,
                format!("expected key: value in frontmatter: line {}", idx + 2),
            ));
        };
        let key = raw_key.trim();
        if key.is_empty() {
            return Err((
                AgentDiagnosticKind::InvalidFrontmatter,
                format!("empty frontmatter key at line {}", idx + 2),
            ));
        }
        idx += 1;

        match key {
            "name" => {
                if name.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'name'".into(),
                    ));
                }
                let (val, next_idx) = parse_scalar_value(raw_rest, &lines, idx, false, 1)?
                    .ok_or_else(|| {
                        (
                            AgentDiagnosticKind::InvalidName,
                            "frontmatter 'name' must be a scalar string".into(),
                        )
                    })
                    .and_then(|(s, n)| {
                        validate_name(&s)?;
                        Ok((s, n))
                    })?;
                name = Some(val);
                idx = next_idx;
            }
            "description" => {
                if description.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'description'".into(),
                    ));
                }
                let (val, next_idx) = parse_scalar_value(raw_rest, &lines, idx, true, 1)?
                    .ok_or_else(|| {
                        (
                            AgentDiagnosticKind::InvalidDescription,
                            "frontmatter 'description' must be a scalar string".into(),
                        )
                    })
                    .and_then(|(s, n)| {
                        let desc = validate_description(&s)?;
                        Ok((desc, n))
                    })?;
                description = Some(val);
                idx = next_idx;
            }
            "tools" => {
                if tools.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'tools'".into(),
                    ));
                }
                let (parsed_tools, next_idx) = parse_tools_value(raw_rest, &lines, idx)?;
                tools = Some(parsed_tools);
                idx = next_idx;
            }
            "model" => {
                if model.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'model'".into(),
                    ));
                }
                let (parsed_model, next_idx) = parse_model_value(raw_rest, &lines, idx)?;
                model = Some(parsed_model);
                idx = next_idx;
            }
            "max_model_rounds" => {
                if max_model_rounds.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'max_model_rounds'".into(),
                    ));
                }
                let (raw_num, next_idx) = parse_scalar_value(raw_rest, &lines, idx, false, 1)?
                    .ok_or_else(|| {
                        (
                            AgentDiagnosticKind::InvalidLimits,
                            "frontmatter 'max_model_rounds' must be a positive integer".into(),
                        )
                    })?;
                let rounds = raw_num.trim().parse::<u32>().map_err(|_| {
                    (
                        AgentDiagnosticKind::InvalidLimits,
                        format!("invalid 'max_model_rounds': {raw_num:?}"),
                    )
                })?;
                if rounds == 0 {
                    return Err((
                        AgentDiagnosticKind::InvalidLimits,
                        "frontmatter 'max_model_rounds' must be positive".into(),
                    ));
                }
                max_model_rounds = Some(rounds);
                idx = next_idx;
            }
            "timeout_secs" => {
                if timeout_secs.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'timeout_secs'".into(),
                    ));
                }
                let (raw_num, next_idx) = parse_scalar_value(raw_rest, &lines, idx, false, 1)?
                    .ok_or_else(|| {
                        (
                            AgentDiagnosticKind::InvalidLimits,
                            "frontmatter 'timeout_secs' must be an integer in 1..=86400".into(),
                        )
                    })?;
                let secs = raw_num.trim().parse::<u64>().map_err(|_| {
                    (
                        AgentDiagnosticKind::InvalidLimits,
                        format!("invalid 'timeout_secs': {raw_num:?}"),
                    )
                })?;
                if !(1..=MAX_TIMEOUT_SECS).contains(&secs) {
                    return Err((
                        AgentDiagnosticKind::InvalidLimits,
                        format!("frontmatter 'timeout_secs' ({secs}) must be in 1..=86400"),
                    ));
                }
                timeout_secs = Some(secs);
                idx = next_idx;
            }
            "default_interaction" => {
                if default_interaction.is_some() {
                    return Err((
                        AgentDiagnosticKind::InvalidFrontmatter,
                        "duplicate frontmatter key 'default_interaction'".into(),
                    ));
                }
                let (raw_val, next_idx) = parse_scalar_value(raw_rest, &lines, idx, false, 1)?
                    .ok_or_else(|| {
                        (
                            AgentDiagnosticKind::InvalidLimits,
                            "frontmatter 'default_interaction' must be 'single_work' or 'reusable'"
                                .into(),
                        )
                    })?;
                let interaction = raw_val
                    .trim()
                    .parse::<AgentInteraction>()
                    .map_err(|e| (AgentDiagnosticKind::InvalidLimits, e.to_string()))?;
                default_interaction = Some(interaction);
                idx = next_idx;
            }
            unknown => {
                return Err((
                    AgentDiagnosticKind::InvalidFrontmatter,
                    format!("unknown frontmatter key '{unknown}'"),
                ));
            }
        }
    }

    let name = name.ok_or_else(|| {
        (
            AgentDiagnosticKind::InvalidName,
            "missing required frontmatter field 'name'".into(),
        )
    })?;
    let description = description.ok_or_else(|| {
        (
            AgentDiagnosticKind::InvalidDescription,
            "missing required frontmatter field 'description'".into(),
        )
    })?;

    Ok(AgentFrontmatter {
        name,
        description,
        tools,
        model,
        max_model_rounds,
        timeout_secs,
        default_interaction,
    })
}

fn parse_tools_value(
    raw_rest: &str,
    lines: &[&str],
    mut idx: usize,
) -> Result<(Vec<BuiltinTool>, usize), (AgentDiagnosticKind, String)> {
    let trimmed = strip_inline_comment(raw_rest.trim());
    let raw_items: Vec<String> = if trimmed.starts_with('[') {
        if !trimmed.ends_with(']') {
            return Err((
                AgentDiagnosticKind::InvalidTools,
                "unterminated inline tools list; expected closing ']'".into(),
            ));
        }
        let inner = trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            Vec::new()
        } else {
            let mut items = Vec::new();
            for part in inner.split(',') {
                let item = parse_inline_scalar(part.trim())
                    .map_err(|msg| (AgentDiagnosticKind::InvalidTools, msg.to_owned()))?;
                items.push(item);
            }
            items
        }
    } else if trimmed.is_empty() {
        let mut items = Vec::new();
        while idx < lines.len() {
            let next = lines[idx];
            let next_trimmed = next.trim();
            if next_trimmed.is_empty() || next_trimmed.starts_with('#') {
                idx += 1;
                continue;
            }
            if !next.starts_with(' ') && !next.starts_with('\t') && !next.starts_with('-') {
                break;
            }
            let item_rest = next_trimmed
                .strip_prefix("- ")
                .or_else(|| (next_trimmed == "-").then_some(""))
                .ok_or_else(|| {
                    (
                        AgentDiagnosticKind::InvalidTools,
                        format!("expected '- <tool_name>' list item at line {}", idx + 2),
                    )
                })?;
            let item_clean = strip_inline_comment(item_rest.trim());
            let item = parse_inline_scalar(item_clean)
                .map_err(|msg| (AgentDiagnosticKind::InvalidTools, msg.to_owned()))?;
            items.push(item);
            idx += 1;
        }
        items
    } else {
        return Err((
            AgentDiagnosticKind::InvalidTools,
            "frontmatter 'tools' must be a YAML list of built-in tool names".into(),
        ));
    };

    let mut seen = HashSet::with_capacity(raw_items.len());
    let mut out = Vec::with_capacity(raw_items.len());
    for raw in raw_items {
        let Some(tool) = BuiltinTool::from_name(&raw) else {
            return Err((
                AgentDiagnosticKind::InvalidTools,
                format!("unknown tool '{raw}' in frontmatter 'tools'"),
            ));
        };
        if !seen.insert(tool) {
            return Err((
                AgentDiagnosticKind::InvalidTools,
                format!("duplicate tool '{raw}' in frontmatter 'tools'"),
            ));
        }
        out.push(tool);
    }
    Ok((out, idx))
}

fn parse_model_value(
    raw_rest: &str,
    lines: &[&str],
    mut idx: usize,
) -> Result<(AgentModelConfig, usize), (AgentDiagnosticKind, String)> {
    let trimmed = strip_inline_comment(raw_rest.trim());
    let mut cfg = AgentModelConfig::default();
    let mut seen_keys = HashSet::new();

    if trimmed.starts_with('{') {
        if !trimmed.ends_with('}') {
            return Err((
                AgentDiagnosticKind::InvalidModel,
                "unterminated inline model map; expected closing '}'".into(),
            ));
        }
        let inner = trimmed[1..trimmed.len() - 1].trim();
        if inner.is_empty() {
            return Err((
                AgentDiagnosticKind::InvalidModel,
                "frontmatter 'model' map must not be empty".into(),
            ));
        }
        for pair in inner.split(',') {
            let Some((raw_k, raw_v)) = pair.split_once(':') else {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    format!("expected key: value in inline model map: {pair:?}"),
                ));
            };
            let k = raw_k.trim();
            let v = parse_inline_scalar(raw_v.trim())
                .map_err(|msg| (AgentDiagnosticKind::InvalidModel, msg.to_owned()))?;
            apply_model_field(&mut cfg, &mut seen_keys, k, &v)?;
        }
        return Ok((cfg, idx));
    }

    if !trimmed.is_empty() {
        return Err((
            AgentDiagnosticKind::InvalidModel,
            "frontmatter 'model' must be a nested mapping".into(),
        ));
    }

    let mut base_indent: Option<usize> = None;
    while idx < lines.len() {
        let next = lines[idx];
        let next_trimmed = next.trim();
        if next_trimmed.is_empty() || next_trimmed.starts_with('#') {
            idx += 1;
            continue;
        }
        let indent = next
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        if indent == 0 {
            break;
        }
        match base_indent {
            None => base_indent = Some(indent),
            Some(expected) if indent != expected => {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    format!(
                        "inconsistent indentation in 'model' map at line {}",
                        idx + 2
                    ),
                ));
            }
            Some(_) => {}
        }
        let Some((raw_k, raw_v)) = next_trimmed.split_once(':') else {
            return Err((
                AgentDiagnosticKind::InvalidModel,
                format!("expected key: value in 'model' map at line {}", idx + 2),
            ));
        };
        let k = raw_k.trim();
        idx += 1;
        let (val, next_idx) = parse_scalar_value(raw_v, lines, idx, false, indent + 1)?
            .ok_or_else(|| {
                (
                    AgentDiagnosticKind::InvalidModel,
                    format!("model.{k} must have a scalar value"),
                )
            })?;
        idx = next_idx;
        apply_model_field(&mut cfg, &mut seen_keys, k, &val)?;
    }

    if seen_keys.is_empty() {
        return Err((
            AgentDiagnosticKind::InvalidModel,
            "frontmatter 'model' mapping must contain at least one field".into(),
        ));
    }

    Ok((cfg, idx))
}

fn apply_model_field(
    cfg: &mut AgentModelConfig,
    seen: &mut HashSet<String>,
    key: &str,
    value: &str,
) -> Result<(), (AgentDiagnosticKind, String)> {
    if !seen.insert(key.to_owned()) {
        return Err((
            AgentDiagnosticKind::InvalidModel,
            format!("duplicate key 'model.{key}'"),
        ));
    }
    match key {
        "profile" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    "model.profile must not be blank".into(),
                ));
            }
            cfg.profile = Some(trimmed.to_owned());
        }
        "id" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    "model.id must not be blank".into(),
                ));
            }
            cfg.id = Some(trimmed.to_owned());
        }
        "reasoning_effort" => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    "model.reasoning_effort must not be blank".into(),
                ));
            }
            cfg.reasoning_effort = Some(trimmed.to_owned());
        }
        "temperature" => {
            let parsed = value.trim().parse::<f32>().map_err(|_| {
                (
                    AgentDiagnosticKind::InvalidModel,
                    format!("invalid model.temperature: {value:?}"),
                )
            })?;
            let temp = Temperature::new(parsed)
                .map_err(|err| (AgentDiagnosticKind::InvalidModel, err.to_string()))?;
            cfg.temperature = Some(temp);
        }
        "max_tokens" => {
            let parsed = value.trim().parse::<u32>().map_err(|_| {
                (
                    AgentDiagnosticKind::InvalidModel,
                    format!("invalid model.max_tokens: {value:?}"),
                )
            })?;
            if parsed == 0 {
                return Err((
                    AgentDiagnosticKind::InvalidModel,
                    "model.max_tokens must be positive".into(),
                ));
            }
            cfg.max_tokens = Some(parsed);
        }
        unknown => {
            return Err((
                AgentDiagnosticKind::InvalidModel,
                format!("unknown key 'model.{unknown}'"),
            ));
        }
    }
    Ok(())
}

fn parse_scalar_value(
    raw_rest: &str,
    lines: &[&str],
    mut idx: usize,
    allow_unquoted_colon_fallback: bool,
    min_continuation_indent: usize,
) -> Result<Option<(String, usize)>, (AgentDiagnosticKind, String)> {
    let trimmed = raw_rest.trim();

    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
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
            let indent = next
                .bytes()
                .take_while(|b| *b == b' ' || *b == b'\t')
                .count();
            if indent >= min_continuation_indent {
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
        return Ok(Some((joined, idx)));
    }

    if trimmed.starts_with('"') {
        let parsed = parse_double_quoted(trimmed).map_err(|msg| {
            (
                AgentDiagnosticKind::InvalidFrontmatter,
                format!("invalid double-quoted frontmatter value: {msg}"),
            )
        })?;
        return Ok(Some((parsed, idx)));
    }

    if trimmed.starts_with('\'') {
        let parsed = parse_single_quoted(trimmed).map_err(|msg| {
            (
                AgentDiagnosticKind::InvalidFrontmatter,
                format!("invalid single-quoted frontmatter value: {msg}"),
            )
        })?;
        return Ok(Some((parsed, idx)));
    }

    let mut parts = vec![trimmed.to_owned()];
    while idx < lines.len() {
        let next = lines[idx];
        if next.trim().is_empty() {
            break;
        }
        let indent = next
            .bytes()
            .take_while(|b| *b == b' ' || *b == b'\t')
            .count();
        if indent >= min_continuation_indent {
            parts.push(next.trim().to_owned());
            idx += 1;
        } else {
            break;
        }
    }
    let combined = parts.join(" ");
    if !allow_unquoted_colon_fallback && let Some((before, _)) = combined.split_once(" #") {
        return Ok(Some((before.trim_end().to_owned(), idx)));
    }
    Ok(Some((combined, idx)))
}

fn parse_inline_scalar(raw: &str) -> Result<String, &'static str> {
    if raw.is_empty() {
        return Err("empty list or map scalar");
    }
    if raw.starts_with('"') {
        return parse_double_quoted(raw);
    }
    if raw.starts_with('\'') {
        return parse_single_quoted(raw);
    }
    Ok(raw.to_owned())
}

fn strip_inline_comment(input: &str) -> &str {
    if input.starts_with('#') {
        return "";
    }
    if let Some((before, _)) = input.split_once(" #") {
        before.trim_end()
    } else {
        input
    }
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

fn validate_name(name: &str) -> Result<(), (AgentDiagnosticKind, String)> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err((
            AgentDiagnosticKind::InvalidName,
            format!("agent name must be 1..={MAX_NAME_LEN} characters"),
        ));
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return Err((
            AgentDiagnosticKind::InvalidName,
            format!("agent name {name:?} must not start/end with '-' or contain '--'"),
        ));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err((
            AgentDiagnosticKind::InvalidName,
            format!("agent name {name:?} may only contain [a-z0-9-]"),
        ));
    }
    Ok(())
}

fn validate_description(description: &str) -> Result<String, (AgentDiagnosticKind, String)> {
    let trimmed = description.trim();
    let char_count = trimmed.chars().count();
    if char_count == 0 {
        return Err((
            AgentDiagnosticKind::InvalidDescription,
            "agent description must not be empty".into(),
        ));
    }
    if char_count > MAX_DESCRIPTION_CHARS {
        return Err((
            AgentDiagnosticKind::InvalidDescription,
            format!("agent description exceeds {MAX_DESCRIPTION_CHARS} characters ({char_count})"),
        ));
    }
    Ok(trimmed.to_owned())
}
