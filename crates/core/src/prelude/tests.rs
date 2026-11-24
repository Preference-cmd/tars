use super::*;
use crate::permission::PermissionMode;
use causa::kernel::{BlockContent, ContentPart, ContextBlock, Role};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

fn block_text(block: &ContextBlock) -> &str {
    match block.content() {
        BlockContent::Message { parts, .. } => match &parts[0] {
            ContentPart::Text(t) => &t.0,
            _ => panic!("expected text part"),
        },
        _ => panic!("expected message block"),
    }
}

fn block_role(block: &ContextBlock) -> Role {
    match block.content() {
        BlockContent::Message { role, .. } => *role,
        _ => panic!("expected message block"),
    }
}

fn write_skill(root: &Path, name: &str, desc: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {desc}\n---\n# {name}\n"),
    )
    .unwrap();
}

#[test]
fn base_contract_and_source_predicates_match_spec() {
    assert!(
        (1500..=2500).contains(&BASE_HOST_CONTRACT.len()),
        "BASE_HOST_CONTRACT length {} outside 1.5..2.5 KiB",
        BASE_HOST_CONTRACT.len()
    );
    assert!(!BASE_HOST_CONTRACT.contains("current_permission_mode"));
    assert!(!BASE_HOST_CONTRACT.contains("read-only mode active"));

    assert!(is_prelude_prefix_source(PRELUDE_BASE_SOURCE));
    assert!(is_prelude_prefix_source(PRELUDE_STATIC_CUSTOM_SOURCE));
    assert!(!is_prelude_update_source(PRELUDE_BASE_SOURCE));
    assert!(is_any_prelude_source(PRELUDE_BASE_SOURCE));

    for sec in DynamicSection::ALL {
        assert!(is_prelude_prefix_source(sec.prefix_source()));
        assert!(!is_prelude_update_source(sec.prefix_source()));
        assert!(is_prelude_update_source(sec.update_source()));
        assert!(!is_prelude_prefix_source(sec.update_source()));
        assert!(is_any_prelude_source(sec.prefix_source()));
        assert!(is_any_prelude_source(sec.update_source()));
        assert_eq!(
            dynamic_section_from_prefix_source(sec.prefix_source()),
            Some(sec)
        );
        assert_eq!(
            dynamic_section_from_update_source(sec.update_source()),
            Some(sec)
        );
    }

    assert!(!is_any_prelude_source("tars:compaction"));
    assert!(!is_any_prelude_source("user"));
    assert!(!is_prelude_prefix_source("tars:prelude:unknown"));
    assert!(!is_prelude_update_source("tars:prelude:update:unknown"));
}

#[test]
fn initial_prelude_orders_static_then_dynamic_omits_empty_and_is_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let cfg_dir = temp.path().join("cfg");
    let ws_dir = temp.path().join("ws");
    let dyn_file = temp.path().join("dyn.txt");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(ws_dir.join(".tars/skills")).unwrap();

    fs::write(cfg_dir.join("AGENTS.md"), "User rule: stay concise.").unwrap();
    fs::write(ws_dir.join("AGENTS.md"), "Workspace rule: run tests.").unwrap();
    fs::write(&dyn_file, "Current sprint: SI-P1").unwrap();
    write_skill(
        &ws_dir.join(".tars/skills"),
        "beta-skill",
        "Second alphabetically",
    );
    write_skill(
        &ws_dir.join(".tars/skills"),
        "alpha-skill",
        "First <alphabetically> & safe",
    );

    let def = PreludeDefinition {
        static_custom: Some(StaticCustomSlot::new("Static custom rules")),
        dynamic_custom: Some(DynamicCustomSlot::File(dyn_file)),
        config_dir: Some(cfg_dir),
        skill_dirs: Vec::new(),
        include_workspace_skills: true,
        skills_enabled: true,
        ..Default::default()
    };
    let mut caps = BTreeSet::new();
    caps.insert(CapabilityCategory::WorkspaceRead);
    caps.insert(CapabilityCategory::Recall);
    let state = PreludeRuntimeState {
        workspace_root: Some(ws_dir),
        permission_mode: PermissionMode::Ask,
        capabilities: caps,
        platform: "linux".into(),
    };
    let budget = PreludeBudgetConfig::default();

    let first = build_initial_prelude(&def, &state, &budget).unwrap();
    let second = build_initial_prelude(&def, &state, &budget).unwrap();

    let sources: Vec<(&str, Role)> = first
        .blocks
        .iter()
        .map(|b| (b.meta().source.as_deref().unwrap(), block_role(b)))
        .collect();
    assert_eq!(
        sources,
        vec![
            (PRELUDE_BASE_SOURCE, Role::System),
            (PRELUDE_STATIC_CUSTOM_SOURCE, Role::System),
            (PRELUDE_USER_INSTRUCTIONS_SOURCE, Role::User),
            (PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE, Role::User),
            (PRELUDE_SKILLS_SOURCE, Role::User),
            (PRELUDE_AGENTS_SOURCE, Role::User),
            (PRELUDE_ENV_SOURCE, Role::User),
            (PRELUDE_DYNAMIC_CUSTOM_SOURCE, Role::User),
        ]
    );

    // Byte-deterministic output across runs
    let texts_1: Vec<&str> = first.blocks.iter().map(block_text).collect();
    let texts_2: Vec<&str> = second.blocks.iter().map(block_text).collect();
    assert_eq!(texts_1, texts_2);

    // Skills sorted alphabetically and XML-escaped
    let skills_xml = block_text(&first.blocks[4]);
    let alpha_pos = skills_xml.find("<name>alpha-skill</name>").unwrap();
    let beta_pos = skills_xml.find("<name>beta-skill</name>").unwrap();
    assert!(alpha_pos < beta_pos);
    assert!(skills_xml.contains("First &lt;alphabetically&gt; &amp; safe"));

    // Agents block includes built-in explore & general
    let agents_xml = block_text(&first.blocks[5]);
    assert!(agents_xml.contains("<name>explore</name>"));
    assert!(agents_xml.contains("<name>general</name>"));

    // Env block includes platform, workspace_root, permission_mode, capabilities
    let env_xml = block_text(&first.blocks[6]);
    assert!(env_xml.contains("platform: linux"));
    assert!(env_xml.contains("permission_mode: ask"));
    assert!(env_xml.contains("capabilities: workspace_read, recall, skills"));
}

#[test]
fn zero_skills_and_empty_optional_sections_are_omitted() {
    let temp = tempfile::tempdir().unwrap();
    let ws_dir = temp.path().join("ws");
    fs::create_dir_all(&ws_dir).unwrap();

    let def = PreludeDefinition {
        agents_enabled: false,
        ..Default::default()
    };
    let state = PreludeRuntimeState {
        workspace_root: Some(ws_dir),
        permission_mode: PermissionMode::ReadOnly,
        capabilities: BTreeSet::new(),
        platform: "macos".into(),
    };
    let out = build_initial_prelude(&def, &state, &PreludeBudgetConfig::default()).unwrap();
    let sources: Vec<&str> = out
        .blocks
        .iter()
        .map(|b| b.meta().source.as_deref().unwrap())
        .collect();
    assert_eq!(sources, vec![PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE]);
    assert!(!block_text(&out.blocks[1]).contains("skills"));
}

#[test]
fn budget_degradation_order_utf8_safety_and_small_window_tightening() {
    let temp = tempfile::tempdir().unwrap();
    let cfg_dir = temp.path().join("cfg");
    let ws_dir = temp.path().join("ws");
    let skills_dir = ws_dir.join(".tars/skills");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::create_dir_all(&skills_dir).unwrap();

    // Multi-byte UTF-8 characters everywhere to test boundary safety
    let user_rules = "用户规则：".repeat(1200); // ~18 KiB
    let ws_rules = "工作区规则：".repeat(1200); // ~21 KiB
    fs::write(cfg_dir.join("AGENTS.md"), &user_rules).unwrap();
    fs::write(ws_dir.join("AGENTS.md"), &ws_rules).unwrap();

    for i in 0..30 {
        write_skill(
            &skills_dir,
            &format!("skill-{i:02}"),
            &"很长的技能描述内容用于触发预算降级 ".repeat(8),
        );
    }

    let dyn_text = "动态上下文：".repeat(600); // ~10.8 KiB
    let def = PreludeDefinition {
        static_custom: Some(StaticCustomSlot::new("Static custom note")),
        dynamic_custom: Some(DynamicCustomSlot::Inline(dyn_text)),
        config_dir: Some(cfg_dir),
        skill_dirs: Vec::new(),
        include_workspace_skills: true,
        skills_enabled: true,
        ..Default::default()
    };
    let state = PreludeRuntimeState {
        workspace_root: Some(ws_dir),
        permission_mode: PermissionMode::Ask,
        capabilities: BTreeSet::new(),
        platform: "linux".into(),
    };

    // 1. Default 24 KiB budget
    let out_default = build_initial_prelude(&def, &state, &PreludeBudgetConfig::default()).unwrap();
    let total_text_bytes: usize = out_default.blocks.iter().map(|b| block_text(b).len()).sum();
    assert!(
        total_text_bytes <= DEFAULT_TOTAL_BUDGET_BYTES,
        "total {total_text_bytes} > {DEFAULT_TOTAL_BUDGET_BYTES}"
    );
    // Base is never truncated
    assert_eq!(block_text(&out_default.blocks[0]), BASE_HOST_CONTRACT);
    // Skills degraded to name-only or tail-truncated
    let skills_block = out_default
        .blocks
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(PRELUDE_SKILLS_SOURCE))
        .unwrap();
    assert!(block_text(skills_block).contains("descriptions omitted"));

    // 2. Small 14K window with folded tool estimate: must fit <= trigger_line / 2
    let small_budget = PreludeBudgetConfig {
        context_window: Some(14_000),
        compact_threshold: 70,
        folded_tool_declaration_estimate: 600,
    };
    let out_small = build_initial_prelude(&def, &state, &small_budget).unwrap();
    assert_eq!(block_text(&out_small.blocks[0]), BASE_HOST_CONTRACT);

    let serialized_bytes: u64 = out_small
        .blocks
        .iter()
        .map(|b| u64::try_from(serde_json::to_vec(b).unwrap().len()).unwrap())
        .sum();
    let prefix_tokens =
        serialized_bytes.div_ceil(3) + small_budget.folded_tool_declaration_estimate;
    let trigger_line = 14_000 * 70 / 100;
    assert!(
        prefix_tokens <= trigger_line / 2,
        "prefix_tokens {prefix_tokens} exceeds half trigger_line {}",
        trigger_line / 2
    );
}

#[test]
fn dynamic_diff_emits_only_changes_handles_removal_tombstones_and_is_tolerant_at_runtime() {
    let temp = tempfile::tempdir().unwrap();
    let ws_dir = temp.path().join("ws");
    let dyn_file = temp.path().join("dyn.md");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("AGENTS.md"), "Rule v1").unwrap();
    fs::write(&dyn_file, "Dyn v1").unwrap();

    let def = PreludeDefinition {
        static_custom: None,
        dynamic_custom: Some(DynamicCustomSlot::File(dyn_file.clone())),
        config_dir: None,
        skill_dirs: Vec::new(),
        include_workspace_skills: true,
        skills_enabled: true,
        ..Default::default()
    };
    let mut state = PreludeRuntimeState {
        workspace_root: Some(ws_dir.clone()),
        permission_mode: PermissionMode::Ask,
        capabilities: BTreeSet::new(),
        platform: "linux".into(),
    };
    let budget = PreludeBudgetConfig::default();

    let initial = build_initial_prelude(&def, &state, &budget).unwrap();
    let mut blocks = initial.blocks;

    // Turn 2: nothing changed -> 0 update blocks
    let turn2 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert!(turn2.blocks.is_empty());
    assert!(turn2.diagnostics.is_empty());

    // Turn 3: change only permission mode -> only tars:prelude:update:env emitted
    state.permission_mode = PermissionMode::AcceptEdits;
    let turn3 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert_eq!(turn3.blocks.len(), 1);
    assert_eq!(
        turn3.blocks[0].meta().source.as_deref(),
        Some(DynamicSection::Env.update_source())
    );
    assert!(block_text(&turn3.blocks[0]).contains("<env updated=\"true\">"));
    assert!(block_text(&turn3.blocks[0]).contains("permission_mode: accept-edits"));
    blocks.extend(turn3.blocks);

    // Turn 4: unchanged after update -> 0 update blocks
    let turn4 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert!(turn4.blocks.is_empty());

    // Turn 5: delete workspace AGENTS.md and dynamic_context_file -> tombstones emitted once
    fs::remove_file(ws_dir.join("AGENTS.md")).unwrap();
    fs::remove_file(&dyn_file).unwrap();
    let turn5 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert_eq!(turn5.blocks.len(), 2);
    assert_eq!(
        block_text(&turn5.blocks[0]),
        "<workspace_instructions removed=\"true\" />"
    );
    assert_eq!(
        block_text(&turn5.blocks[1]),
        "<dynamic_context removed=\"true\" />"
    );
    blocks.extend(turn5.blocks);

    // Turn 6: still deleted -> 0 update blocks
    let turn6 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert!(turn6.blocks.is_empty());

    // Turn 7: recreate AGENTS.md with invalid UTF-8 -> tolerant mode keeps previous (absent) & logs diagnostic
    fs::write(ws_dir.join("AGENTS.md"), [0xff, 0xfe, 0x00]).unwrap();
    let turn7 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert!(turn7.blocks.is_empty());
    assert_eq!(turn7.diagnostics.len(), 1);
    assert_eq!(
        turn7.diagnostics[0].kind,
        PreludeDiagnosticKind::InvalidUtf8
    );

    // Turn 8: fix AGENTS.md -> updated="true" emitted again
    fs::write(ws_dir.join("AGENTS.md"), "Rule v2").unwrap();
    let turn8 = evaluate_dynamic_updates(&blocks, &def, &state, &budget);
    assert_eq!(turn8.blocks.len(), 1);
    assert!(block_text(&turn8.blocks[0]).contains("<workspace_instructions updated=\"true\">"));
    assert!(block_text(&turn8.blocks[0]).contains("Rule v2"));
}

#[test]
fn strict_creation_fails_on_invalid_utf8_or_missing_explicit_dynamic_file() {
    let temp = tempfile::tempdir().unwrap();
    let ws_dir = temp.path().join("ws");
    fs::create_dir_all(&ws_dir).unwrap();
    fs::write(ws_dir.join("AGENTS.md"), [0xff, 0xfe]).unwrap();

    let state = PreludeRuntimeState {
        workspace_root: Some(ws_dir),
        permission_mode: PermissionMode::Ask,
        capabilities: BTreeSet::new(),
        platform: "linux".into(),
    };
    let err = build_initial_prelude(
        &PreludeDefinition::default(),
        &state,
        &PreludeBudgetConfig::default(),
    )
    .unwrap_err();
    assert!(matches!(err, PreludeError::InvalidUtf8 { .. }));

    // Missing explicit dynamic file also errors in strict creation mode
    let missing_dyn = PreludeDefinition {
        dynamic_custom: Some(DynamicCustomSlot::File(temp.path().join("absent.md"))),
        ..Default::default()
    };
    let state_clean = PreludeRuntimeState {
        workspace_root: None,
        permission_mode: PermissionMode::Ask,
        capabilities: BTreeSet::new(),
        platform: "linux".into(),
    };
    assert!(
        build_initial_prelude(&missing_dyn, &state_clean, &PreludeBudgetConfig::default()).is_err()
    );
}
