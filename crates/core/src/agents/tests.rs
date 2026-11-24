use super::*;
use crate::definition::{
    BuiltinTool, ChildNarrowingRequest, CredentialRef, Provider, SessionDefinition, Temperature,
    WorkspaceTool, WorkspaceToolLimits, narrow_child_definition,
};
use crate::events::AgentInteraction;
use crate::permission::PermissionMode;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn sample_parent_definition(root: &Path) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "parent-model".into(),
        endpoint: Some("https://parent.example.com/v1".into()),
        credential: CredentialRef::Environment("TARS_PARENT_KEY".into()),
        work_timeout_secs: Some(120),
        retained_work_capacity: 16,
        max_model_rounds: 8,
        max_tool_calls: 16,
        max_tokens: Some(2048),
        reasoning_effort: Some("medium".into()),
        temperature: Some(Temperature::new(0.4).unwrap()),
        extra_body: Default::default(),
        context_window: Some(64_000),
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        max_model_attempts: Some(2),
        render_options: {
            let mut m = serde_json::Map::new();
            m.insert("include_usage".into(), serde_json::json!(true));
            m
        },
        workspace_root: Some(root.to_path_buf()),
        tools: vec![WorkspaceTool::ReadFile, WorkspaceTool::ListDirectory],
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
        permission_mode: Some(PermissionMode::Ask),
        command: None,
        mcp_servers: Vec::new(),
        prelude: Default::default(),
        code_execution: None,
    }
}

#[test]
fn builtin_presets_include_explore_and_general() {
    let discovery = AgentDiscovery::discover_non_workspace(None, &[]);
    assert!(discovery.diagnostics.is_empty());
    assert_eq!(discovery.registry.len(), 2);

    let explore = discovery.registry.get("explore").expect("explore");
    assert_eq!(explore.source, AgentPresetSource::Builtin);
    assert!(explore.is_builtin_readonly());
    assert_eq!(explore.default_interaction, AgentInteraction::SingleWork);
    assert!(explore.role_prompt.is_some());
    assert_eq!(
        explore.tools.as_deref(),
        Some(
            [
                BuiltinTool::Workspace(WorkspaceTool::ReadFile),
                BuiltinTool::Workspace(WorkspaceTool::ListDirectory),
                BuiltinTool::Workspace(WorkspaceTool::SearchText),
                BuiltinTool::Recall,
                BuiltinTool::RunCode,
            ]
            .as_slice()
        )
    );

    let general = discovery.registry.get("general").expect("general");
    assert_eq!(general.source, AgentPresetSource::Builtin);
    assert!(!general.is_builtin_readonly());
    assert_eq!(general.tools, None);
    assert_eq!(general.default_interaction, AgentInteraction::SingleWork);
}

#[test]
fn frontmatter_parses_valid_block_and_inline_variants_with_trimmed_body() {
    let doc1 = concat!(
        "\u{feff}---\n",
        "name: code-reviewer\n",
        "description: \"Reviews diffs and tests: carefully\"\n",
        "tools:\n",
        "  - read_file\n",
        "  - search_text\n",
        "  - run_code\n",
        "model:\n",
        "  profile: fast\n",
        "  id: gpt-4o-mini\n",
        "  reasoning_effort: low\n",
        "  temperature: 0.25\n",
        "  max_tokens: 1500\n",
        "max_model_rounds: 6\n",
        "timeout_secs: 90\n",
        "default_interaction: reusable\n",
        "---\n\n  You are a careful code reviewer.  \n\n",
    );
    let parsed = frontmatter::parse_agent_document(doc1).unwrap();
    assert_eq!(parsed.meta.name, "code-reviewer");
    assert_eq!(
        parsed.meta.description,
        "Reviews diffs and tests: carefully"
    );
    assert_eq!(
        parsed.meta.tools,
        Some(vec![
            BuiltinTool::Workspace(WorkspaceTool::ReadFile),
            BuiltinTool::Workspace(WorkspaceTool::SearchText),
            BuiltinTool::RunCode,
        ])
    );
    let m = parsed.meta.model.unwrap();
    assert_eq!(m.profile.as_deref(), Some("fast"));
    assert_eq!(m.id.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(m.reasoning_effort.as_deref(), Some("low"));
    assert_eq!(m.temperature, Some(Temperature::new(0.25).unwrap()));
    assert_eq!(m.max_tokens, Some(1500));
    assert_eq!(parsed.meta.max_model_rounds, Some(6));
    assert_eq!(parsed.meta.timeout_secs, Some(90));
    assert_eq!(
        parsed.meta.default_interaction,
        Some(AgentInteraction::Reusable)
    );
    assert_eq!(
        parsed.role_prompt.as_deref(),
        Some("You are a careful code reviewer.")
    );

    let doc2 = "---\n\
        name: empty-tools\n\
        description: Minimal inline form\n\
        tools: [recall, load_skill]\n\
        model: { id: claude-haiku, temperature: 0 }\n\
        default_interaction: single_work\n\
        ---\n   \n";
    let parsed2 = frontmatter::parse_agent_document(doc2).unwrap();
    assert_eq!(
        parsed2.meta.tools,
        Some(vec![BuiltinTool::Recall, BuiltinTool::LoadSkill])
    );
    assert_eq!(
        parsed2.meta.model.unwrap().temperature,
        Some(Temperature::new(0.0).unwrap())
    );
    assert_eq!(parsed2.role_prompt, None);
}

#[test]
fn frontmatter_rejects_unknown_keys_duplicates_and_invalid_values() {
    let cases = [
        ("missing frontmatter", "no delimiter"),
        ("unterminated", "---\nname: a\ndescription: b\n"),
        (
            "unknown top key",
            "---\nname: a\ndescription: b\npermission_mode: full-access\n---\n",
        ),
        (
            "duplicate top key",
            "---\nname: a\nname: b\ndescription: b\n---\n",
        ),
        (
            "unknown model key",
            "---\nname: a\ndescription: b\nmodel:\n  endpoint: https://evil.example\n---\n",
        ),
        (
            "duplicate model key",
            "---\nname: a\ndescription: b\nmodel:\n  id: x\n  id: y\n---\n",
        ),
        (
            "invalid name uppercase",
            "---\nname: Bad\ndescription: b\n---\n",
        ),
        (
            "invalid name double dash",
            "---\nname: a--b\ndescription: b\n---\n",
        ),
        (
            "invalid name trailing dash",
            "---\nname: a-\ndescription: b\n---\n",
        ),
        (
            "empty description",
            "---\nname: a\ndescription: \"  \"\n---\n",
        ),
        (
            "unknown tool",
            "---\nname: a\ndescription: b\ntools: [wait_agent]\n---\n",
        ),
        (
            "duplicate tool",
            "---\nname: a\ndescription: b\ntools: [read_file, read_file]\n---\n",
        ),
        (
            "scalar tools",
            "---\nname: a\ndescription: b\ntools: read_file\n---\n",
        ),
        (
            "temperature out of range",
            "---\nname: a\ndescription: b\nmodel:\n  temperature: 2.5\n---\n",
        ),
        (
            "temperature negative",
            "---\nname: a\ndescription: b\nmodel:\n  temperature: -0.1\n---\n",
        ),
        (
            "zero max_tokens",
            "---\nname: a\ndescription: b\nmodel:\n  max_tokens: 0\n---\n",
        ),
        (
            "zero max_model_rounds",
            "---\nname: a\ndescription: b\nmax_model_rounds: 0\n---\n",
        ),
        (
            "zero timeout_secs",
            "---\nname: a\ndescription: b\ntimeout_secs: 0\n---\n",
        ),
        (
            "over max timeout_secs",
            "---\nname: a\ndescription: b\ntimeout_secs: 86401\n---\n",
        ),
        (
            "invalid default_interaction",
            "---\nname: a\ndescription: b\ndefault_interaction: forever\n---\n",
        ),
        (
            "blank model.id",
            "---\nname: a\ndescription: b\nmodel:\n  id: \"  \"\n---\n",
        ),
    ];
    for (label, raw) in cases {
        assert!(
            frontmatter::parse_agent_document(raw).is_err(),
            "case '{label}' unexpectedly succeeded"
        );
    }
}

#[test]
fn discovery_respects_precedence_and_skips_malformed_files() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    let user_agents = cfg_dir.join("agents");
    let extra_dir = dir.path().join("extra");
    let ws_root = dir.path().join("ws");
    let ws_agents = ws_root.join(".tars/agents");
    fs::create_dir_all(&user_agents).unwrap();
    fs::create_dir_all(&extra_dir).unwrap();
    fs::create_dir_all(&ws_agents).unwrap();

    // User overrides built-in `general` and defines `shared`, plus one malformed file.
    fs::write(
        user_agents.join("general.md"),
        "---\nname: general\ndescription: User general\n---\nUser prompt\n",
    )
    .unwrap();
    fs::write(
        user_agents.join("shared.md"),
        "---\nname: shared\ndescription: From user\n---\n",
    )
    .unwrap();
    fs::write(
        user_agents.join("broken.md"),
        "---\nname: broken\ndescription: bad\nunknown_field: 1\n---\n",
    )
    .unwrap();

    // Configured dir overrides `shared`.
    fs::write(
        extra_dir.join("shared.md"),
        "---\nname: shared\ndescription: From configured dir\n---\n",
    )
    .unwrap();

    // Workspace defines `shared` and `ws-only`.
    fs::write(
        ws_agents.join("shared.md"),
        "---\nname: shared\ndescription: From workspace\n---\n",
    )
    .unwrap();
    fs::write(
        ws_agents.join("ws-only.md"),
        "---\nname: ws-only\ndescription: Workspace agent\n---\n",
    )
    .unwrap();
    // Non-md file ignored.
    fs::write(ws_agents.join("README.txt"), "ignored").unwrap();

    // 1. Default user dir discovery (when agent_dirs is empty)
    let user_only = AgentDiscovery::discover_non_workspace(Some(&cfg_dir), &[]);
    assert_eq!(user_only.diagnostics.len(), 1);
    assert_eq!(
        user_only.diagnostics[0].kind,
        AgentDiagnosticKind::InvalidFrontmatter
    );
    let gen_preset = user_only.registry.get("general").unwrap();
    assert_eq!(gen_preset.source, AgentPresetSource::User);
    assert_eq!(gen_preset.description, "User general");
    // Built-in `explore` remains active
    assert!(
        user_only
            .registry
            .get("explore")
            .unwrap()
            .is_builtin_readonly()
    );

    // 2. Explicit agent_dirs + trusted workspace presets (highest precedence)
    let ws_snapshot = scan_workspace_agents(&ws_root);
    assert!(ws_snapshot.diagnostics.is_empty());
    assert_eq!(ws_snapshot.presets.len(), 2);
    assert!(ws_snapshot.manifest_text.contains("shared.md:"));
    assert!(ws_snapshot.manifest_text.contains("ws-only.md:"));

    let full = AgentDiscovery::discover(
        Some(&cfg_dir),
        &[user_agents.clone(), extra_dir.clone()],
        &ws_snapshot.presets,
    );
    assert_eq!(
        full.registry.get("shared").unwrap().description,
        "From workspace"
    );
    assert_eq!(
        full.registry.get("shared").unwrap().source,
        AgentPresetSource::Workspace
    );
    assert!(full.registry.contains("ws-only"));

    // Without trusted workspace presets, `ws-only` is absent and `shared` comes from `extra_dir`.
    let untrusted = AgentDiscovery::discover(Some(&cfg_dir), &[user_agents, extra_dir], &[]);
    assert!(!untrusted.registry.contains("ws-only"));
    assert_eq!(
        untrusted.registry.get("shared").unwrap().description,
        "From configured dir"
    );
    assert_eq!(
        untrusted.registry.get("shared").unwrap().source,
        AgentPresetSource::ConfiguredDir
    );
}

#[cfg(unix)]
#[test]
fn discovery_rejects_symlink_escapes_and_non_utf8_files() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_agents = ws.join(".tars/agents");
    fs::create_dir_all(&ws_agents).unwrap();

    let outside = dir.path().join("outside.md");
    fs::write(&outside, "---\nname: evil\ndescription: escaped\n---\n").unwrap();
    std::os::unix::fs::symlink(&outside, ws_agents.join("evil.md")).unwrap();
    fs::write(ws_agents.join("binary.md"), [0xff, 0xfe, 0x00]).unwrap();
    fs::write(
        ws_agents.join("good.md"),
        "---\nname: good\ndescription: valid\n---\n",
    )
    .unwrap();

    let snap = scan_workspace_agents(&ws);
    assert_eq!(snap.presets.len(), 1);
    assert_eq!(snap.presets[0].name, "good");
    assert!(
        snap.diagnostics
            .iter()
            .any(|d| d.kind == AgentDiagnosticKind::SymlinkEscape)
    );
    assert!(
        snap.diagnostics
            .iter()
            .any(|d| d.kind == AgentDiagnosticKind::InvalidUtf8)
    );
}

#[test]
fn resolve_model_override_handles_profiles_protocols_and_rejections() {
    let parent_root = if cfg!(windows) {
        PathBuf::from(r"C:\ws")
    } else {
        PathBuf::from("/ws")
    };
    let parent = sample_parent_definition(&parent_root);

    let mut profiles = BTreeMap::new();
    profiles.insert(
        "claude".to_owned(),
        ModelProfile {
            name: "claude".into(),
            provider: Provider::Anthropic,
            model: "claude-sonnet-4".into(),
            endpoint: Some("https://anthropic.example.com/v1/messages".into()),
            credential: CredentialRef::Environment("TARS_ANTHROPIC_KEY".into()),
            max_tokens: Some(4096),
            reasoning_effort: None,
            temperature: Some(Temperature::new(0.3).unwrap()),
            extra_body: Default::default(),
            context_window: Some(200_000),
            max_model_attempts: Some(4),
            render_options: Default::default(),
        },
    );
    profiles.insert(
        "claude-no-max".to_owned(),
        ModelProfile {
            name: "claude-no-max".into(),
            provider: Provider::Anthropic,
            model: "claude-sonnet-4".into(),
            endpoint: None,
            credential: CredentialRef::Environment("TARS_ANTHROPIC_KEY".into()),
            max_tokens: None,
            reasoning_effort: None,
            temperature: None,
            extra_body: Default::default(),
            context_window: None,
            max_model_attempts: None,
            render_options: Default::default(),
        },
    );

    // 1. None/None returns Ok(None)
    assert_eq!(
        resolve_model_override(&parent, None, None, &profiles).unwrap(),
        None
    );

    // 2. Unknown profile rejected
    let unknown = ModelConfigInput {
        profile: Some("missing".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_model_override(&parent, None, Some(&unknown), &profiles),
        Err(ModelResolutionError::UnknownProfile("missing".into()))
    );

    // 3. Anthropic temperature > 1.0 rejected
    let bad_anthropic_temp = ModelConfigInput {
        profile: Some("claude".into()),
        temperature: Some(Temperature::new(1.5).unwrap()),
        ..Default::default()
    };
    assert_eq!(
        resolve_model_override(&parent, None, Some(&bad_anthropic_temp), &profiles),
        Err(ModelResolutionError::InvalidTemperature)
    );

    // 4. Anthropic missing max_tokens rejected
    let missing_max = ModelConfigInput {
        profile: Some("claude-no-max".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_model_override(&parent, None, Some(&missing_max), &profiles),
        Err(ModelResolutionError::AnthropicMaxTokensRequired)
    );

    // 5. Valid cross-protocol profile switch + call override clears stale render_options & preserves tools
    let preset_cfg = AgentModelConfig {
        profile: Some("claude".into()),
        id: None,
        reasoning_effort: Some("high".into()),
        temperature: Some(Temperature::new(0.8).unwrap()),
        max_tokens: None,
    };
    let call_cfg = ModelConfigInput {
        profile: None,
        id: Some("claude-opus-4".into()),
        reasoning_effort: None,
        temperature: Some(Temperature::new(0.5).unwrap()),
        max_tokens: Some(1024),
    };
    let resolved = resolve_model_override(&parent, Some(&preset_cfg), Some(&call_cfg), &profiles)
        .unwrap()
        .expect("override");

    let narrowed = narrow_child_definition(
        &parent,
        &ChildNarrowingRequest {
            model_override: Some(resolved),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(narrowed.provider, Provider::Anthropic);
    assert_eq!(narrowed.model, "claude-opus-4");
    assert_eq!(
        narrowed.credential,
        CredentialRef::Environment("TARS_ANTHROPIC_KEY".into())
    );
    assert_eq!(narrowed.max_tokens, Some(1024));
    assert_eq!(narrowed.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(narrowed.temperature, Some(Temperature::new(0.5).unwrap()));
    // Stale OpenAiChat render_options (`include_usage`) were cleared!
    assert!(narrowed.render_options.is_empty());
    // Tools, workspace, and permission mode were NEVER widened!
    assert_eq!(narrowed.tools, parent.tools);
    assert_eq!(narrowed.workspace_root, parent.workspace_root);
    assert_eq!(narrowed.permission_mode, parent.permission_mode);

    // 6. Switching `call.model.profile` ignores stale preset.model.{id, effort, temperature, max_tokens}
    profiles.insert(
        "fast-openai".to_owned(),
        ModelProfile {
            name: "fast-openai".into(),
            provider: Provider::OpenAiChat,
            model: "gpt-4o-mini".into(),
            endpoint: Some("https://openai.example.com/v1/chat/completions".into()),
            credential: CredentialRef::Environment("OPENAI_KEY".into()),
            max_tokens: Some(512),
            reasoning_effort: Some("low".into()),
            temperature: Some(Temperature::new(0.2).unwrap()),
            extra_body: Default::default(),
            context_window: Some(64_000),
            max_model_attempts: Some(2),
            render_options: Default::default(),
        },
    );
    let stale_preset = AgentModelConfig {
        profile: Some("fast-openai".into()),
        id: Some("stale-preset-model-id".into()),
        reasoning_effort: Some("minimal".into()),
        temperature: Some(Temperature::new(0.9).unwrap()),
        max_tokens: Some(999),
    };
    let switched_call = ModelConfigInput {
        profile: Some("claude".into()),
        id: None,
        reasoning_effort: Some("medium".into()),
        temperature: None,
        max_tokens: None,
    };
    let switched = resolve_model_override(
        &parent,
        Some(&stale_preset),
        Some(&switched_call),
        &profiles,
    )
    .unwrap()
    .expect("switched override");
    assert_eq!(switched.provider, Some(Provider::Anthropic));
    assert_eq!(switched.model.as_deref(), Some("claude-sonnet-4"));
    assert_eq!(
        switched.temperature,
        Some(Some(Temperature::new(0.3).unwrap()))
    );
    assert_eq!(switched.max_tokens, Some(Some(4096)));
    assert_eq!(switched.reasoning_effort, Some(Some("medium".to_owned())));
}
