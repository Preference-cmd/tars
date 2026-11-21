use super::types::PreludeBudgetConfig;
use crate::skills::SkillEntry;

/// Default unified byte budget across static and dynamic prelude sections (`24 KiB`).
pub const DEFAULT_TOTAL_BUDGET_BYTES: usize = 24 * 1024;

/// Shared soft byte cap for `user-instructions` + `workspace-instructions` (`16 KiB`).
pub const INSTRUCTIONS_SOFT_CAP_BYTES: usize = 16 * 1024;

/// Soft byte cap for `<available_skills>` (`4 KiB`).
pub const SKILLS_SOFT_CAP_BYTES: usize = 4 * 1024;

/// Soft byte cap for `dynamic-custom` (`4 KiB`).
pub const DYNAMIC_CUSTOM_SOFT_CAP_BYTES: usize = 4 * 1024;

/// Soft byte cap for `env` (`1 KiB`).
pub const ENV_SOFT_CAP_BYTES: usize = 1024;

/// Computes the effective prelude byte budget, tightening below [`DEFAULT_TOTAL_BUDGET_BYTES`]
/// when `context_window` is known so the fixed prefix stays safely under `trigger_line / 2`.
pub fn effective_budget_bytes(config: &PreludeBudgetConfig) -> usize {
    let Some(window) = config.context_window else {
        return DEFAULT_TOTAL_BUDGET_BYTES;
    };
    let trigger_line =
        u64::try_from(u128::from(window) * u128::from(config.compact_threshold) / 100)
            .unwrap_or(u64::MAX);
    let half_line_tokens = trigger_line / 2;
    let safety_margin_tokens = (half_line_tokens / 10).clamp(64, 512);
    let available_tokens = half_line_tokens
        .saturating_sub(config.folded_tool_declaration_estimate)
        .saturating_sub(safety_margin_tokens);
    let window_byte_budget =
        usize::try_from(available_tokens.saturating_mul(3)).unwrap_or(usize::MAX);
    DEFAULT_TOTAL_BUDGET_BYTES.min(window_byte_budget)
}

#[derive(Clone, Debug, Default)]
pub(super) struct DynamicDraft {
    pub(super) user_instructions: Option<String>,
    pub(super) workspace_instructions: Option<String>,
    pub(super) skills: Vec<SkillEntry>,
    pub(super) env: Option<String>,
    pub(super) dynamic_custom: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RenderedSections {
    pub(super) user_instructions: Option<String>,
    pub(super) workspace_instructions: Option<String>,
    pub(super) skills: Option<String>,
    pub(super) env: Option<String>,
    pub(super) dynamic_custom: Option<String>,
}

impl RenderedSections {
    pub(super) fn total_bytes(&self) -> usize {
        self.user_instructions.as_ref().map_or(0, String::len)
            + self.workspace_instructions.as_ref().map_or(0, String::len)
            + self.skills.as_ref().map_or(0, String::len)
            + self.env.as_ref().map_or(0, String::len)
            + self.dynamic_custom.as_ref().map_or(0, String::len)
    }
}

pub(super) fn apply_budget(
    draft: &DynamicDraft,
    static_bytes: usize,
    total_budget_bytes: usize,
) -> RenderedSections {
    let mut rendered = RenderedSections {
        user_instructions: draft
            .user_instructions
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|body| wrap_text_section("user_instructions", body)),
        workspace_instructions: draft
            .workspace_instructions
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|body| wrap_text_section("workspace_instructions", body)),
        skills: render_skills_full(&draft.skills),
        env: draft
            .env
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|body| wrap_text_section("env", body)),
        dynamic_custom: draft
            .dynamic_custom
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|body| wrap_text_section("dynamic_context", body)),
    };

    // 1. Apply per-section soft caps first.
    if let Some(ref text) = rendered.dynamic_custom
        && text.len() > DYNAMIC_CUSTOM_SOFT_CAP_BYTES
        && let Some(raw) = draft.dynamic_custom.as_deref()
    {
        rendered.dynamic_custom =
            shrink_wrapped_text_section("dynamic_context", raw, DYNAMIC_CUSTOM_SOFT_CAP_BYTES);
    }

    if let Some(ref text) = rendered.env
        && text.len() > ENV_SOFT_CAP_BYTES
        && let Some(raw) = draft.env.as_deref()
    {
        rendered.env = shrink_wrapped_text_section("env", raw, ENV_SOFT_CAP_BYTES);
    }

    // Shared 16 KiB soft cap for user-instructions + workspace-instructions:
    // degrade workspace-instructions first, then user-instructions.
    let instr_total = rendered.user_instructions.as_ref().map_or(0, String::len)
        + rendered
            .workspace_instructions
            .as_ref()
            .map_or(0, String::len);
    if instr_total > INSTRUCTIONS_SOFT_CAP_BYTES {
        let user_len = rendered.user_instructions.as_ref().map_or(0, String::len);
        if let Some(raw_ws) = draft.workspace_instructions.as_deref()
            && rendered.workspace_instructions.is_some()
        {
            let ws_target = INSTRUCTIONS_SOFT_CAP_BYTES.saturating_sub(user_len);
            rendered.workspace_instructions =
                shrink_wrapped_text_section("workspace_instructions", raw_ws, ws_target);
        }
        let after_ws = rendered.user_instructions.as_ref().map_or(0, String::len)
            + rendered
                .workspace_instructions
                .as_ref()
                .map_or(0, String::len);
        if after_ws > INSTRUCTIONS_SOFT_CAP_BYTES
            && let Some(raw_user) = draft.user_instructions.as_deref()
        {
            let ws_len = rendered
                .workspace_instructions
                .as_ref()
                .map_or(0, String::len);
            let user_target = INSTRUCTIONS_SOFT_CAP_BYTES.saturating_sub(ws_len);
            rendered.user_instructions =
                shrink_wrapped_text_section("user_instructions", raw_user, user_target);
        }
    }

    if let Some(ref text) = rendered.skills
        && text.len() > SKILLS_SOFT_CAP_BYTES
    {
        rendered.skills = shrink_skills(&draft.skills, SKILLS_SOFT_CAP_BYTES);
    }

    // 2. Enforce unified total budget in deterministic degradation order:
    //    dynamic-custom -> workspace-instructions -> user-instructions
    //    -> skills name-only -> skills tail-truncated.
    //    Static base is never truncated.
    let dynamic_budget = total_budget_bytes.saturating_sub(static_bytes);

    if rendered.total_bytes() > dynamic_budget
        && let Some(raw) = draft.dynamic_custom.as_deref()
        && rendered.dynamic_custom.is_some()
    {
        let others =
            rendered.total_bytes() - rendered.dynamic_custom.as_ref().map_or(0, String::len);
        let target = dynamic_budget.saturating_sub(others);
        rendered.dynamic_custom = shrink_wrapped_text_section("dynamic_context", raw, target);
    }

    if rendered.total_bytes() > dynamic_budget
        && let Some(raw) = draft.workspace_instructions.as_deref()
        && rendered.workspace_instructions.is_some()
    {
        let others = rendered.total_bytes()
            - rendered
                .workspace_instructions
                .as_ref()
                .map_or(0, String::len);
        let target = dynamic_budget.saturating_sub(others);
        rendered.workspace_instructions =
            shrink_wrapped_text_section("workspace_instructions", raw, target);
    }

    if rendered.total_bytes() > dynamic_budget
        && let Some(raw) = draft.user_instructions.as_deref()
        && rendered.user_instructions.is_some()
    {
        let others =
            rendered.total_bytes() - rendered.user_instructions.as_ref().map_or(0, String::len);
        let target = dynamic_budget.saturating_sub(others);
        rendered.user_instructions = shrink_wrapped_text_section("user_instructions", raw, target);
    }

    if rendered.total_bytes() > dynamic_budget && rendered.skills.is_some() {
        let others = rendered.total_bytes() - rendered.skills.as_ref().map_or(0, String::len);
        let target = dynamic_budget.saturating_sub(others);
        rendered.skills = shrink_skills(&draft.skills, target);
    }

    rendered
}

pub(super) fn wrap_text_section(tag: &str, body: &str) -> String {
    format!("<{tag}>\n{body}\n</{tag}>")
}

fn shrink_wrapped_text_section(tag: &str, raw_body: &str, max_bytes: usize) -> Option<String> {
    let full = wrap_text_section(tag, raw_body);
    if full.len() <= max_bytes {
        return Some(full);
    }
    let original_bytes = raw_body.len();
    let wrapper_overhead = tag.len() * 2 + 7; // `<tag>\n` + `\n</tag>`
    let min_notice = format!("[truncated; original size: {original_bytes} bytes]");
    if max_bytes < wrapper_overhead + min_notice.len() {
        return None;
    }
    let inner_budget = max_bytes - wrapper_overhead;
    let truncated_body = truncate_with_notice(raw_body, inner_budget);
    Some(wrap_text_section(tag, &truncated_body))
}

/// Truncates `text` on a valid UTF-8 character boundary to fit within `max_bytes`,
/// appending a notice stating the original byte count.
pub(super) fn truncate_with_notice(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let original_bytes = text.len();
    let notice = format!("\n\n[truncated; original size: {original_bytes} bytes]");
    if max_bytes <= notice.len() {
        let short = format!("[truncated; original size: {original_bytes} bytes]");
        let cut = short.floor_char_boundary(max_bytes);
        return short[..cut].to_owned();
    }
    let keep_budget = max_bytes - notice.len();
    let cut = text.floor_char_boundary(keep_budget);
    let mut out = text[..cut].to_owned();
    out.push_str(&notice);
    out
}

fn render_skills_full(skills: &[SkillEntry]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from("<available_skills>\n");
    for skill in skills {
        out.push_str("  <skill>\n    <name>");
        push_xml_escaped(&mut out, &skill.name);
        out.push_str("</name>\n    <description>");
        push_xml_escaped(&mut out, &skill.description);
        out.push_str("</description>\n  </skill>\n");
    }
    out.push_str("</available_skills>");
    Some(out)
}

fn render_skills_names_only(skills: &[SkillEntry], omitted_count: usize) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let note = if omitted_count == 0 {
        "descriptions omitted due to budget".to_owned()
    } else {
        format!("descriptions omitted; {omitted_count} more skills omitted due to budget")
    };
    let mut out = format!("<available_skills note=\"{note}\">\n");
    for skill in skills {
        out.push_str("  <skill><name>");
        push_xml_escaped(&mut out, &skill.name);
        out.push_str("</name></skill>\n");
    }
    out.push_str("</available_skills>");
    Some(out)
}

fn shrink_skills(skills: &[SkillEntry], max_bytes: usize) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    if let Some(full) = render_skills_full(skills)
        && full.len() <= max_bytes
    {
        return Some(full);
    }
    // Step 4: degrade to name-only list.
    if let Some(names_only) = render_skills_names_only(skills, 0)
        && names_only.len() <= max_bytes
    {
        return Some(names_only);
    }
    // Step 5: tail-truncate name-only list, recording omitted count.
    for keep in (1..skills.len()).rev() {
        let omitted = skills.len() - keep;
        if let Some(candidate) = render_skills_names_only(&skills[..keep], omitted)
            && candidate.len() <= max_bytes
        {
            return Some(candidate);
        }
    }
    None
}

pub(super) fn push_xml_escaped(out: &mut String, input: &str) {
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
}
