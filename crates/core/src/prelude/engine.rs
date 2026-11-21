use super::budget::{
    DynamicDraft, RenderedSections, apply_budget, effective_budget_bytes, push_xml_escaped,
};
use super::contract::{
    BASE_HOST_CONTRACT, DynamicSection, PRELUDE_BASE_SOURCE, PRELUDE_STATIC_CUSTOM_SOURCE,
    dynamic_section_from_prefix_source, dynamic_section_from_update_source,
};
use super::types::{
    CapabilityCategory, DynamicCustomSlot, DynamicUpdate, InitialPrelude, PreludeBudgetConfig,
    PreludeDefinition, PreludeDiagnostic, PreludeDiagnosticKind, PreludeError, PreludeRuntimeState,
};
use crate::permission::escape_untrusted_text;
use crate::skills::{SkillDiscovery, SkillEntry, SkillRegistry};
use causa::kernel::{BlockContent, BlockId, BlockMeta, ContentPart, ContextBlock, Role};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

const MAX_PRELUDE_FILE_BYTES: usize = 1024 * 1024;

/// Builds the initial static (`Role::System`) and dynamic (`Role::User`) prelude
/// blocks for a new session or transient invocation.
///
/// Strict mode is used for file reads: absent default `AGENTS.md` files are
/// silently skipped, whereas an unreadable, non-UTF-8, oversized, or non-file
/// `AGENTS.md` or explicit `DynamicCustomSlot::File` fails immediately.
/// Individual invalid `SKILL.md` entries are skipped with structured diagnostics.
pub fn build_initial_prelude(
    definition: &PreludeDefinition,
    state: &PreludeRuntimeState,
    budget: &PreludeBudgetConfig,
) -> Result<InitialPrelude, PreludeError> {
    let static_blocks = build_static_blocks(definition);
    let static_bytes = blocks_text_bytes(&static_blocks);
    let effective_budget = effective_budget_bytes(budget);

    let (registry, diagnostics) = discover_skills_for_prelude(definition, state);
    let skills: Vec<SkillEntry> = registry.iter().cloned().collect();

    let user_instructions = read_optional_agents_md_strict(
        definition.config_dir.as_deref(),
        DynamicSection::UserInstructions,
    )?;
    let workspace_instructions = read_optional_agents_md_strict(
        state.workspace_root.as_deref(),
        DynamicSection::WorkspaceInstructions,
    )?;
    let dynamic_custom = match &definition.dynamic_custom {
        Some(DynamicCustomSlot::Inline(text)) => Some(sanitize_instruction_text(
            text,
            DynamicSection::DynamicCustom.xml_tag(),
        )),
        Some(DynamicCustomSlot::File(path)) => read_explicit_dynamic_file_strict(path)?,
        None => None,
    };

    let env_body = render_env_body(state, definition.skills_enabled && !skills.is_empty());
    let draft = DynamicDraft {
        user_instructions,
        workspace_instructions,
        skills,
        env: Some(env_body),
        dynamic_custom,
    };

    let (rendered, final_budget) = tighten_to_window(
        &draft,
        &static_blocks,
        static_bytes,
        effective_budget,
        budget,
    );

    let mut blocks = static_blocks;
    for sec in DynamicSection::ALL {
        if let Some(text) = rendered_section_text(&rendered, sec) {
            blocks.push(make_block(Role::User, sec.prefix_source(), text));
        }
    }

    Ok(InitialPrelude {
        blocks,
        registry,
        diagnostics,
        effective_budget_bytes: final_budget,
    })
}

/// Derives each [`DynamicSection`]'s latest effective canonical text from `existing_blocks`.
///
/// Returns `None` for sections that were never emitted or whose latest block is
/// a `removed="true"` tombstone.
pub fn effective_section_contents(
    existing_blocks: &[ContextBlock],
) -> BTreeMap<DynamicSection, Option<String>> {
    let mut map: BTreeMap<DynamicSection, Option<String>> = BTreeMap::new();
    for sec in DynamicSection::ALL {
        map.insert(sec, None);
    }

    for block in existing_blocks {
        let Some(source) = block.meta().source.as_deref() else {
            continue;
        };
        let Some(sec) = dynamic_section_from_prefix_source(source)
            .or_else(|| dynamic_section_from_update_source(source))
        else {
            continue;
        };
        let Some(text) = extract_text(block) else {
            continue;
        };
        map.insert(sec, canonicalize_section_block_text(sec, &text));
    }

    map
}

/// Evaluates current dynamic prelude state in tolerant mode and emits incremental
/// `tars:prelude:update:*` `Role::User` blocks only for sections that changed,
/// were newly added, or were removed compared to `existing_blocks`.
pub fn evaluate_dynamic_updates(
    existing_blocks: &[ContextBlock],
    definition: &PreludeDefinition,
    state: &PreludeRuntimeState,
    budget: &PreludeBudgetConfig,
) -> DynamicUpdate {
    let previous = effective_section_contents(existing_blocks);
    let (static_blocks, static_bytes) = leading_static_blocks(existing_blocks, definition);
    let effective_budget = effective_budget_bytes(budget);

    let (registry, mut diagnostics) = discover_skills_for_prelude(definition, state);
    let skills: Vec<SkillEntry> = registry.iter().cloned().collect();

    let mut keep_previous = [false; 5];

    let user_instructions = match read_optional_agents_md_tolerant(
        definition.config_dir.as_deref(),
        DynamicSection::UserInstructions,
        &mut diagnostics,
    ) {
        Ok(val) => val,
        Err(()) => {
            keep_previous[section_index(DynamicSection::UserInstructions)] = true;
            previous
                .get(&DynamicSection::UserInstructions)
                .and_then(|v| v.as_deref())
                .and_then(|t| strip_outer_tag(DynamicSection::UserInstructions, t))
        }
    };

    let workspace_instructions = match read_optional_agents_md_tolerant(
        state.workspace_root.as_deref(),
        DynamicSection::WorkspaceInstructions,
        &mut diagnostics,
    ) {
        Ok(val) => val,
        Err(()) => {
            keep_previous[section_index(DynamicSection::WorkspaceInstructions)] = true;
            previous
                .get(&DynamicSection::WorkspaceInstructions)
                .and_then(|v| v.as_deref())
                .and_then(|t| strip_outer_tag(DynamicSection::WorkspaceInstructions, t))
        }
    };

    let dynamic_custom = match &definition.dynamic_custom {
        Some(DynamicCustomSlot::Inline(text)) => Some(sanitize_instruction_text(
            text,
            DynamicSection::DynamicCustom.xml_tag(),
        )),
        Some(DynamicCustomSlot::File(path)) => match read_explicit_dynamic_file_tolerant(
            path,
            DynamicSection::DynamicCustom,
            &mut diagnostics,
        ) {
            Ok(val) => val,
            Err(()) => {
                keep_previous[section_index(DynamicSection::DynamicCustom)] = true;
                previous
                    .get(&DynamicSection::DynamicCustom)
                    .and_then(|v| v.as_deref())
                    .and_then(|t| strip_outer_tag(DynamicSection::DynamicCustom, t))
            }
        },
        None => None,
    };

    let env_body = render_env_body(state, definition.skills_enabled && !skills.is_empty());
    let draft = DynamicDraft {
        user_instructions,
        workspace_instructions,
        skills,
        env: Some(env_body),
        dynamic_custom,
    };

    let (rendered, _) = tighten_to_window(
        &draft,
        &static_blocks,
        static_bytes,
        effective_budget,
        budget,
    );

    let mut update_blocks = Vec::new();
    let mut skills_changed = false;

    for sec in DynamicSection::ALL {
        if keep_previous[section_index(sec)] {
            continue;
        }
        let prev = previous.get(&sec).and_then(|v| v.as_deref());
        let curr = rendered_section_text(&rendered, sec);
        match (prev, curr) {
            (None, None) => {}
            (Some(p), Some(c)) if p == c => {}
            (Some(_), None) => {
                if sec == DynamicSection::Skills {
                    skills_changed = true;
                }
                update_blocks.push(make_block(
                    Role::User,
                    sec.update_source(),
                    removal_marker(sec),
                ));
            }
            (_, Some(c)) => {
                if sec == DynamicSection::Skills {
                    skills_changed = true;
                }
                update_blocks.push(make_block(
                    Role::User,
                    sec.update_source(),
                    mark_updated_tag(sec, &c),
                ));
            }
        }
    }

    DynamicUpdate {
        blocks: update_blocks,
        registry,
        diagnostics,
        skills_changed,
    }
}

fn section_index(sec: DynamicSection) -> usize {
    match sec {
        DynamicSection::UserInstructions => 0,
        DynamicSection::WorkspaceInstructions => 1,
        DynamicSection::Skills => 2,
        DynamicSection::Env => 3,
        DynamicSection::DynamicCustom => 4,
    }
}

fn build_static_blocks(definition: &PreludeDefinition) -> Vec<ContextBlock> {
    let mut blocks = vec![make_block(
        Role::System,
        PRELUDE_BASE_SOURCE,
        BASE_HOST_CONTRACT.to_owned(),
    )];
    if let Some(custom) = &definition.static_custom
        && !custom.is_empty()
    {
        blocks.push(make_block(
            Role::System,
            PRELUDE_STATIC_CUSTOM_SOURCE,
            custom.0.clone(),
        ));
    }
    blocks
}

fn leading_static_blocks(
    existing_blocks: &[ContextBlock],
    definition: &PreludeDefinition,
) -> (Vec<ContextBlock>, usize) {
    let mut static_blocks = Vec::new();
    for block in existing_blocks {
        let is_sys = matches!(
            block.content(),
            BlockContent::Message {
                role: Role::System,
                ..
            }
        );
        if is_sys {
            static_blocks.push(block.clone());
        } else if !matches!(block.content(), BlockContent::ToolChange(_)) {
            break;
        }
    }
    if static_blocks.is_empty() {
        static_blocks = build_static_blocks(definition);
    }
    let bytes = blocks_text_bytes(&static_blocks);
    (static_blocks, bytes)
}

fn blocks_text_bytes(blocks: &[ContextBlock]) -> usize {
    blocks
        .iter()
        .filter_map(extract_text)
        .map(|s| s.len())
        .sum()
}

fn tighten_to_window(
    draft: &DynamicDraft,
    static_blocks: &[ContextBlock],
    static_bytes: usize,
    initial_budget: usize,
    budget_config: &PreludeBudgetConfig,
) -> (RenderedSections, usize) {
    let mut current_budget = initial_budget;
    let mut rendered = apply_budget(draft, static_bytes, current_budget);

    let Some(window) = budget_config.context_window else {
        return (rendered, current_budget);
    };

    let trigger_line =
        u64::try_from(u128::from(window) * u128::from(budget_config.compact_threshold) / 100)
            .unwrap_or(u64::MAX);
    let half_line_tokens = trigger_line / 2;

    for _ in 0..4 {
        let mut candidate_blocks = static_blocks.to_vec();
        for sec in DynamicSection::ALL {
            if let Some(text) = rendered_section_text(&rendered, sec) {
                candidate_blocks.push(make_block(Role::User, sec.prefix_source(), text));
            }
        }
        let serialized_bytes: u64 = candidate_blocks
            .iter()
            .map(|b| {
                serde_json::to_vec(b)
                    .map(|v| u64::try_from(v.len()).unwrap_or(u64::MAX))
                    .unwrap_or(u64::MAX)
            })
            .sum();
        let total_tokens = serialized_bytes
            .div_ceil(3)
            .saturating_add(budget_config.folded_tool_declaration_estimate);
        if total_tokens <= half_line_tokens {
            break;
        }
        let excess_tokens = total_tokens - half_line_tokens;
        let step_down = usize::try_from(excess_tokens.saturating_mul(3).saturating_add(64))
            .unwrap_or(usize::MAX);
        let next_budget = current_budget.saturating_sub(step_down);
        if next_budget == current_budget {
            break;
        }
        current_budget = next_budget;
        rendered = apply_budget(draft, static_bytes, current_budget);
    }

    (rendered, current_budget)
}

fn discover_skills_for_prelude(
    definition: &PreludeDefinition,
    state: &PreludeRuntimeState,
) -> (SkillRegistry, Vec<PreludeDiagnostic>) {
    if !definition.skills_enabled {
        return (SkillRegistry::new(), Vec::new());
    }
    let discovery = SkillDiscovery::discover(
        &definition.skill_dirs,
        state.workspace_root.as_deref(),
        definition.include_workspace_skills,
    );
    let diagnostics = discovery
        .diagnostics
        .into_iter()
        .map(PreludeDiagnostic::from)
        .collect();
    (discovery.registry, diagnostics)
}

fn read_optional_agents_md_strict(
    dir: Option<&Path>,
    section: DynamicSection,
) -> Result<Option<String>, PreludeError> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let path = dir.join("AGENTS.md");
    match read_text_file(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(sanitize_instruction_text(&text, section.xml_tag())))
            }
        }
        Err(FileReadFailure::NotFound) => Ok(None),
        Err(err) => Err(err.into_error(path)),
    }
}

fn read_explicit_dynamic_file_strict(path: &Path) -> Result<Option<String>, PreludeError> {
    match read_text_file(path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(sanitize_instruction_text(
                    &text,
                    DynamicSection::DynamicCustom.xml_tag(),
                )))
            }
        }
        Err(err) => Err(err.into_error(path.to_owned())),
    }
}
enum FileReadFailure {
    NotFound,
    Io(io::ErrorKind),
    NotAFile,
    TooLarge,
    InvalidUtf8,
}

impl FileReadFailure {
    fn into_error(self, path: std::path::PathBuf) -> PreludeError {
        match self {
            Self::NotFound => PreludeError::Io {
                path,
                kind: io::ErrorKind::NotFound,
            },
            Self::Io(kind) => PreludeError::Io { path, kind },
            Self::NotAFile => PreludeError::NotAFile { path },
            Self::TooLarge => PreludeError::TooLarge {
                path,
                limit: MAX_PRELUDE_FILE_BYTES,
            },
            Self::InvalidUtf8 => PreludeError::InvalidUtf8 { path },
        }
    }

    fn into_diagnostic(
        self,
        section: DynamicSection,
        path: std::path::PathBuf,
    ) -> PreludeDiagnostic {
        match self {
            Self::NotFound => PreludeDiagnostic {
                section: Some(section),
                path,
                kind: PreludeDiagnosticKind::IoError,
                message: "file not found".into(),
            },
            Self::Io(kind) => PreludeDiagnostic {
                section: Some(section),
                path,
                kind: PreludeDiagnosticKind::IoError,
                message: format!("I/O error reading prelude file: {kind:?}"),
            },
            Self::NotAFile => PreludeDiagnostic {
                section: Some(section),
                path,
                kind: PreludeDiagnosticKind::NotAFile,
                message: "prelude path is not a regular file".into(),
            },
            Self::TooLarge => PreludeDiagnostic {
                section: Some(section),
                path,
                kind: PreludeDiagnosticKind::TooLarge,
                message: format!("prelude file exceeds {MAX_PRELUDE_FILE_BYTES} bytes"),
            },
            Self::InvalidUtf8 => PreludeDiagnostic {
                section: Some(section),
                path,
                kind: PreludeDiagnosticKind::InvalidUtf8,
                message: "prelude file is not valid UTF-8".into(),
            },
        }
    }
}

fn read_text_file(path: &Path) -> Result<String, FileReadFailure> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(FileReadFailure::NotFound);
        }
        Err(error) => return Err(FileReadFailure::Io(error.kind())),
    };
    if !meta.is_file() {
        return Err(FileReadFailure::NotAFile);
    }
    let file = File::open(path).map_err(|e| FileReadFailure::Io(e.kind()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PRELUDE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| FileReadFailure::Io(e.kind()))?;
    if bytes.len() > MAX_PRELUDE_FILE_BYTES {
        return Err(FileReadFailure::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| FileReadFailure::InvalidUtf8)
}

fn read_optional_agents_md_tolerant(
    dir: Option<&Path>,
    section: DynamicSection,
    diagnostics: &mut Vec<PreludeDiagnostic>,
) -> Result<Option<String>, ()> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let path = dir.join("AGENTS.md");
    match read_text_file(&path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(sanitize_instruction_text(&text, section.xml_tag())))
            }
        }
        Err(FileReadFailure::NotFound) => Ok(None),
        Err(err) => {
            diagnostics.push(err.into_diagnostic(section, path));
            Err(())
        }
    }
}

fn read_explicit_dynamic_file_tolerant(
    path: &Path,
    section: DynamicSection,
    diagnostics: &mut Vec<PreludeDiagnostic>,
) -> Result<Option<String>, ()> {
    match read_text_file(path) {
        Ok(text) => {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(sanitize_instruction_text(&text, section.xml_tag())))
            }
        }
        // Runtime deletion of dynamic_context_file produces a removal block (`None`).
        Err(FileReadFailure::NotFound) => Ok(None),
        Err(err) => {
            diagnostics.push(err.into_diagnostic(section, path.to_owned()));
            Err(())
        }
    }
}

fn sanitize_instruction_text(raw: &str, outer_tag: &str) -> String {
    let escaped = escape_untrusted_text(raw);
    let closing = format!("</{outer_tag}");
    neutralize_case_insensitive_closing_tag(&escaped, &closing)
}

fn neutralize_case_insensitive_closing_tag(input: &str, needle_lower: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if !lower.contains(needle_lower) {
        return input.to_owned();
    }
    let mut out = String::with_capacity(input.len() + 8);
    let mut pos = 0usize;
    while let Some(rel) = lower[pos..].find(needle_lower) {
        let abs = pos + rel;
        // `abs` is safe on UTF-8 boundary because `needle_lower` is ASCII,
        // wait: `.to_ascii_lowercase()` preserves byte length!
        out.push_str(&input[pos..abs]);
        out.push_str("&lt;");
        out.push_str(&input[abs + 1..abs + needle_lower.len()]);
        pos = abs + needle_lower.len();
    }
    out.push_str(&input[pos..]);
    out
}

fn render_env_body(state: &PreludeRuntimeState, skills_active: bool) -> String {
    let mut caps = state.capabilities.clone();
    if skills_active {
        caps.insert(CapabilityCategory::Skills);
    } else {
        caps.remove(&CapabilityCategory::Skills);
    }
    let caps_text = if caps.is_empty() {
        "(none)".to_owned()
    } else {
        caps.iter()
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut ws_escaped = String::new();
    match &state.workspace_root {
        Some(root) => push_xml_escaped(&mut ws_escaped, &root.to_string_lossy()),
        None => ws_escaped.push_str("(none)"),
    }
    let mut platform_escaped = String::new();
    push_xml_escaped(&mut platform_escaped, &state.platform);
    format!(
        "platform: {platform_escaped}\nworkspace_root: {ws_escaped}\npermission_mode: {}\ncapabilities: {caps_text}",
        state.permission_mode.as_str()
    )
}

fn rendered_section_text(rendered: &RenderedSections, sec: DynamicSection) -> Option<String> {
    match sec {
        DynamicSection::UserInstructions => rendered.user_instructions.clone(),
        DynamicSection::WorkspaceInstructions => rendered.workspace_instructions.clone(),
        DynamicSection::Skills => rendered.skills.clone(),
        DynamicSection::Env => rendered.env.clone(),
        DynamicSection::DynamicCustom => rendered.dynamic_custom.clone(),
    }
}

fn removal_marker(sec: DynamicSection) -> String {
    format!("<{} removed=\"true\" />", sec.xml_tag())
}

fn mark_updated_tag(sec: DynamicSection, canonical_xml: &str) -> String {
    let open_prefix = format!("<{}", sec.xml_tag());
    if let Some(rest) = canonical_xml.strip_prefix(&open_prefix) {
        format!("{open_prefix} updated=\"true\"{rest}")
    } else {
        canonical_xml.to_owned()
    }
}

fn canonicalize_section_block_text(sec: DynamicSection, text: &str) -> Option<String> {
    if text == removal_marker(sec) {
        return None;
    }
    let updated_with_space = format!("<{} updated=\"true\" ", sec.xml_tag());
    let updated_with_close = format!("<{} updated=\"true\">", sec.xml_tag());
    if let Some(rest) = text.strip_prefix(&updated_with_space) {
        Some(format!("<{} {rest}", sec.xml_tag()))
    } else if let Some(rest) = text.strip_prefix(&updated_with_close) {
        Some(format!("<{}>{rest}", sec.xml_tag()))
    } else {
        Some(text.to_owned())
    }
}

fn strip_outer_tag(sec: DynamicSection, canonical_xml: &str) -> Option<String> {
    let open = format!("<{}>\n", sec.xml_tag());
    let close = format!("\n</{}>", sec.xml_tag());
    canonical_xml
        .strip_prefix(&open)
        .and_then(|s| s.strip_suffix(&close))
        .map(str::to_owned)
}

fn extract_text(block: &ContextBlock) -> Option<String> {
    let BlockContent::Message { parts, .. } = block.content() else {
        return None;
    };
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text(text) = part {
            out.push_str(&text.0);
        }
    }
    Some(out)
}

fn make_block(role: Role, source: &str, text: String) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::text(role, text),
        BlockMeta::from_source(source),
    )
}
